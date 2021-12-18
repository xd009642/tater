use crate::runner::*;
use std::fs;
use std::io;
use std::path::Path;
use std::process::{Child, Command, Stdio};

pub fn get_command(
    root: impl AsRef<Path>,
    context: &Context,
    spec: &CrateSpec,
) -> io::Result<Child> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "tater can't interpret travis yet",
    ))
}
