use crate::runner::*;
use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::process::{Child, Command, Stdio};

pub mod github;
pub mod gitlab;
pub mod travis;

pub struct CiContext {
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
}

pub fn default_args() -> Vec<String> {
    vec![
        "tarpaulin".to_string(),
        "--debug".to_string(),
        "--color".to_string(),
        "never".to_string(),
    ]
}

fn default_spawn(root: impl AsRef<Path>, context: &Context, spec: &CrateSpec) -> io::Result<Child> {
    let mut args = default_args();
    args.extend_from_slice(&context.args);
    args.extend_from_slice(&spec.args);

    Command::new("cargo")
        .args(&args)
        .current_dir(root)
        .env("RUST_LOG", "cargo_tarpaulin=info")
        .envs(&spec.env)
        .envs(&context.env)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
}

pub fn spawn_tarpaulin<'a>(
    root: impl AsRef<Path>,
    context: &Context,
    spec: &CrateSpec,
) -> io::Result<Child> {
    github::get_command(root.as_ref(), context, spec)
        .or_else(|_| gitlab::get_command(root.as_ref(), context, spec))
        .or_else(|_| travis::get_command(root.as_ref(), context, spec))
        .or_else(|_| default_spawn(root, context, spec))
}
