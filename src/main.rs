use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::fs::{copy, create_dir, create_dir_all, read_dir, remove_dir_all, remove_file, File};
use std::io::prelude::*;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::process::Command;
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
    #[structopt(name = "jobs", short = "j", long = "jobs", default_value = "8")]
    jobs: usize,
}

#[derive(Debug, Default, Clone, Eq, PartialEq, Serialize, Deserialize)]
struct Context {
    toolchain: String,
    target: Option<String>,
    crates: Vec<CrateSpec>,
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
        run_tater(&context, &args.output);
    }
    Ok(())
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

fn run_tater(context: &Context, output: &Path) {
    info!("Processing {} projects", context.crates.len());
    let projects = output.join("projects");
    let results = output.join("results");
    if create_dir(&projects).is_err() {
        warn!("Projects directory already exists");
    }
    if create_dir(&results).is_err() {
        warn!("Results directory already exists");
    }

    for (i, proj) in context.crates.iter().enumerate() {
        // Clone project
        let proj_name = proj.name().unwrap_or_else(|| "unnamed_project");
        let proj_dir = projects.join(proj_name);
        info!("{}. {}/{}", proj_name, i + 1, context.crates.len());
        if proj_dir.join(".git").exists() {
            warn!("Project already cloned, using existing version");
        } else {
            let git = Command::new("git")
                .args(&[
                    "clone",
                    "--recurse-submodules",
                    "--depth",
                    "1",
                    proj.repository_url.as_str(),
                    proj_name,
                ])
                .current_dir(&projects)
                .output()
                .expect("Git doesn't seem to be installed");

            if !git.status.success() {
                error!("Git clone of {} failed", proj.repository_url);
                continue;
            }
        }
        let mut args = vec![
            "tarpaulin".to_string(),
            "--debug".to_string(),
            "--color".to_string(),
            "never".to_string(),
        ];
        args.extend_from_slice(&proj.args);
        let tarp = Command::new("cargo")
            .args(&args)
            .current_dir(&proj_dir)
            .env("RUST_LOG", "cargo_tarpaulin=info")
            .envs(&proj.env)
            .output()
            .expect("cargo-tarpaulin doesn't seem to be installed");

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
        if tarp.status.success() && found_log {
            info!("Removing {}", proj_dir.display());
            let _ = remove_dir_all(&proj_dir);
        } else {
            error!("Tarpaulin failed on {}", proj_name);
        }
    }
}
