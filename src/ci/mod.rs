use crate::runner::*;
use std::io;
use std::path::Path;
use std::process::{Child, Command, Stdio};

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

pub fn init_command(root: impl AsRef<Path>, cmd: &mut Command) {
    cmd.args(&default_args())
        .env("RUST_LOG", "cargo_tarpaulin=info")
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
}

fn default_spawn(root: impl AsRef<Path>, context: &Context, spec: &CrateSpec) -> io::Result<Child> {
    let mut cmd = Command::new("cargo");
    init_command(root, &mut cmd);

    cmd.args(&context.args)
        .args(&spec.args)
        .envs(&spec.env)
        .envs(&context.env)
        .spawn()
}

pub fn spawn_tarpaulin(
    root: impl AsRef<Path>,
    context: &Context,
    spec: &CrateSpec,
) -> io::Result<Child> {
    github::get_command(root.as_ref(), context, spec)
        .or_else(|_| gitlab::get_command(root.as_ref(), context, spec))
        .or_else(|_| travis::get_command(root.as_ref(), context, spec))
        .or_else(|_| default_spawn(root, context, spec))
}
