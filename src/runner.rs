use crate::ci;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{
    copy, create_dir_all, read_dir, remove_dir_all, remove_file, symlink_metadata, File,
};
use std::io::prelude::*;
use std::io::{self, BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::Duration;
use sysinfo::{Pid, ProcessExt, System, SystemExt};
use thiserror::Error;
use tracing::{error, info, instrument, warn};
use url::Url;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

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

#[derive(Debug, Clone, Copy)]
pub struct RunOptions {
    pub jobs: Option<usize>,
    pub project_jobs: usize,
    pub retain_failed: bool,
    pub disk_budget: Option<u64>,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct CrateSpec {
    #[serde(with = "url_serde")]
    pub repository_url: Url,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Per-invocation toolchain, used by collected commands such as `cargo +nightly tarpaulin`.
    #[serde(default)]
    pub toolchain: Option<String>,
    /// For anything that requires something like another server to be up and running
    /// This is going to be executed like `sh -c CrateSpec::setup` so not great but :shrug:
    #[serde(default)]
    pub setup: Option<String>,
    /// To tear down any addition things that need running.
    #[serde(default)]
    pub teardown: Option<String>,
    /// Metadata present when this entry came from the CI command collector.
    #[serde(default)]
    pub ci: Option<CiInvocation>,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct CiInvocation {
    pub command: Vec<String>,
    pub command_file: String,
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
    #[error("Failed to remove checkout: {0}")]
    Cleanup(io::Error),
    #[error("Disk budget exceeded: {used} of {budget} bytes used")]
    DiskBudgetExceeded { used: u64, budget: u64 },
}

#[derive(Debug, Eq, PartialEq)]
pub enum RunOutcome {
    Passed,
    Skipped(String),
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

pub fn directory_size(root: &Path) -> io::Result<u64> {
    let mut pending = vec![root.to_path_buf()];
    let mut total = 0_u64;
    while let Some(path) = pending.pop() {
        let metadata = match symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            total = total.saturating_add(metadata.blocks().saturating_mul(512));
        }
        #[cfg(not(unix))]
        {
            total = total.saturating_add(metadata.len());
        }
        if metadata.is_dir() {
            match read_dir(path) {
                Ok(entries) => {
                    for entry in entries {
                        pending.push(entry?.path());
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(total)
}

fn archive_project(project: &Path, archive_path: &Path) -> io::Result<()> {
    let temporary = archive_path.with_extension("zip.tmp");
    let result = (|| {
        let output = File::create(&temporary)?;
        let mut archive = ZipWriter::new(output).set_auto_large_file();
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        let mut pending = read_dir(project)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<io::Result<Vec<PathBuf>>>()?;

        while let Some(path) = pending.pop() {
            let relative = path
                .strip_prefix(project)
                .expect("archived paths must belong to the project");
            let metadata = std::fs::symlink_metadata(&path)?;
            if metadata.is_dir() {
                archive
                    .add_directory_from_path(relative, options)
                    .map_err(io::Error::other)?;
                pending.extend(
                    read_dir(path)?
                        .map(|entry| entry.map(|entry| entry.path()))
                        .collect::<io::Result<Vec<PathBuf>>>()?,
                );
            } else if metadata.file_type().is_symlink() {
                archive
                    .add_symlink_from_path(relative, std::fs::read_link(&path)?, options)
                    .map_err(io::Error::other)?;
            } else if metadata.is_file() {
                archive
                    .start_file_from_path(relative, options)
                    .map_err(io::Error::other)?;
                let mut input = BufReader::new(File::open(path)?);
                io::copy(&mut input, &mut archive)?;
            }
        }
        let output = archive.finish().map_err(io::Error::other)?;
        output.sync_all()?;
        std::fs::rename(&temporary, archive_path)
    })();
    if result.is_err() {
        let _ = remove_file(temporary);
    }
    result
}

fn clean_project_checkout(project: &Path, archive_path: &Path, retain: bool) -> io::Result<()> {
    match remove_dir_all(project.join("target")) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    if retain {
        archive_project(project, archive_path)?;
    }
    remove_dir_all(project)
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

    let kill_result =
        killpg(NixPid::from_raw(child.id() as i32), Signal::SIGKILL).map_err(io::Error::other);
    let wait_result = child.wait().map(|_| ());
    kill_result.and(wait_result)
}

#[cfg(not(unix))]
fn kill_process_tree(child: &mut Child) -> io::Result<()> {
    child.kill()?;
    child.wait().map(|_| ())
}

fn wait_for_tarpaulin(
    child: &mut Child,
    output: &Path,
    disk_budget: Option<u64>,
) -> Result<ExitStatus, RunError> {
    let mut system = System::new();
    let root = child.id() as Pid;
    let mut idle_samples = 0;
    let mut samples = 0;

    // CPU usage is calculated from the difference between refreshes.
    system.refresh_processes();
    loop {
        thread::sleep(Duration::from_secs(1));
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {
                samples += 1;
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
                if samples % 2 == 0 {
                    if let Some(budget) = disk_budget {
                        let used = match directory_size(output) {
                            Ok(used) => used,
                            Err(error) => {
                                let cleanup_error = kill_process_tree(child).err();
                                if let Some(cleanup) = cleanup_error {
                                    error!("Process cleanup also failed: {}", cleanup);
                                }
                                return Err(RunError::Output(error));
                            }
                        };
                        if used > budget {
                            error!("Disk budget exceeded, killing process group");
                            kill_process_tree(child).map_err(|error| {
                                RunError::Tarpaulin(format!(
                                    "Failed to kill over-budget process group: {}",
                                    error
                                ))
                            })?;
                            return Err(RunError::DiskBudgetExceeded { used, budget });
                        }
                    }
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
        self.repository_url
            .path_segments()?
            .rfind(|segment| !segment.is_empty())
    }

    pub fn project_id(&self) -> String {
        let mut readable = self
            .repository_url
            .host_str()
            .into_iter()
            .chain(
                self.repository_url
                    .path_segments()
                    .into_iter()
                    .flatten()
                    .filter(|segment| !segment.is_empty()),
            )
            .flat_map(|part| part.chars().chain(std::iter::once('-')))
            .map(|character| {
                if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                    character
                } else {
                    '_'
                }
            })
            .take(96)
            .collect::<String>();
        readable.truncate(readable.trim_end_matches('-').len());

        // FNV-1a keeps IDs stable across program and Rust releases, unlike DefaultHasher.
        let hash = self
            .repository_url
            .as_str()
            .bytes()
            .chain(self.ci.iter().flat_map(|ci| {
                ci.command_file
                    .bytes()
                    .chain(ci.command.iter().flat_map(|arg| arg.bytes()))
            }))
            .fold(0xcbf29ce484222325_u64, |hash, byte| {
                (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
            });
        if readable.is_empty() {
            format!("repository-{:016x}", hash)
        } else {
            format!("{}-{:016x}", readable, hash)
        }
    }
}

fn clone_project(
    projects: impl AsRef<Path>,
    repository_url: &str,
    proj_name: &str,
) -> Result<(), String> {
    let projects = projects.as_ref();
    let git_hnd = Command::new("git")
        .args(&[
            "clone",
            "--recurse-submodules",
            "--shallow-submodules",
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
        let _ = remove_dir_all(projects.join(proj_name));
        Err(format!("Git clone of {} failed", repository_url))
    } else {
        info!("{} cloned successfully", proj_name);
        Ok(())
    }
}

#[instrument(skip(i, context, proj, projects, results, options), fields(project=%proj.repository_url))]
pub fn run_test(
    i: usize,
    context: &Context,
    proj: &CrateSpec,
    projects: &Path,
    results: &Path,
    options: &RunOptions,
) -> Result<RunOutcome, RunError> {
    let proj_name = proj.name().unwrap_or("unnamed_project");
    let project_id = proj.project_id();
    let proj_dir = projects.join(&project_id);
    let proj_res = results.join(&project_id);
    info!("{}. {}/{}", proj_name, i + 1, context.crates.len());
    create_dir_all(&proj_res).map_err(RunError::Output)?;
    if let Some(reason) = ci::compatibility::unsupported_reason(&proj_dir, proj) {
        info!("Skipping {}: {}", proj_name, reason);
        if proj_dir.exists() {
            clean_project_checkout(&proj_dir, &proj_res.join("checkout.zip"), false)
                .map_err(RunError::Cleanup)?;
        }
        return Ok(RunOutcome::Skipped(reason));
    }
    if proj_dir.join(".git").exists() {
        warn!("Project already cloned, using existing version");
    } else {
        clone_project(&projects, proj.repository_url.as_str(), &project_id)
            .map_err(|e| RunError::Git(e))?
    }

    let _guard = ProjectCleanupGuard(&proj_dir);
    if let Some(reason) = ci::compatibility::unsupported_reason(&proj_dir, proj) {
        info!("Skipping {}: {}", proj_name, reason);
        clean_project_checkout(&proj_dir, &proj_res.join("checkout.zip"), false)
            .map_err(RunError::Cleanup)?;
        return Ok(RunOutcome::Skipped(reason));
    }
    let output = projects
        .parent()
        .expect("projects directory must have an output parent");
    if let Some(budget) = options.disk_budget {
        let used = directory_size(output).map_err(RunError::Output)?;
        if used > budget {
            clean_project_checkout(&proj_dir, &proj_res.join("checkout.zip"), false)
                .map_err(RunError::Cleanup)?;
            return Err(RunError::DiskBudgetExceeded { used, budget });
        }
    }

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
            match ci::spawn_tarpaulin(&proj_dir, options.jobs.as_ref(), context, proj) {
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

                    let wait_result = wait_for_tarpaulin(&mut tarp, output, options.disk_budget);
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
    match read_dir(&proj_dir) {
        Ok(entries) => {
            for entry in entries {
                match entry {
                    Ok(entry) => {
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
                    Err(error) => warn!("Failed to inspect project output: {}", error),
                }
            }
        }
        Err(error) => warn!("Failed to inspect project for Tarpaulin logs: {}", error),
    }
    if !found_log {
        warn!("Haven't found tarpaulin log file");
    }
    let result = match (tarpaulin_result, teardown_result) {
        (Ok(()), teardown) => teardown,
        (Err(primary), Ok(())) => Err(primary),
        (Err(primary), Err(teardown)) => {
            error!("Teardown also failed for {}: {}", proj_name, teardown);
            Err(primary)
        }
    };

    let retain_checkout = result.is_err()
        && options.retain_failed
        && !matches!(&result, Err(RunError::DiskBudgetExceeded { .. }));
    let cleanup_result =
        clean_project_checkout(&proj_dir, &proj_res.join("checkout.zip"), retain_checkout);
    match (result, cleanup_result) {
        (Ok(()), Ok(())) => Ok(RunOutcome::Passed),
        (Ok(()), Err(cleanup)) => Err(RunError::Cleanup(cleanup)),
        (Err(primary), Ok(())) => Err(primary),
        (Err(primary), Err(cleanup)) => {
            error!(
                "Checkout cleanup also failed for {}: {}",
                proj_name, cleanup
            );
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

    /// Distinct repository URLs with the same basename receive distinct storage directories.
    #[test]
    fn project_id_disambiguates_matching_repository_names() {
        let first = CrateSpec {
            repository_url: Url::parse("https://example.com/first/parser")
                .expect("first repository URL should be valid"),
            args: Vec::new(),
            env: HashMap::new(),
            toolchain: None,
            setup: None,
            teardown: None,
            ci: None,
        };
        let second = CrateSpec {
            repository_url: Url::parse("https://example.com/second/parser")
                .expect("second repository URL should be valid"),
            args: Vec::new(),
            env: HashMap::new(),
            toolchain: None,
            setup: None,
            teardown: None,
            ci: None,
        };

        assert_ne!(first.project_id(), second.project_id());
    }

    /// Retained failures exclude build artifacts, produce a readable archive, and remove checkout.
    #[test]
    fn failed_checkout_is_archived_without_target_directory() {
        let directory =
            std::env::temp_dir().join(format!("tater-archive-test-{}", std::process::id()));
        let project = directory.join("project");
        let archive_path = directory.join("checkout.zip");
        fs::create_dir_all(project.join("src")).expect("source directory should be created");
        fs::create_dir_all(project.join("target")).expect("target directory should be created");
        fs::write(project.join("src/lib.rs"), "pub fn retained() {}")
            .expect("source file should be written");
        fs::write(project.join("target/artifact"), "large build output")
            .expect("target file should be written");

        clean_project_checkout(&project, &archive_path, true)
            .expect("failed checkout should be retained");

        assert!(!project.exists());
        let archive_file = File::open(&archive_path).expect("archive should be readable");
        let mut archive = zip::ZipArchive::new(archive_file).expect("archive should be valid");
        let mut source = String::new();
        archive
            .by_name("src/lib.rs")
            .expect("source should be retained")
            .read_to_string(&mut source)
            .expect("archived source should be readable");
        assert_eq!(source, "pub fn retained() {}");
        assert!(archive.by_name("target/artifact").is_err());
        fs::remove_dir_all(&directory).expect("test directory should be removed");
    }

    /// Unretained checkouts are removed without creating an archive.
    #[test]
    fn checkout_is_deleted_without_retention() {
        let directory =
            std::env::temp_dir().join(format!("tater-delete-test-{}", std::process::id()));
        let project = directory.join("project");
        let archive_path = directory.join("checkout.zip");
        fs::create_dir_all(&project).expect("project directory should be created");

        clean_project_checkout(&project, &archive_path, false).expect("checkout should be deleted");

        assert!(!project.exists());
        assert!(!archive_path.exists());
        fs::remove_dir_all(&directory).expect("test directory should be removed");
    }
}
