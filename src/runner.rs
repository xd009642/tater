use crate::ci;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{copy, create_dir_all, read_dir, remove_dir_all, remove_file, File};
use std::io::prelude::*;
use std::io::{self, BufReader, BufWriter};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::Duration;
use sysinfo::{Pid, ProcessExt, System, SystemExt};
use thiserror::Error;
use tracing::{error, info, instrument, warn};
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
    #[error("Setup script exited with {0}")]
    SetupFailed(ExitStatus),
    #[error("Failed to run tarpaulin: {0}")]
    Tarpaulin(String),
    #[error("Tarpaulin seems to have stalled")]
    Stalled,
    #[error("Tarpaulin exited with {0}")]
    Failed(ExitStatus),
    #[error("Failed to write run output: {0}")]
    Output(io::Error),
    #[error("Output writer thread panicked")]
    OutputThread,
    #[error("Failed to run teardown script: {0}")]
    Teardown(io::Error),
    #[error("Teardown script exited with {0}")]
    TeardownFailed(ExitStatus),
}

fn run_script(script: &str, project: &Path, log: &Path) -> io::Result<ExitStatus> {
    let output = File::create(log)?;
    let errors = output.try_clone()?;
    Command::new("sh")
        .args(&["-c", script])
        .current_dir(project)
        .stdout(Stdio::from(output))
        .stderr(Stdio::from(errors))
        .status()
}

fn stream_output<R: Read>(reader: R, output: File) -> io::Result<()> {
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(output);
    io::copy(&mut reader, &mut writer)?;
    writer.flush()
}

fn belongs_to_process_tree(system: &System, mut pid: Pid, root: Pid) -> bool {
    while let Some(process) = system.process(pid) {
        if pid == root {
            return true;
        }
        match process.parent() {
            Some(parent) if parent != pid => pid = parent,
            _ => return false,
        }
    }
    false
}

#[cfg(unix)]
fn kill_process_tree(child: &mut Child) -> io::Result<()> {
    use nix::sys::signal::{killpg, Signal};
    use nix::unistd::Pid as NixPid;

    let kill_result = killpg(NixPid::from_raw(child.id() as i32), Signal::SIGKILL)
        .map_err(|error| io::Error::new(io::ErrorKind::Other, error));
    let wait_result = child.wait().map(|_| ());
    kill_result.and(wait_result)
}

#[cfg(not(unix))]
fn kill_process_tree(child: &mut Child) -> io::Result<()> {
    child.kill()?;
    child.wait().map(|_| ())
}

fn wait_for_tarpaulin(child: &mut Child) -> Result<ExitStatus, RunError> {
    let mut system = System::new();
    let root = child.id() as Pid;
    let mut idle_samples = 0;

    // CPU usage is calculated from the difference between refreshes.
    system.refresh_processes();
    loop {
        thread::sleep(Duration::from_secs(1));
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {
                system.refresh_processes();
                let cpu_usage: f32 = system
                    .processes()
                    .iter()
                    .filter(|(pid, _)| belongs_to_process_tree(&system, **pid, root))
                    .map(|(_, process)| process.cpu_usage())
                    .sum();
                if cpu_usage < 0.1 {
                    idle_samples += 1;
                } else {
                    idle_samples = 0;
                }

                if idle_samples >= 60 {
                    error!("Stalled, killing process group");
                    kill_process_tree(child).map_err(|error| {
                        RunError::Tarpaulin(format!(
                            "Failed to kill stalled process group: {}",
                            error
                        ))
                    })?;
                    return Err(RunError::Stalled);
                }
            }
            Err(error) => {
                let cleanup_error = kill_process_tree(child).err();
                let message = match cleanup_error {
                    Some(cleanup) => format!(
                        "Failed to wait on tarpaulin: {}; process cleanup also failed: {}",
                        error, cleanup
                    ),
                    None => format!("Failed to wait on tarpaulin: {}", error),
                };
                return Err(RunError::Tarpaulin(message));
            }
        }
    }
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

