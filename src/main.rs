use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::fs::{
    copy, create_dir, create_dir_all, read_dir, remove_dir_all, remove_file, File, OpenOptions,
};
use std::io::prelude::*;
use std::io::{self, BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use structopt::StructOpt;
use tracing::{error, info, warn};
use tracing_subscriber::{EnvFilter, Layer, Registry};
use url::Url;

#[derive(Debug, Default, Clone, Eq, PartialEq, Hash, Ord, PartialOrd, StructOpt)]
struct Args {
    /// Location to the repos file
    #[structopt(
        name = "input repos",
        short = "i",
        long = "input",
        default_value = "repos.json"
    )]
    repos: PathBuf,
    /// Directory to add the projects and results folder
    #[structopt(
        name = "output folder",
        short = "o",
        long = "output",
        default_value = "./output"
    )]
    output: PathBuf,
    /// Limit the number of jobs, this will limit cargo build jobs and also the number of test
    /// threads TODO
    #[structopt(name = "jobs", short = "j", long = "jobs")]
    jobs: Option<usize>,
}

#[derive(Debug, Default, Clone, Eq, PartialEq, Serialize, Deserialize)]
struct Context {
    toolchain: String,
    target: Option<String>,
    crates: Vec<CrateSpec>,
    /// Args to be passed to every tarpaulin evocation
    #[serde(default)]
    args: Vec<String>,
    /// Env vars for every tarpaulin evocation
    #[serde(default)]
    env: HashMap<String, String>,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
struct CrateSpec {
    #[serde(with = "url_serde")]
    repository_url: Url,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
}

impl CrateSpec {
    fn name(&self) -> Option<&str> {
        self.repository_url.path().split('/').next_back()
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    setup_logging();
    let ctrlc_events = ctrl_handler()?;
    let args = Args::from_args();

    if !args.repos.is_file() {
        panic!("No repos file provided");
    }
    if args.output.is_file() {
        panic!("Output directory is a file");
    }
    if !args.output.is_dir() {
        info!("Creating output directory: {}", args.output.display());
        create_dir_all(&args.output).unwrap();
    }

    if let Ok(file) = File::open(args.repos) {
        let reader = BufReader::new(file);
        let context: Context = serde_json::from_reader(reader).expect("Unable to parse repos json");
        run_tater(&context, &args.output, args.jobs, ctrlc_events);
    }
    Ok(())
}

fn ctrl_handler() -> Result<mpsc::Receiver<()>, ctrlc::Error> {
    let (sender, receiver) = mpsc::channel();
    ctrlc::set_handler(move || {
        let _e = sender.send(());
    })?;
    Ok(receiver)
}

fn setup_logging() {
    let filter = match env::var("RUST_LOG") {
        Ok(_) => EnvFilter::from_default_env(),
        _ => EnvFilter::new("tater=info"),
    };
    let fmt = tracing_subscriber::fmt::Layer::default();
    let subscriber = filter.and_then(fmt).with_subscriber(Registry::default());
    tracing::subscriber::set_global_default(subscriber).unwrap();
}

/// Returns the next crate to process for resuming a workflow
fn get_progress(progress_file: &Path) -> std::io::Result<usize> {
    if progress_file.is_file() {
        let reader = BufReader::new(File::open(&progress_file)?);
        if let Some(line) = reader.lines().next() {
            let line = line?;
            match line.trim().parse::<usize>() {
                Ok(n) => Ok(n),
                Err(_) => {
                    warn!("Invalid progress file contents: {}", line);
                    Ok(0)
                }
            }
        } else {
            Ok(0)
        }
    } else {
        Ok(0)
    }
}

fn should_exit(progress_file: &Path, index: usize, rx: &mpsc::Receiver<()>) -> bool {
    if rx.try_recv().is_ok() {
        info!("Pausing execution");
        let progress_msg = "Unable to write progress file do it yourself";
        let mut f = File::create(&progress_file).expect(progress_msg);
        f.write_all(index.to_string().as_bytes())
            .expect(progress_msg);
        true
    } else {
        false
    }
}

fn get_status_linewriter(path: &Path, start_iter: usize) -> io::Result<BufWriter<File>> {
    let file = if start_iter == 0 {
        File::create(path)
    } else {
        OpenOptions::new().append(true).create(true).open(path)
    }?;
    Ok(BufWriter::new(file))
}

fn run_tater(context: &Context, output: &Path, jobs: Option<usize>, rx: mpsc::Receiver<()>) {
    info!("Processing {} projects", context.crates.len());
    let projects = output.join("projects");
    let results = output.join("results");
    let progress_file = output.join("progress");
    let pass_file = output.join("pass");
    let fail_file = output.join("fail");
    if create_dir(&projects).is_err() {
        warn!("Projects directory already exists");
    }
    if create_dir(&results).is_err() {
        warn!("Results directory already exists");
    }
    let start_from = match get_progress(&progress_file) {
        Ok(s) => s,
        Err(e) => {
            error!("Invalid progress file: {}", e);
            0
        }
    };
    if start_from > 0 {
        info!("Resuming execution from {}", start_from);
    }
    let mut fail_writer = get_status_linewriter(&fail_file, start_from).unwrap();
    let mut pass_writer = get_status_linewriter(&pass_file, start_from).unwrap();
    let mut failures = 0;
    for (i, proj) in context.crates.iter().enumerate().skip(start_from) {
        // Clone project
        let proj_name = proj.name().unwrap_or_else(|| "unnamed_project");
        let proj_dir = projects.join(proj_name);
        info!("{}. {}/{}", proj_name, i + 1, context.crates.len());
        if proj_dir.join(".git").exists() {
            warn!("Project already cloned, using existing version");
        } else {
            let git_hnd = Command::new("git")
                .args(&[
                    "clone",
                    "--recurse-submodules",
                    "--depth",
                    "1",
                    proj.repository_url.as_str(),
                    proj_name,
                ])
                .current_dir(&projects)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("Unable to spawn process");
            let git = git_hnd
                .wait_with_output()
                .expect("Git doesn't seem to be installed");
            if !git.status.success() {
                error!("Git clone of {} failed", proj.repository_url);
            } else {
                info!("{} cloned successfully", proj_name);
            }
        }
        if should_exit(&progress_file, i, &rx) {
            return;
        }
        if !proj_dir.exists() {
            continue;
        }
        let mut args = vec![
            "tarpaulin".to_string(),
            "--debug".to_string(),
            "--color".to_string(),
            "never".to_string(),
        ];
        args.extend_from_slice(&context.args);
        args.extend_from_slice(&proj.args);
        if let Some(nj) = jobs {
            args.extend_from_slice(&[
                "--jobs".to_string(),
                nj.to_string(),
                "--".to_string(),
                "--test-threads".to_string(),
                nj.to_string(),
            ]);
        }

        let tarp = Command::new("cargo")
            .args(&args)
            .current_dir(&proj_dir)
            .env("RUST_LOG", "cargo_tarpaulin=info")
            .envs(&proj.env)
            .envs(&context.env)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("Unable to spawn process");

        let tarp = tarp
            .wait_with_output()
            .expect("cargo tarpaulin doesn't seem to be installed");
        let proj_res = results.join(proj_name);

        let _ = create_dir(&proj_res);
        let mut writer =
            BufWriter::new(File::create(proj_res.join(format!("{}.log", proj_name))).unwrap());
        writer.write_all(b"stdout:\n").unwrap();
        writer.write_all(&tarp.stdout).unwrap();
        writer.write_all(b"\n\nstderr:\n").unwrap();
        writer.write_all(&tarp.stderr).unwrap();

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
        let exit_index = if tarp.status.success() && found_log {
            info!("Removing {}", proj_dir.display());
            let _ = remove_dir_all(&proj_dir);
            let _ = pass_writer.write_all(proj_name.as_bytes());
            let _ = pass_writer.write_all(b"\n");
            let _ = pass_writer.flush();
            i + 1
        } else {
            failures += 1;
            error!("Tarpaulin failed on {}", proj_name);
            i
        };
        if should_exit(&progress_file, exit_index, &rx) {
            let _ = fail_writer.write_all(proj_name.as_bytes());
            let _ = fail_writer.write_all(b"\n");
            let _ = fail_writer.flush();
            return;
        } else if i == exit_index {
            let _ = fail_writer.write_all(proj_name.as_bytes());
            let _ = fail_writer.write_all(b"\n");
            let _ = fail_writer.flush();
        }
    }
    if failures > 0 {
        error!(
            "Tarpaulin failed on {}/{} projects",
            failures,
            context.crates.len()
        );
    }
}
