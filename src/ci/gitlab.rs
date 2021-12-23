use crate::runner::*;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;
use std::process::{Child, Command, Stdio};

#[derive(Debug, Deserialize)]
pub struct Pipeline {
    image: Option<String>,
    /// List of jobs
    #[serde(default)]
    variables: HashMap<String, serde_yaml::Value>,
    #[serde(flatten)]
    stages: HashMap<serde_yaml::Value, Stage>,
}

#[derive(Debug, Deserialize)]
pub struct Stage {
    script: Vec<String>,
}

pub fn get_command(
    root: impl AsRef<Path>,
    context: &Context,
    spec: &CrateSpec,
) -> io::Result<Child> {
    let workflow = root.as_ref().join(".gitlab-ci.yml");
    if workflow.exists() {
        let workflow = fs::File::open(workflow)?;

        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "tater can't interpret gitlab yet",
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
