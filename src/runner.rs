use crate::ci;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{copy, create_dir, read_dir, remove_dir_all, remove_file, File};
use std::io::prelude::*;
use std::io::{self, BufWriter};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;
use sysinfo::{ProcessExt, System, SystemExt};
use thiserror::Error;
use tracing::{error, info, warn};
use url::Url;

#[derive(Debug, Default, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct Context {
    pub toolchain: String,
    pub target: Option<String>,
    pub crates: Vec<CrateSpec>,
    /// Args to be passed to every tarpaulin evocation
    #[serde(default)]
    pub args: Vec<String>,
    /// Env vars for every tarpaulin evocation
    #[serde(default)]
    pub env: HashMap<String, String>,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct CrateSpec {
    #[serde(with = "url_serde")]
    pub repository_url: Url,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// For anything that requires something like another server to be up and running
    /// This is going to be executed like `sh -c CrateSpec::setup` so not great but :shrug:
    #[serde(default)]
    pub setup: Option<String>,
    /// To tear down any addition things that need running.
    #[serde(default)]
    pub teardown: Option<String>,
}

#[derive(Error, Debug)]
pub enum RunError {
    #[error("Issue cloning repo: {0}")]
    Git(String),
    #[error("Failed to run setup script: {0}")]
    Setup(io::Error),
    #[error("Failed to run tarpaulin: {0}")]
    Tarpaulin(String),
    #[error("Tarpaulin seems to have stalled")]
    Stalled,
    #[error("Tarpaulin exited with a failure")]
    Failed,
}

/// This is to make it easier to clean up the project after exiting from running the test with an
/// error
struct ProjectCleanupGuard<'a>(&'a Path);

impl<'a> Drop for ProjectCleanupGuard<'a> {
    fn drop(&mut self) {
        let _ = remove_dir_all(self.0.join("target"));
    }
}

impl CrateSpec {
    pub fn name(&self) -> Option<&str> {
        self.repository_url.path().split('/').next_back()
    }
}

fn clone_project(
    projects: impl AsRef<Path>,
    repository_url: &str,
    proj_name: &str,
) -> Result<(), String> {
    let git_hnd = Command::new("git")
        .args(&[
            "clone",
            "--recurse-submodules",
            "--depth",
            "1",
            repository_url,
            proj_name,
        ])
        .current_dir(projects)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn git {}", e))?;

    let git = git_hnd
        .wait_with_output()
        .map_err(|e| format!("Git may not be installed: {}", e))?;

    if !git.status.success() {
        Err(format!("Git clone of {} failed", repository_url))
    } else {
        info!("{} cloned successfully", proj_name);
        Ok(())
    }
}

pub fn run_test(
    i: usize,
    context: &Context,
    proj: &CrateSpec,
    jobs: Option<&usize>,
    projects: &Path,
    results: &Path,
) -> Result<(), RunError> {
    let proj_name = proj.name().unwrap_or_else(|| "unnamed_project");
    let proj_dir = projects.join(proj_name);
    info!("{}. {}/{}", proj_name, i + 1, context.crates.len());
    if proj_dir.join(".git").exists() {
        warn!("Project already cloned, using existing version");
    } else {
        clone_project(&projects, proj.repository_url.as_str(), proj_name)
            .map_err(|e| RunError::Git(e))?
    }

    let _guard = ProjectCleanupGuard(&proj_dir);

    if let Some(setup) = proj.setup.as_ref() {
        let res = Command::new("sh")
            .args(&["-c", setup])
            .current_dir(&proj_dir)
            .output();
        if let Err(res) = res {
            error!("setup failed for {}", proj_name);
            return Err(RunError::Setup(res));
        }
    }

    let mut tarp =
        ci::spawn_tarpaulin(&proj_dir, &context, &proj).expect("Unable to spawn process");

    let system = System::default();
    // I need to take the stdout and stderr and start writing them now instead...
    let mut stdout = tarp.stdout.take().unwrap();
    let mut stderr = tarp.stderr.take().unwrap();

    let stdout_reading = thread::spawn(move || {
        let mut output = vec![];
        let _ = stdout.read_to_end(&mut output);
        output
    });

    let stderr_reading = thread::spawn(move || {
        let mut output = vec![];
        let _ = stderr.read_to_end(&mut output);
        output
    });

    let mut time_doing_nothing = 0;
    let tarp = loop {
        // We know tarpaulin won't be immediately done so lets just sleep at the start of the loop
        thread::sleep(Duration::new(10, 0));
        match tarp.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                // Check the CPU level
                if let Some(proc) = system.process(tarp.id() as _) {
                    if proc.cpu_usage() < 0.1 {
                        time_doing_nothing += 1;
                    } else {
                        time_doing_nothing = 0;
                    }

                    // If we've sampled < 0.1% CPU utilisation for a minute we should just give up
                    if time_doing_nothing > 5 {
                        error!("Stalled, killing");
                        let _ = tarp.kill();
                        return Err(RunError::Stalled);
                    }
                }
            }
            Err(e) => {
                return Err(RunError::Tarpaulin(format!(
                    "Failed to wait on tarpaulin: {}",
                    e
                )))
            }
        };
    };

    if let Some(teardown) = proj.teardown.as_ref() {
        let res = Command::new("sh")
            .args(&["-c", teardown])
            .current_dir(&proj_dir)
            .output();
        if let Err(res) = res {
            warn!("teardown failed for {}: {}", proj_name, res);
        }
    }
    let _ = remove_dir_all(proj_dir.join("target"));
    let proj_res = results.join(proj_name);

    let stdout = stdout_reading.join().unwrap();
    let stderr = stderr_reading.join().unwrap();

    let _ = create_dir(&proj_res);
    let mut writer =
        BufWriter::new(File::create(proj_res.join(format!("{}.log", proj_name))).unwrap());
    writer.write_all(b"stdout:\n").unwrap();
    writer.write_all(&stdout).unwrap();
    writer.write_all(b"\n\nstderr:\n").unwrap();
    writer.write_all(&stderr).unwrap();

    let mut found_log = false;
    for entry in read_dir(&proj_dir).unwrap() {
        let entry = entry.unwrap();
        if let Some(name) = entry.path().file_name() {
            if name.to_string_lossy().starts_with("tarpaulin-run") {
                if copy(entry.path(), proj_res.join("tarpaulin-run.json")).is_ok() {
                    let _ = remove_file(entry.path());
                    found_log = true;
                    break;
                } else {
                    warn!("Failed to copy log, still in project directory");
                }
            }
        }
    }
    if !found_log {
        warn!("Haven't found tarpaulin log file");
    }
    if tarp.success() {
        Ok(())
    } else {
        Err(RunError::Failed)
    }
}
