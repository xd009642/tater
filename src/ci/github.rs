use crate::ci::init_command;
use crate::runner::*;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;
use std::process::{Child, Command};
use tracing::{info, warn};

/// The overall github actions workflow, look [here](https://docs.github.com/en/actions/learn-github-actions/workflow-syntax-for-github-actions) for
/// docs
#[derive(Debug, Deserialize)]
pub struct Workflow {
    /// List of jobs
    jobs: HashMap<String, Job>,
    #[serde(default)]
    defaults: Defaults,
}

#[derive(Debug, Default, Deserialize)]
pub struct Defaults {
    run: HashMap<String, serde_yaml::Value>,
}

impl Defaults {
    fn working_directory(&self) -> Option<&str> {
        self.run.get("working-directory").and_then(|x| x.as_str())
    }
}

/// Specification for a job
#[derive(Debug, Deserialize)]
pub struct Job {
    #[serde(default)]
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
    #[serde(default)]
    run: String,
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
        read_workflow(root.as_ref(), coverage, &mut cmd)
    } else if let Some(coverage) = workflows.iter().find(|x| find_job(x, "test")) {
        read_workflow(root.as_ref(), coverage, &mut cmd)
    } else if let Some(coverage) = workflows.iter().find(|x| find_job(x, "ci")) {
        read_workflow(root.as_ref(), coverage, &mut cmd)
    } else if let Some(coverage) = workflows.iter().find(|x| find_job(x, "rust")) {
        read_workflow(root.as_ref(), coverage, &mut cmd)
    } else {
        // Dumb search
        for coverage in &workflows {
            if let Ok(c) = read_workflow(root.as_ref(), coverage, &mut cmd) {
                return Ok(c);
            }
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "Didn't find valid github action",
        ))
    }
}

fn read_workflow(root: &Path, workflow: &Path, cmd: &mut Command) -> io::Result<Child> {
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
                        process_arg_string(cmd, &val);
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
            if step.with.get("command").and_then(|x| x.as_str()) == Some("test") {
                if let Some(dir) = workflow.defaults.working_directory() {
                    cmd.current_dir(root.join(dir));
                }
                if let Some(s) = step.with.get("args") {
                    if s.is_string() {
                        process_arg_string(cmd, s.as_str().unwrap());
                    }
                }
                return cmd.spawn();
            }
        } else {
            for step in &job.steps {
                // TODO detect kcov, cargo-llvm-cov, llvm coverage, or last attempt cargo test
                // calls
                if step.run.contains("cargo test") {
                    info!("Maybe one: '{}'", step.run);
                }
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "Didn't find a command to convert to tarpaulin",
    ))
}

fn process_arg_string(cmd: &mut Command, args: &str) {
    let mut skip_next = false;
    for arg in args.split_whitespace() {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_yamls() {
        let x = r#"
name: Tests & Checks

on:
  push:
    branches:
      - main
      - konrad/treesync
  pull_request:
    branches:
      - main
      - konrad/treesync
  workflow_dispatch:

env:
  CARGO_TERM_COLOR: always

defaults:
  run:
    working-directory: openmls

jobs:
  tests:
    strategy:
      fail-fast: false
      matrix:
        os:
          - macos-latest
          - ubuntu-latest
          - windows-latest
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v2
        with:
          ref: ${{ github.event.pull_request.head.sha }}
      - name: Tests debug build
        run: |
          cargo test --verbose
      - name: Tests release build
        run: |
          cargo test --verbose --release
        # Test 32 bit builds on windows
      - name: Install rust target
        if: matrix.os == 'windows-latest'
        uses: actions-rs/toolchain@v1
        with:
          toolchain: stable
          profile: minimal
          override: true
          target: i686-pc-windows-msvc
      - name: Tests 32bit windows release build
        if: matrix.os == 'windows-latest'
        run: |
          cargo test --verbose --target i686-pc-windows-msvc
          cargo test --verbose --release --target i686-pc-windows-msvc
            "#;

        let result: Workflow = serde_yaml::from_str(x).unwrap();
        assert_eq!(result.defaults.working_directory(), Some("openmls"));

        assert_eq!(result.jobs.get("tests").unwrap().steps.len(), 5);
    }
}
