#![allow(dead_code)]
use crate::ci::*;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;
use std::process::{Child, Command};
use tracing::info;

#[derive(Debug, Deserialize)]
pub struct Pipeline {
    image: Option<String>,
    /// List of jobs
    #[serde(default)]
    variables: HashMap<String, serde_yaml::Value>,
    #[serde(flatten)]
    stages: HashMap<serde_yaml::Value, Stage>,
    /// TODO this can contain file references to other gitlab ci yamls that are inherited from - it
    /// may be required for some projects to later load these files and interpret them to get the
    /// best coverage command
    #[serde(default)]
    include: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
pub struct Stage {
    #[serde(default)]
    script: Vec<String>,
}

pub fn get_command(
    root: impl AsRef<Path>,
    jobs: Option<&usize>,
    context: &Context,
    spec: &CrateSpec,
) -> io::Result<Child> {
    let workflow = root.as_ref().join(".gitlab-ci.yml");
    if workflow.exists() {
        let workflow = fs::File::open(workflow)?;
        let workflow: Pipeline = serde_yaml::from_reader(workflow)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

        let mut cmd = Command::new("cargo");
        init_command(root.as_ref(), jobs, context, spec, &mut cmd);
        for (k, stage) in &workflow.stages {
            info!("Scanning stage: {:?}", k);
            for line in &stage.script {
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
            "Didn't find valid gitlab-ci.yml",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_ci_config() {
        let config = r#"
image: "rust:latest"

test:cargo:
  script:
    - cargo test --features foo
"#;

        let result: Pipeline = serde_yaml::from_str(config).unwrap();
    }
}