#[instrument(skip(i, context, proj, jobs, projects, results), fields(project=%proj.repository_url))]
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

    let proj_res = results.join(proj_name);
    create_dir_all(&proj_res).map_err(RunError::Output)?;
    let _guard = ProjectCleanupGuard(&proj_dir);

    let setup_result = match proj.setup.as_ref() {
        Some(setup) => match run_script(setup, &proj_dir, &proj_res.join("setup.log")) {
            Ok(status) if status.success() => Ok(()),
            Ok(status) => Err(RunError::SetupFailed(status)),
            Err(error) => Err(RunError::Setup(error)),
        },
        None => Ok(()),
    };

    let tarpaulin_result = if setup_result.is_ok() {
        (|| -> Result<(), RunError> {
            let stdout_file =
                File::create(proj_res.join("stdout.log")).map_err(RunError::Output)?;
            let stderr_file =
                File::create(proj_res.join("stderr.log")).map_err(RunError::Output)?;
            match ci::spawn_tarpaulin(&proj_dir, jobs, context, proj) {
                Ok(mut tarp) => {
                    let stdout = tarp
                        .stdout
                        .take()
                        .expect("tarpaulin command must pipe stdout");
                    let stderr = tarp
                        .stderr
                        .take()
                        .expect("tarpaulin command must pipe stderr");
                    let stdout_reading = thread::spawn(move || stream_output(stdout, stdout_file));
                    let stderr_reading = thread::spawn(move || stream_output(stderr, stderr_file));

                    let wait_result = wait_for_tarpaulin(&mut tarp);
                    let stdout_result =
                        stdout_reading.join().map_err(|_| RunError::OutputThread)?;
                    let stderr_result =
                        stderr_reading.join().map_err(|_| RunError::OutputThread)?;
                    stdout_result.map_err(RunError::Output)?;
                    stderr_result.map_err(RunError::Output)?;

                    wait_result.and_then(|status| {
                        if status.success() {
                            Ok(())
                        } else {
                            Err(RunError::Failed(status))
                        }
                    })
                }
                Err(error) => Err(RunError::Tarpaulin(format!(
                    "Unable to spawn process: {}",
                    error
                ))),
            }
        })()
    } else {
        setup_result
    };

    let teardown_result = match proj.teardown.as_ref() {
        Some(teardown) => match run_script(teardown, &proj_dir, &proj_res.join("teardown.log")) {
            Ok(status) if status.success() => Ok(()),
            Ok(status) => Err(RunError::TeardownFailed(status)),
            Err(error) => Err(RunError::Teardown(error)),
        },
        None => Ok(()),
    };

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
    match (tarpaulin_result, teardown_result) {
        (Ok(()), teardown) => teardown,
        (Err(primary), Ok(())) => Err(primary),
        (Err(primary), Err(teardown)) => {
            error!("Teardown also failed for {}: {}", proj_name, teardown);
            Err(primary)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::process::CommandExt;

    /// Script output is retained even when the script reports a failure.
    #[test]
    fn script_failure_preserves_output() {
        let directory =
            std::env::temp_dir().join(format!("tater-script-test-{}", std::process::id()));
        fs::create_dir_all(&directory).expect("test directory should be created");
        let log = directory.join("script.log");

        let status = run_script("printf 'diagnostic'; exit 7", &directory, &log)
            .expect("script should be executed");

        assert_eq!(status.code(), Some(7));
        assert_eq!(
            fs::read_to_string(&log).expect("script log should be readable"),
            "diagnostic"
        );
        fs::remove_dir_all(&directory).expect("test directory should be removed");
    }

    /// Killing a stalled run also reaps its cargo process instead of leaving a zombie.
    #[cfg(unix)]
    #[test]
    fn process_tree_termination_reaps_child() {
        let mut command = Command::new("sh");
        command
            .args(&["-c", "sleep 30 & wait"])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command.process_group(0);
        let mut child = command.spawn().expect("test process should start");

        kill_process_tree(&mut child).expect("test process group should be terminated");

        assert!(child
            .try_wait()
            .expect("terminated child status should be available")
            .is_some());
    }
}
