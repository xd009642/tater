use crate::runner::*;
use lazy_static::lazy_static;
use regex::{Regex, RegexBuilder};
use std::io;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use tracing::{debug, info, warn};

pub mod github;
pub mod gitlab;
pub mod travis;

pub fn default_args() -> Vec<String> {
    vec![
        "tarpaulin".to_string(),
        "--debug".to_string(),
        "--color".to_string(),
        "never".to_string(),
    ]
}

pub fn try_to_populate_command(data: &str, cmd: &mut Command) -> bool {
    // TODO need to split up commands and handle things like `cd blah && cargo test;
    // Also, find tarpaulin ran via shell commands
    if data.contains("cargo test") {
        debug!("Maybe one: '{}'", data);
        let commands = extract_tarpaulin_commands(data);
        info!("Found commands: {:?}", commands);
        if commands.len() == 1 {
            cmd.args(commands[0].split_whitespace().skip(2));
        } else if commands.len() > 1 {
            // Should generate a tarpaulin.toml for these commands
            warn!("Ignoring commands: {:?}", &commands[1..]);
            cmd.args(commands[0].split_whitespace().skip(2));
        }
        true
    } else {
        false
    }
}

pub fn extract_tarpaulin_commands(input: &str) -> Vec<String> {
    lazy_static! {
        static ref FIX_LINES: Regex = RegexBuilder::new(r#"\\\s*\n"#)
            .multi_line(true)
            .build()
            .unwrap();
        static ref TEST_CMD: Regex =
            Regex::new(r#"cargo\s+test\s*([\-a-zA-Z\d\\\s\$\{\}\."~\n])*(;?|\s*~\\\s*\n|&&|$)"#)
                .unwrap();
    }
    let line_break_removed = FIX_LINES.replace_all(input, " ");
    let mut res = vec![];
    for s in line_break_removed.lines() {
        for m in TEST_CMD.find_iter(s) {
            res.push(m.as_str().replace("cargo test", "cargo tarpaulin"));
        }
    }
    res
}

pub fn init_command(
    root: impl AsRef<Path>,
    jobs: Option<&usize>,
    context: &Context,
    spec: &CrateSpec,
    cmd: &mut Command,
) {
    if let Some(j) = jobs {
        cmd.args(&["--jobs", j.to_string().as_str()]);
    }
    cmd.args(&default_args())
        .env("RUST_LOG", "cargo_tarpaulin=info")
        .args(&context.args)
        .args(&spec.args)
        .envs(&spec.env)
        .envs(&context.env)
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
}

fn default_spawn(
    root: impl AsRef<Path>,
    jobs: Option<&usize>,
    context: &Context,
    spec: &CrateSpec,
) -> io::Result<Child> {
    let mut cmd = Command::new("cargo");
    init_command(root, jobs, context, spec, &mut cmd);

    cmd.spawn()
}

pub fn spawn_tarpaulin(
    root: impl AsRef<Path>,
    jobs: Option<&usize>,
    context: &Context,
    spec: &CrateSpec,
) -> io::Result<Child> {
    github::get_command(root.as_ref(), jobs, context, spec)
        .or_else(|_| gitlab::get_command(root.as_ref(), jobs, context, spec))
        .or_else(|_| travis::get_command(root.as_ref(), jobs, context, spec))
        .or_else(|_| default_spawn(root, jobs, context, spec))
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn command_regex_test() {
        assert_eq!(
            extract_tarpaulin_commands("cargo test"),
            vec!["cargo tarpaulin".to_string()]
        );
        assert_eq!(
            extract_tarpaulin_commands("cargo test --all-features"),
            vec!["cargo tarpaulin --all-features".to_string()]
        );
        assert_eq!(
            extract_tarpaulin_commands("cargo test --all-features -- --test-threads 8"),
            vec!["cargo tarpaulin --all-features -- --test-threads 8".to_string()]
        );
        assert_eq!(
            extract_tarpaulin_commands("cargo test -- --skip \"this\""),
            vec!["cargo tarpaulin -- --skip \"this\"".to_string()]
        );
        assert_eq!(
            extract_tarpaulin_commands("cargo test ; -- --skip \"this\""),
            vec!["cargo tarpaulin ;".to_string()]
        );
        assert_eq!(
            extract_tarpaulin_commands("cargo test \\ \n -- hello"),
            vec!["cargo tarpaulin   -- hello".to_string()]
        );
        assert_eq!(
            extract_tarpaulin_commands("cargo test\n -- hello"),
            vec!["cargo tarpaulin".to_string()]
        );
    }
}
