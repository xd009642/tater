use crate::runner::*;
use std::env;
use std::fs::{create_dir, create_dir_all, File, OpenOptions};
use std::io::prelude::*;
use std::io::{self, BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use structopt::StructOpt;
use tracing::{error, info, warn};
use tracing_subscriber::{EnvFilter, Layer, Registry};

mod ci;
mod runner;

#[derive(Debug, Default, Clone, Eq, PartialEq, Hash, Ord, PartialOrd, StructOpt)]
struct Args {
    /// Location to the repos file
    #[structopt(
        name = "input repos",
        short = "i",
        long = "input",
        default_value = "repos.json"
    )]
    repos: PathBuf,
    /// Directory to add the projects and results folder
    #[structopt(
        name = "output folder",
        short = "o",
        long = "output",
        default_value = "./output"
    )]
    output: PathBuf,
    /// Limit the number of jobs, this will limit cargo build jobs and also the number of test
    /// threads TODO
    #[structopt(name = "jobs", short = "j", long = "jobs")]
    jobs: Option<usize>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    setup_logging();
    let ctrlc_events = ctrl_handler()?;
    let args = Args::from_args();

    if !args.repos.is_file() {
        panic!("No repos file provided");
    }
    if args.output.is_file() {
        panic!("Output directory is a file");
    }
    if !args.output.is_dir() {
        info!("Creating output directory: {}", args.output.display());
        create_dir_all(&args.output).unwrap();
    }

    if let Ok(file) = File::open(args.repos) {
        let reader = BufReader::new(file);
        let context: Context = serde_json::from_reader(reader).expect("Unable to parse repos json");
        run_tater(&context, &args.output, args.jobs, ctrlc_events);
    }
    Ok(())
}

fn ctrl_handler() -> Result<mpsc::Receiver<()>, ctrlc::Error> {
    let (sender, receiver) = mpsc::channel();
    ctrlc::set_handler(move || {
        let _e = sender.send(());
    })?;
    Ok(receiver)
}

fn setup_logging() {
    let filter = match env::var("RUST_LOG") {
        Ok(_) => EnvFilter::from_default_env(),
        _ => EnvFilter::new("tater=info"),
    };
    let fmt = tracing_subscriber::fmt::Layer::default();
    let subscriber = filter.and_then(fmt).with_subscriber(Registry::default());
    tracing::subscriber::set_global_default(subscriber).unwrap();
}

/// Returns the next crate to process for resuming a workflow
fn get_progress(progress_file: &Path) -> std::io::Result<usize> {
    if progress_file.is_file() {
        let reader = BufReader::new(File::open(&progress_file)?);
        if let Some(line) = reader.lines().next() {
            let line = line?;
            match line.trim().parse::<usize>() {
                Ok(n) => Ok(n),
                Err(_) => {
                    warn!("Invalid progress file contents: {}", line);
                    Ok(0)
                }
            }
        } else {
            Ok(0)
        }
    } else {
        Ok(0)
    }
}

fn should_exit(progress_file: &Path, index: usize, rx: &mpsc::Receiver<()>) -> bool {
    if rx.try_recv().is_ok() {
        info!("Pausing execution");
        let progress_msg = "Unable to write progress file do it yourself";
        let mut f = File::create(&progress_file).expect(progress_msg);
        f.write_all(index.to_string().as_bytes())
            .expect(progress_msg);
        true
    } else {
        false
    }
}

fn get_status_linewriter(path: &Path, start_iter: usize) -> io::Result<BufWriter<File>> {
    let file = if start_iter == 0 {
        File::create(path)
    } else {
        OpenOptions::new().append(true).create(true).open(path)
    }?;
    Ok(BufWriter::new(file))
}

fn run_tater(context: &Context, output: &Path, jobs: Option<usize>, rx: mpsc::Receiver<()>) {
    info!("Processing {} projects", context.crates.len());
    let projects = output.join("projects");
    let results = output.join("results");
    let progress_file = output.join("progress");
    let pass_file = output.join("pass");
    let fail_file = output.join("fail");
    if create_dir(&projects).is_err() {
        warn!("Projects directory already exists");
    }
    if create_dir(&results).is_err() {
        warn!("Results directory already exists");
    }
    let start_from = match get_progress(&progress_file) {
        Ok(s) => s,
        Err(e) => {
            error!("Invalid progress file: {}", e);
            0
        }
    };
    if start_from > 0 {
        info!("Resuming execution from {}", start_from);
    }
    let mut fail_writer = get_status_linewriter(&fail_file, start_from).unwrap();
    let mut pass_writer = get_status_linewriter(&pass_file, start_from).unwrap();
    let mut failures = 0;
    for (i, proj) in context.crates.iter().enumerate().skip(start_from) {
        let proj_name = proj.name().unwrap_or_else(|| "unnamed_project");
        let res = run_test(i, context, proj, jobs.as_ref(), &projects, &results);
        let exit_index = if let Err(e) = res {
            failures += 1;
            error!("Tarpaulin failed on {}: {:?}", proj_name, e);
            i
        } else {
            let _ = pass_writer.write_all(proj_name.as_bytes());
            let _ = pass_writer.write_all(b"\n");
            let _ = pass_writer.flush();
            i + 1
        };

        if should_exit(&progress_file, exit_index, &rx) {
            let _ = fail_writer.write_all(proj_name.as_bytes());
            let _ = fail_writer.write_all(b"\n");
            let _ = fail_writer.flush();
            return;
        } else if i == exit_index {
            let _ = fail_writer.write_all(proj_name.as_bytes());
            let _ = fail_writer.write_all(b"\n");
            let _ = fail_writer.flush();
        }
    }
    if failures > 0 {
        error!(
            "Tarpaulin failed on {}/{} projects",
            failures,
            context.crates.len()
        );
    }
}
