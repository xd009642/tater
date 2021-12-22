use crate::ci::*;
use crate::runner::*;
use lazy_static::lazy_static;
use regex::Regex;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;
use std::process::{Child, Command};
use tracing::{debug, info, warn};

/// The overall github actions workflow, look [here](https://docs.github.com/en/actions/learn-github-actions/workflow-syntax-for-github-actions) for
/// docs
#[derive(Debug, Deserialize)]
pub struct Workflow {
    /// List of jobs
    jobs: HashMap<String, Job>,
    #[serde(default)]
    defaults: Defaults,
    env: HashMap<String, serde_yaml::Value>,
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
    #[serde(default)]
    strategy: Strategy,
}

#[derive(Debug, Default, Deserialize)]
pub struct Strategy {
    #[serde(default)]
    matrix: Matrix,
}

#[derive(Debug, Default, Deserialize)]
pub struct Matrix {
    #[serde(flatten)]
    elements: HashMap<String, Vec<serde_yaml::Value>>,
    #[serde(default)]
    include: Vec<serde_yaml::Value>,
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

#[derive(Debug, PartialEq)]
struct MatrixValue {
    data: Vec<serde_yaml::Value>,
    preconditions: HashMap<String, serde_yaml::Value>,
}

impl Job {
    fn get_possible_matrix_values(&self, var: &str) -> Option<Vec<MatrixValue>> {
        // If it's not directly in the matrix elements then it will be defined by the include table
        // this is gonna be a bit wild

        let parts = var.split(".").collect::<Vec<&str>>();
        if parts.len() < 2 || parts[0] != "matrix" {
            None
        } else if self.strategy.matrix.elements.contains_key(parts[1]) {
            let data = self.strategy.matrix.elements[parts[1]].clone();
            let val = MatrixValue {
                data,
                preconditions: Default::default(),
            };
            Some(vec![val])
        } else if !self.strategy.matrix.include.is_empty() {
            let mut res = vec![];
            for map in &self.strategy.matrix.include {
                if let Some(mapping) = map.as_mapping() {
                    let mut current_val = None;
                    let mut preconditions = HashMap::new();
                    for (key, val) in mapping.iter() {
                        if key.as_str() == Some(parts[1]) {
                            if let Some(s) = val.as_sequence() {
                                current_val = Some(s.clone());
                            } else {
                                current_val = Some(vec![val.clone()]);
                            }
                        } else if let Some(s) = key.as_str() {
                            preconditions.insert(s.to_string(), val.clone());
                        } else {
                            warn!("Unexpected key type in GHA matrix: {:?}", key);
                        }
                    }
                    if let Some(data) = current_val {
                        res.push(MatrixValue {
                            data,
                            preconditions,
                        })
                    }
                }
            }
            Some(res)
        } else {
            None
        }
    }
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
    init_command(root.as_ref(), &mut cmd);

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
    debug!("Processing workflow: {}", workflow.display());
    lazy_static! {
        static ref GHA_VARIABLE: Regex = Regex::new(r#"${{\s*(?P<v>[:alpha:]+)\s*}}"#).unwrap();
    }
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
            info!("Spawning: {:?}", cmd);
            return cmd.spawn();
        } else if let Some(step) = job
            .steps
            .iter()
            .find(|x| x.uses.starts_with("actions-rs/cargo"))
        {
            // Convert grcov args to tarpaulin https://github.com/actions-rs/grcov
            if step.with.get("command").and_then(|x| x.as_str()) == Some("test") {
                if let Some(dir) = workflow.defaults.working_directory() {
                    info!("Working dir to {}", root.join(dir).display());
                    cmd.current_dir(root.join(dir));
                }
                if let Some(s) = step.with.get("args") {
                    if s.is_string() {
                        process_arg_string(cmd, s.as_str().unwrap());
                    }
                }
                info!("Spawning: {:?}", cmd);
                return cmd.spawn();
            }
        } else {
            for step in &job.steps {
                // TODO detect kcov, cargo-llvm-cov, llvm coverage, or last attempt cargo test
                // calls

                // TODO need to split up commands and handle things like `cd blah && cargo test;
                if step.run.contains("cargo test") {
                    info!("Maybe one: '{}'", step.run);
                    let commands = extract_tarpaulin_commands(&step.run);
                    info!("Found commands: {:?}", commands);
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
    fn openmls_yaml() {
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

    #[test]
    fn hyper_yaml() {
        let x = r#"
name: CI
on:
  pull_request:
  push:
    branches:
      - master

env:
  RUST_BACKTRACE: 1

jobs:
  test:
    name: Test ${{ matrix.rust }} on ${{ matrix.os }}
    needs: [style]
    strategy:
      matrix:
        rust:
          - stable
          - beta
          - nightly

        os:
          - ubuntu-latest
          - windows-latest
          - macOS-latest

        include:
          - rust: stable
            features: "--features full"
          - rust: beta
            features: "--features full"
          - rust: nightly
            features: "--features full,nightly"
            benches: true

    runs-on: ${{ matrix.os }}

    steps:
      - name: Checkout
        uses: actions/checkout@v1

      - name: Install Rust (${{ matrix.rust }})
        uses: actions-rs/toolchain@v1
        with:
          profile: minimal
          toolchain: ${{ matrix.rust }}
          override: true

      - name: Test
        uses: actions-rs/cargo@v1
        with:
          command: test
          args: ${{ matrix.features }}

      - name: Test all benches
        if: matrix.benches
        uses: actions-rs/cargo@v1
        with:
          command: test
          args: --benches ${{ matrix.features }}
        "#;

        let result: Workflow = serde_yaml::from_str(x).unwrap();
        let job = &result.jobs["test"];

        let rust_versions = job.get_possible_matrix_values("matrix.rust").unwrap();
        assert!(rust_versions.iter().all(|x| x.preconditions.is_empty()));
        assert!(rust_versions
            .iter()
            .all(|x| x.data.iter().all(|y| y.is_string())));
        assert_eq!(
            rust_versions[0]
                .data
                .iter()
                .filter_map(|x| x.as_str())
                .collect::<Vec<_>>(),
            vec!["stable", "beta", "nightly"]
        );

        let os_versions = job.get_possible_matrix_values("matrix.os").unwrap();
        assert!(os_versions.iter().all(|x| x.preconditions.is_empty()));
        assert!(os_versions
            .iter()
            .all(|x| x.data.iter().all(|y| y.is_string())));
        assert_eq!(
            os_versions[0]
                .data
                .iter()
                .filter_map(|x| x.as_str())
                .collect::<Vec<_>>(),
            vec!["ubuntu-latest", "windows-latest", "macOS-latest"]
        );

        let mut preconditions = HashMap::new();
        preconditions.insert(
            "rust".to_string(),
            serde_yaml::Value::String("stable".to_string()),
        );

        let features_stable = MatrixValue {
            data: vec![serde_yaml::Value::String("--features full".to_string())],
            preconditions,
        };

        let features = job.get_possible_matrix_values("matrix.features").unwrap();
        assert_eq!(features[0], features_stable);
        assert_eq!(features.len(), 3);

        assert_eq!(job.get_possible_matrix_values("maatrix.foo"), None);
        assert_eq!(job.get_possible_matrix_values("matrix.foo"), Some(vec![]));
        assert_eq!(job.get_possible_matrix_values("matrix"), None);
    }
}
