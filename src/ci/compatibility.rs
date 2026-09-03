use crate::runner::CrateSpec;
use serde_yaml::{Mapping, Value};
use std::fs::File;
use std::path::{Component, Path, PathBuf};

/// Returns a reason only when the collected CI invocation cannot run on this Linux host.
pub fn unsupported_reason(root: &Path, spec: &CrateSpec) -> Option<String> {
    let invocation = spec.ci.as_ref()?;
    if let Some(target) = command_target(&invocation.command) {
        if !target.contains("${{") && target_is_incompatible(target) {
            return Some(format!("CI command targets {}", target));
        }
    }

    let workflow = workflow_path(root, &invocation.command_file)?;
    let document: Value = match File::open(&workflow)
        .ok()
        .and_then(|file| serde_yaml::from_reader(file).ok())
    {
        Some(document) => document,
        None => return None,
    };
    let jobs = document
        .as_mapping()?
        .get(&Value::String("jobs".to_string()))?
        .as_mapping()?;
    let classifications = jobs
        .iter()
        .filter_map(|(_, value)| value.as_mapping())
        .filter(|job| job_contains_tarpaulin(job))
        .filter_map(|job| classify_job(job, &invocation.command))
        .collect::<Vec<_>>();

    if !classifications.is_empty() && classifications.iter().all(|supported| !supported) {
        Some(format!(
            "{} only runs Tarpaulin on a non-Linux or non-host target",
            workflow.display()
        ))
    } else {
        None
    }
}

fn workflow_path(root: &Path, command_file: &str) -> Option<PathBuf> {
    let url = url::Url::parse(command_file).ok()?;
    if url.host_str()? != "raw.githubusercontent.com" {
        return None;
    }
    let relative = url.path_segments()?.skip(3).collect::<PathBuf>();
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return None;
    }
    Some(root.join(relative))
}

fn job_contains_tarpaulin(job: &Mapping) -> bool {
    job.get(&Value::String("steps".to_string()))
        .and_then(Value::as_sequence)
        .into_iter()
        .flatten()
        .filter_map(Value::as_mapping)
        .any(|step| {
            ["run", "uses"]
                .iter()
                .filter_map(|key| step.get(&Value::String((*key).to_string())))
                .filter_map(Value::as_str)
                .any(|value| value.to_ascii_lowercase().contains("tarpaulin"))
        })
}

fn classify_job(job: &Mapping, command: &[String]) -> Option<bool> {
    let runner = job
        .get(&Value::String("runs-on".to_string()))
        .and_then(Value::as_str)?;
    let runners = resolve_matrix_value(job, runner).unwrap_or_else(|| vec![runner]);
    let runner_supported =
        classify_values(&runners, runner_is_linux_compatible, runner_is_incompatible)?;
    if !runner_supported {
        return Some(false);
    }

    let joined = command.join(" ");
    if let Some(variable) = matrix_reference(&joined, "target") {
        if let Some(targets) = resolve_matrix_key(job, variable) {
            return classify_values(&targets, target_is_linux_compatible, target_is_incompatible);
        }
    }
    Some(true)
}

fn classify_values(
    values: &[&str],
    compatible: fn(&str) -> bool,
    incompatible: fn(&str) -> bool,
) -> Option<bool> {
    if values.iter().any(|value| compatible(value)) {
        Some(true)
    } else if values.iter().all(|value| incompatible(value)) {
        Some(false)
    } else {
        None
    }
}

fn resolve_matrix_value<'a>(job: &'a Mapping, expression: &str) -> Option<Vec<&'a str>> {
    let variable = matrix_reference(expression, "")?;
    resolve_matrix_key(job, variable)
}

fn resolve_matrix_key<'a>(job: &'a Mapping, key: &str) -> Option<Vec<&'a str>> {
    job.get(&Value::String("strategy".to_string()))?
        .as_mapping()?
        .get(&Value::String("matrix".to_string()))?
        .as_mapping()?
        .get(&Value::String(key.to_string()))?
        .as_sequence()
        .map(|values| values.iter().filter_map(Value::as_str).collect())
}

