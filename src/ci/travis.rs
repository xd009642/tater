use crate::ci::types::*;
use crate::ci::*;
use serde::Deserialize;
use std::fs;
use std::io;
use std::path::Path;
use std::process::{Child, Command};

#[derive(Debug, Deserialize)]
pub struct Workflow {
    language: Option<String>,
    #[serde(default)]
    script: Vec<String>,
    after_success: Option<SingleOrMultiString>,
}

pub fn get_command(
    root: impl AsRef<Path>,
    jobs: Option<&usize>,
    context: &Context,
    spec: &CrateSpec,
) -> io::Result<Child> {
    let workflow = root.as_ref().join(".travis.yml");
    if workflow.exists() {
        let workflow = fs::File::open(workflow)?;
        let workflow: Workflow = serde_yaml::from_reader(workflow)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

        let mut cmd = Command::new("cargo");
        init_command(root.as_ref(), jobs, context, spec, &mut cmd);
        if let Some(after_success) = workflow.after_success.as_ref() {
            for line in after_success.lines() {
                if try_to_populate_command(line, &mut cmd) {
                    return cmd.spawn();
                }
            }
        } else {
            for line in &workflow.script {
                if try_to_populate_command(line.as_str(), &mut cmd) {
                    return cmd.spawn();
                }
            }
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "Did find valid command to turn into tarpaulin run",
        ))
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "Didn't find valid .travis.yml",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_config() {
        let yaml = r#"
language: rust
sudo: required
dist: trusty
addons:
    apt:
        packages:
            - libssl-dev
            - gfortran
cache: cargo
rust:
  - stable
  - beta
  - nightly
matrix:
  allow_failures:
    - rust: nightly

before_install: 
  - curl https://blas-lapack-rs.github.io/travis/fortran.sh | bash

script:
- cargo clean
- cargo build
- cargo test

after_success: |
    if [[ "$TRAVIS_RUST_VERSION" == nightly ]]; then
        cargo tarpaulin --ciserver travis-ci --coveralls $TRAVIS_JOB_ID
    fi 
"#;
        let result: Workflow = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(result.after_success.unwrap().lines().count(), 3);
        assert_eq!(result.script.len(), 3);
    }
}
