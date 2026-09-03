use crate::runner::*;
use lazy_static::lazy_static;
use regex::{Regex, RegexBuilder};
use std::io;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use tracing::{debug, info, warn};

pub mod compatibility;
pub mod github;
pub mod gitlab;
pub mod travis;
pub mod types;

pub fn default_args() -> Vec<String> {
    vec![
        "tarpaulin".to_string(),
        "--debug".to_string(),
        "--color".to_string(),
        "never".to_string(),
    ]
}

pub fn try_to_populate_command(data: &str, cmd: &mut Command) -> io::Result<bool> {
    // TODO need to split up commands and handle things like `cd blah && cargo test;
    // Also, find tarpaulin ran via shell commands
    if data.contains("cargo test") {
        debug!("Maybe one: '{}'", data);
        let commands = extract_tarpaulin_commands(data);
        info!("Found commands: {:?}", commands);
        if commands.is_empty() {
            return Ok(false);
        }
        if commands.len() > 1 {
            // Should generate a tarpaulin.toml for these commands
            warn!("Ignoring commands: {:?}", &commands[1..]);
        }
        let args =
            shlex::split(commands[0].trim_end_matches(&[';', '&'][..])).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Invalid shell quoting in command: {}", commands[0]),
                )
            })?;
        cmd.args(args.into_iter().skip(2));
        Ok(true)
    } else {
        Ok(false)
    }
}

pub fn extract_tarpaulin_commands(input: &str) -> Vec<String> {
    lazy_static! {
        static ref FIX_LINES: Regex = RegexBuilder::new(r#"\\\s*\n"#)
            .multi_line(true)
            .build()
            .unwrap();
        static ref TEST_CMD: Regex = Regex::new(r#"cargo\s+test[^\n;&]*"#).unwrap();
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
    if let Some(toolchain) = spec
        .toolchain
        .as_ref()
        .or_else(|| (!context.toolchain.is_empty()).then_some(&context.toolchain))
    {
        cmd.arg(toolchain);
    }
    cmd.args(&default_args());
    if let Some(j) = jobs {
        cmd.args(&["--jobs", j.to_string().as_str()]);
    }
    cmd.env("RUST_LOG", "cargo_tarpaulin=info")
        .args(
            context
                .target
                .iter()
                .flat_map(|target| ["--target", target]),
        )
        .args(&context.args)
        .args(&spec.args)
        .envs(&spec.env)
        .envs(&context.env)
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // A stalled run must be terminated as a unit: cargo, rustc, test binaries, and build scripts.
    #[cfg(unix)]
    cmd.process_group(0);
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
    if spec.ci.is_some() {
        return default_spawn(root, jobs, context, spec);
    }
    github::get_command(root.as_ref(), jobs, context, spec)
        .or_else(|_| gitlab::get_command(root.as_ref(), jobs, context, spec))
        .or_else(|_| travis::get_command(root.as_ref(), jobs, context, spec))
        .or_else(|_| default_spawn(root, jobs, context, spec))
}

#[cfg(test)]
mod test {
    use super::*;
    use std::collections::HashMap;
    use url::Url;

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
            vec!["cargo tarpaulin ".to_string()]
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

    /// Toolchain selection precedes Cargo options while target selection is passed to Tarpaulin.
    #[test]
    fn command_uses_configured_toolchain_and_target() {
        let context = Context {
            toolchain: "+nightly".to_string(),
            target: Some("x86_64-unknown-linux-musl".to_string()),
            args: vec!["--all-features".to_string()],
            ..Context::default()
        };
        let spec = CrateSpec {
            repository_url: Url::parse("https://example.com/owner/repo")
                .expect("repository URL should be valid"),
            args: vec!["--release".to_string()],
            env: HashMap::new(),
            toolchain: None,
            setup: None,
            teardown: None,
            ci: None,
        };
        let mut command = Command::new("cargo");

        init_command(".", Some(&4), &context, &spec, &mut command);

        let args = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            args,
            vec![
                "+nightly",
                "tarpaulin",
                "--debug",
                "--color",
                "never",
                "--jobs",
                "4",
                "--target",
                "x86_64-unknown-linux-musl",
                "--all-features",
                "--release",
            ]
        );
    }

    /// Shell quoting is removed without splitting a quoted test argument.
    #[test]
    fn inferred_command_preserves_quoted_arguments() {
        let mut command = Command::new("cargo");

        assert!(try_to_populate_command(
            "cargo test -- --skip \"test with spaces\" && echo done",
            &mut command
        )
        .expect("command should have valid shell quoting"));

        let args = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(args, vec!["--", "--skip", "test with spaces"]);
    }
}
