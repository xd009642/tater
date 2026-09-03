use crate::runner::*;
use std::env;
use std::fs::{create_dir, create_dir_all, remove_file, rename, File, OpenOptions};
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
    /// threads
    #[structopt(name = "jobs", short = "j", long = "jobs")]
    jobs: Option<usize>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    setup_logging();
    let ctrlc_events = ctrl_handler()?;
    let args = Args::from_args();

    if !args.repos.is_file() {
        return Err(format!("Repos file does not exist: {}", args.repos.display()).into());
    }
    if args.output.is_file() {
        return Err(format!("Output directory is a file: {}", args.output.display()).into());
    }
    if !args.output.is_dir() {
        info!("Creating output directory: {}", args.output.display());
        create_dir_all(&args.output)?;
    }

    let file = File::open(&args.repos)?;
    let reader = BufReader::new(file);
    let context: Context = serde_json::from_reader(reader)?;
    run_tater(&context, &args.output, args.jobs, ctrlc_events)?;
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

fn write_progress(progress_file: &Path, index: usize) -> io::Result<()> {
    let temporary = progress_file.with_extension("tmp");
    let mut file = File::create(&temporary)?;
    file.write_all(index.to_string().as_bytes())?;
    file.sync_all()?;
    rename(temporary, progress_file)?;
    #[cfg(unix)]
    File::open(
        progress_file
            .parent()
            .expect("progress file must have a parent directory"),
    )?
    .sync_all()?;
    Ok(())
}

fn should_exit(rx: &mpsc::Receiver<()>) -> bool {
    if rx.try_recv().is_ok() {
        info!("Pausing execution");
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

fn run_tater(
    context: &Context,
    output: &Path,
    jobs: Option<usize>,
    rx: mpsc::Receiver<()>,
) -> Result<(), Box<dyn std::error::Error>> {
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
    let mut fail_writer = get_status_linewriter(&fail_file, start_from)?;
    let mut pass_writer = get_status_linewriter(&pass_file, start_from)?;
    let mut failures = 0;
    for (i, proj) in context.crates.iter().enumerate().skip(start_from) {
        let project_id = proj.project_id();
        let res = run_test(i, context, proj, jobs.as_ref(), &projects, &results);
        let failed = match res {
            Err(error) => {
                failures += 1;
                error!("Tarpaulin failed on {}: {}", project_id, error);
                true
            }
            Ok(()) => {
                pass_writer.write_all(project_id.as_bytes())?;
                pass_writer.write_all(b"\n")?;
                pass_writer.flush()?;
                false
            }
        };
        if failed {
            fail_writer.write_all(project_id.as_bytes())?;
            fail_writer.write_all(b"\n")?;
            fail_writer.flush()?;
        }
        // Persist the result before advancing the checkpoint so resume cannot skip an unreported run.
        write_progress(&progress_file, i + 1)?;

        if should_exit(&rx) {
            if failures > 0 {
                return Err(format!(
                    "Tarpaulin failed on {}/{} processed projects before pausing",
                    failures,
                    i + 1 - start_from
                )
                .into());
            }
            return Ok(());
        }
    }
    match remove_file(&progress_file) {
        Ok(()) => {
            #[cfg(unix)]
            File::open(
                progress_file
                    .parent()
                    .expect("progress file must have a parent directory"),
            )?
            .sync_all()?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    if failures > 0 {
        error!(
            "Tarpaulin failed on {}/{} projects",
            failures,
            context.crates.len()
        );
        return Err(format!(
            "Tarpaulin failed on {}/{} projects",
            failures,
            context.crates.len()
        )
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A completed run removes its checkpoint so the next invocation starts from the beginning.
    #[test]
    fn completed_run_clears_progress() {
        let output =
            std::env::temp_dir().join(format!("tater-progress-test-{}", std::process::id()));
        create_dir_all(&output).expect("test output directory should be created");
        let progress = output.join("progress");
        write_progress(&progress, 42).expect("initial progress should be written");
        let (_sender, receiver) = mpsc::channel();

        run_tater(&Context::default(), &output, None, receiver)
            .expect("empty crater run should complete");

        assert!(!progress.exists());
        std::fs::remove_dir_all(&output).expect("test output directory should be removed");
    }

    /// Replacing a checkpoint leaves one complete value and no temporary file behind.
    #[test]
    fn progress_updates_atomically() {
        let output =
            std::env::temp_dir().join(format!("tater-progress-update-test-{}", std::process::id()));
        create_dir_all(&output).expect("test output directory should be created");
        let progress = output.join("progress");

        write_progress(&progress, 1).expect("first progress value should be written");
        write_progress(&progress, 27).expect("replacement progress value should be written");

        assert_eq!(
            get_progress(&progress).expect("progress should be valid"),
            27
        );
        assert!(!progress.with_extension("tmp").exists());
        std::fs::remove_dir_all(&output).expect("test output directory should be removed");
    }
}
