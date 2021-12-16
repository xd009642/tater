use crate::ci::init_command;
use crate::runner::*;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;
use std::process::{Child, Command, Stdio};

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
        for step in &job.steps {
            if step.uses.starts_with("actions-rs/tarpaulin") {
                // Extract tarpaulin args and merge
            } else if step.uses.starts_with("actions-rs/grcov") {
                // Convert grcov args to tarpaulin
            } else {
                // TODO detect kcov, cargo-llvm-cov, llvm coverage, or last attempt cargo test
                // calls
            }
        }
    }
    todo!()
}
