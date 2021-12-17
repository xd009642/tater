use crate::ci::init_command;
use crate::runner::*;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use tracing::warn;

/// The overall github actions workflow, look [here](https://docs.github.com/en/actions/learn-github-actions/workflow-syntax-for-github-actions) for
/// docs
#[derive(Debug, Deserialize)]
pub struct Workflow {
    /// List of jobs
    jobs: HashMap<String, Job>,
}

/// Specification for a job
#[derive(Debug, Deserialize)]
pub struct Job {
    name: String,
    #[serde(rename = "runs-on")]
    runs_on: String,
    steps: Vec<Step>,
}

#[derive(Debug, Deserialize)]
pub struct Step {
    #[serde(default)]
    name: String,
    #[serde(default)]
    uses: String,
    #[serde(default)]
    with: HashMap<String, serde_yaml::Value>,
}

fn find_job(file: &Path, name: &str) -> bool {
    file.file_name()
        .unwrap()
        .to_string_lossy()
        .to_lowercase()
        .contains(name)
}

pub fn get_command(
    root: impl AsRef<Path>,
    context: &Context,
    spec: &CrateSpec,
) -> io::Result<Child> {
    let workflows = root.as_ref().join(".github/workflows");
    let workflows: Vec<_> = fs::read_dir(&workflows)?
        .filter_map(|x| x.ok())
        .map(|x| x.path())
        .filter(|x| x.is_file())
        .collect();

    // First we look for one called coverage, then test, then ci. After that we go over all of them for
    // the first one containing `cargo test` or `cargo tarpaulin` usage
    let mut cmd = Command::new("cargo");
    init_command(&mut cmd);

    if let Some(coverage) = workflows.iter().find(|x| find_job(x, "coverage")) {
        read_workflow(coverage, &mut cmd)
    } else if let Some(coverage) = workflows.iter().find(|x| find_job(x, "test")) {
        read_workflow(coverage, &mut cmd)
    } else if let Some(coverage) = workflows.iter().find(|x| find_job(x, "ci")) {
        read_workflow(coverage, &mut cmd)
    } else if let Some(coverage) = workflows.iter().find(|x| find_job(x, "rust")) {
        read_workflow(coverage, &mut cmd)
    } else {
        // Dumb search
        todo!()
    }
}

fn read_workflow(workflow: &Path, cmd: &mut Command) -> io::Result<Child> {
    let workflow = fs::File::open(workflow)?;
    let workflow: Workflow = serde_yaml::from_reader(workflow)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

    for (name, job) in &workflow.jobs {
        if let Some(step) = job
            .steps
            .iter()
            .find(|x| x.uses.starts_with("actions-rs/tarpaulin"))
        {
            // Extract tarpaulin args and merge https://github.com/actions-rs/tarpaulin
            for (arg, val) in step
                .with
                .iter()
                .filter(|(_, v)| v.is_string())
                .map(|(k, v)| (k, v.as_str().unwrap()))
            {
                match arg.as_str() {
                    "run-types" => {
                        cmd.arg("--run-types");
                        cmd.args(val.split_whitespace());
                    }
                    "timeout" => {
                        cmd.arg("--timeout");
                        cmd.arg(val);
                    }
                    "out-type" => {
                        cmd.arg("--out");
                    }
                    "args" | "version" => {
                        let mut skip_next = false;
                        for arg in val.split_whitespace() {
                            if skip_next {
                                skip_next = false;
                                continue;
                            }
                            if arg == "--color" {
                                skip_next = true;
                                continue;
                            }
                            cmd.arg(arg);
                        }
                    }
                    e => warn!("Unexpected with field: {}", e),
                }
            }
            return cmd.spawn();
        } else if let Some(step) = job
            .steps
            .iter()
            .find(|x| x.uses.starts_with("actions-rs/cargo"))
        {
            // Convert grcov args to tarpaulin https://github.com/actions-rs/grcov
            if step.with.get("command").and_then(|x| x.as_str()) == Some("test") {}
        } else {
            for step in &job.steps {
                // TODO detect kcov, cargo-llvm-cov, llvm coverage, or last attempt cargo test
                // calls
            }
        }
    }
    todo!()
}