fn matrix_reference<'a>(value: &'a str, expected_name: &str) -> Option<&'a str> {
    let start = value.to_ascii_lowercase().find("matrix.")? + "matrix.".len();
    let rest = &value[start..];
    let end = rest
        .find(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
        .unwrap_or(rest.len());
    let name = &rest[..end];
    if expected_name.is_empty() || name.eq_ignore_ascii_case(expected_name) {
        Some(name)
    } else {
        None
    }
}

fn command_target(command: &[String]) -> Option<&str> {
    command.iter().enumerate().find_map(|(index, argument)| {
        argument.strip_prefix("--target=").or_else(|| {
            (argument == "--target")
                .then(|| command.get(index + 1))
                .flatten()
                .map(String::as_str)
        })
    })
}

fn runner_is_linux_compatible(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    value.contains("ubuntu") || value == "linux" || value.contains("linux-")
}

fn target_is_linux_compatible(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    value.contains("linux") && value.starts_with(std::env::consts::ARCH)
}

fn runner_is_incompatible(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    value.contains("windows")
        || value.contains("macos")
        || value.contains("darwin")
        || value.contains("apple")
}

fn target_is_incompatible(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    runner_is_incompatible(&value)
        || value.starts_with("thumb")
        || value.starts_with("wasm")
        || value.starts_with("avr")
        || value.contains("-none")
        || value.contains("nvptx")
        || (!value.contains("linux") && value.matches('-').count() >= 2)
        || (value.contains("linux") && !value.starts_with(std::env::consts::ARCH))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::CiInvocation;
    use std::collections::HashMap;
    use url::Url;

    /// Targets that cannot execute natively on the current Linux host are rejected.
    #[test]
    fn classifies_explicit_targets() {
        assert!(target_is_incompatible("x86_64-pc-windows-msvc"));
        assert!(target_is_incompatible("thumbv7em-none-eabihf"));
        assert!(!target_is_incompatible(&format!(
            "{}-unknown-linux-gnu",
            std::env::consts::ARCH
        )));
    }

    /// A matrix with at least one Linux runner remains runnable.
    #[test]
    fn resolves_linux_runner_from_matrix() {
        let document: Value = serde_yaml::from_str(
            r#"
runs-on: ${{ matrix.os }}
strategy:
  matrix:
    os: [windows-latest, ubuntu-latest, macos-latest]
"#,
        )
        .expect("job YAML should parse");
        assert_eq!(
            classify_job(document.as_mapping().expect("job should be a map"), &[]),
            Some(true)
        );
    }

    /// The workflow associated with a collected command can rule out an OS-incompatible job.
    #[test]
    fn rejects_windows_only_collected_workflow() {
        let root =
            std::env::temp_dir().join(format!("tater-compatibility-test-{}", std::process::id()));
        let workflow = root.join(".github/workflows/ci.yml");
        std::fs::create_dir_all(workflow.parent().expect("workflow should have a parent"))
            .expect("workflow directory should be created");
        std::fs::write(
            &workflow,
            r#"
jobs:
  coverage:
    runs-on: windows-latest
    steps:
      - run: cargo tarpaulin
"#,
        )
        .expect("workflow should be written");
        let spec = CrateSpec {
            repository_url: Url::parse("https://github.com/example/project")
                .expect("repository URL should be valid"),
            args: Vec::new(),
            env: HashMap::new(),
            toolchain: None,
            setup: None,
            teardown: None,
            ci: Some(CiInvocation {
                command: vec!["cargo".to_string(), "tarpaulin".to_string()],
                command_file: "https://raw.githubusercontent.com/example/project/revision/.github/workflows/ci.yml".to_string(),
            }),
        };

        assert!(unsupported_reason(&root, &spec)
            .expect("Windows-only workflow should be rejected")
            .contains("non-Linux"));
        std::fs::remove_dir_all(root).expect("test directory should be removed");
    }
}
