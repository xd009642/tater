use crate::runner::*;
use std::collections::HashSet;
use std::env;
use std::fs::{create_dir, create_dir_all, read_to_string, remove_file, rename, File};
use std::io::prelude::*;
use std::io::{self, BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use structopt::StructOpt;
use tracing::{error, info, warn};
use tracing_subscriber::{EnvFilter, Layer, Registry};
use url::Url;

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
    /// Maximum number of projects to run concurrently
    #[structopt(long = "project-jobs", default_value = "1", parse(try_from_str = parse_project_jobs))]
    project_jobs: usize,
    /// Keep failed project checkouts as compressed archives in their result directories
    #[structopt(long = "retain-failed")]
    retain_failed: bool,
    /// Stop when Tater's output reaches this size, for example 500MB or 5GiB
    #[structopt(long = "disk-budget", parse(try_from_str = parse_size::parse_size))]
    disk_budget: Option<u64>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(untagged)]
enum RepositoriesInput {
    Tater(Context),
    Collected(Vec<CollectedInvocation>),
}

#[derive(Debug, serde::Deserialize)]
struct CollectedInvocation {
    url: String,
    command: Vec<String>,
    command_file: String,
}

impl RepositoriesInput {
    fn into_context(self) -> Result<Context, Box<dyn std::error::Error>> {
        match self {
            Self::Tater(context) => Ok(context),
            Self::Collected(invocations) => {
                let mut crates = Vec::with_capacity(invocations.len());
                for invocation in invocations {
                    let tarpaulin = invocation
                        .command
                        .iter()
                        .position(|argument| argument == "tarpaulin")
                        .ok_or_else(|| {
                            format!(
                                "collected command for {} does not invoke cargo tarpaulin",
                                invocation.url
                            )
                        })?;
                    if invocation.command.first().map(String::as_str) != Some("cargo") {
                        return Err(format!(
                            "collected command for {} does not invoke cargo tarpaulin",
                            invocation.url
                        )
                        .into());
                    }
                    crates.push(CrateSpec {
                        repository_url: Url::parse(&invocation.url)?,
                        args: collected_arguments(&invocation.command[tarpaulin + 1..]),
                        env: Default::default(),
                        toolchain: invocation.command[1..tarpaulin]
                            .iter()
                            .find(|argument| argument.starts_with('+'))
                            .cloned(),
                        setup: None,
                        teardown: None,
                        ci: Some(CiInvocation {
                            command: invocation.command,
                            command_file: invocation.command_file,
                        }),
                    });
                }
                Ok(Context {
                    crates,
                    ..Context::default()
                })
            }
        }
    }
}

fn collected_arguments(arguments: &[String]) -> Vec<String> {
    let mut result = Vec::new();
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        if argument == "--coveralls" || argument == "--color" {
            index += 1;
            if index < arguments.len() && !arguments[index].starts_with('-') {
                if arguments[index].contains("${{") {
                    while index < arguments.len() && arguments[index] != "}}" {
                        index += 1;
                    }
                    index += usize::from(index < arguments.len());
                } else {
                    index += 1;
                }
            }
            continue;
        }
        if argument.starts_with("--coveralls=")
            || argument.starts_with("--color=")
            || argument.contains(">/dev/null")
        {
            index += 1;
            continue;
        }
        if index + 1 < arguments.len() && arguments[index + 1].contains("${{") {
            index += 2;
            while index < arguments.len() && arguments[index] != "}}" {
                index += 1;
            }
            index += usize::from(index < arguments.len());
            continue;
        }
        if argument.contains("${{") {
            while index < arguments.len() && arguments[index] != "}}" {
                index += 1;
            }
            index += usize::from(index < arguments.len());
            continue;
        }
        result.push(argument.clone());
        index += 1;
    }
    result
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
    let context = serde_json::from_reader::<_, RepositoriesInput>(reader)?.into_context()?;
    let options = RunOptions {
        jobs: args.jobs,
        project_jobs: args.project_jobs,
        retain_failed: args.retain_failed,
        disk_budget: args.disk_budget,
    };
    run_tater(&context, &args.output, options, ctrlc_events)?;
    Ok(())
}

fn ctrl_handler() -> Result<Arc<AtomicBool>, ctrlc::Error> {
    let interrupted = Arc::new(AtomicBool::new(false));
    let handler_flag = Arc::clone(&interrupted);
    ctrlc::set_handler(move || {
        handler_flag.store(true, Ordering::SeqCst);
    })?;
    Ok(interrupted)
}

fn parse_project_jobs(value: &str) -> Result<usize, String> {
    match value.parse::<usize>() {
        Ok(0) => Err("project jobs must be greater than zero".to_string()),
        Ok(jobs) => Ok(jobs),
        Err(error) => Err(error.to_string()),
    }
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

#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
enum ProjectOutcome {
    Passed,
    Failed,
    Skipped(String),
}

#[derive(Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
struct Progress {
    outcomes: Vec<Option<ProjectOutcome>>,
}

fn read_status(path: &Path) -> io::Result<HashSet<String>> {
    match read_to_string(path) {
        Ok(contents) => Ok(contents.lines().map(str::to_string).collect()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(HashSet::new()),
        Err(error) => Err(error),
    }
}

fn get_progress(
    progress_file: &Path,
    pass_file: &Path,
    fail_file: &Path,
    context: &Context,
) -> io::Result<Progress> {
    let contents = match read_to_string(progress_file) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(Progress {
                outcomes: vec![None; context.crates.len()],
            })
        }
        Err(error) => return Err(error),
    };
    if let Ok(mut progress) = serde_json::from_str::<Progress>(&contents) {
        progress.outcomes.resize(context.crates.len(), None);
        progress.outcomes.truncate(context.crates.len());
        return Ok(progress);
    }

    // Numeric checkpoints were written by older Tater versions. Recover their outcomes from the
    // existing reports once, then immediately migrate to the structured format on the next result.
    let completed = contents.trim().parse::<usize>().map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidData, "invalid progress file contents")
    })?;
    let passes = read_status(pass_file)?;
    let failures = read_status(fail_file)?;
    let mut outcomes = vec![None; context.crates.len()];
    for (index, project) in context.crates.iter().take(completed).enumerate() {
        let id = project.project_id();
        let name = project.name().unwrap_or("unnamed_project");
        outcomes[index] = if failures.contains(&id) || failures.contains(name) {
            Some(ProjectOutcome::Failed)
        } else if passes.contains(&id) || passes.contains(name) {
            Some(ProjectOutcome::Passed)
        } else {
            warn!("No prior outcome found for completed project {}", index + 1);
            Some(ProjectOutcome::Failed)
        };
    }
    Ok(Progress { outcomes })
}

fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    let temporary = path.with_extension("tmp");
    let mut file = BufWriter::new(File::create(&temporary)?);
    file.write_all(contents)?;
    file.flush()?;
    file.get_ref().sync_all()?;
    rename(temporary, path)?;
    #[cfg(unix)]
    File::open(
        path.parent()
            .expect("output file must have a parent directory"),
    )?
    .sync_all()?;
    Ok(())
}

fn write_progress(progress_file: &Path, progress: &Progress) -> io::Result<()> {
    let contents = serde_json::to_vec(progress)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    write_atomic(progress_file, &contents)
}

fn write_status_reports(
    pass_file: &Path,
    fail_file: &Path,
    skip_file: &Path,
    context: &Context,
    progress: &Progress,
) -> io::Result<()> {
    let mut passes = String::new();
    let mut failures = String::new();
    let mut skips = String::new();
    for (project, outcome) in context.crates.iter().zip(&progress.outcomes) {
        match outcome {
            Some(ProjectOutcome::Passed) => {
                passes.push_str(&project.project_id());
                passes.push('\n');
            }
            Some(ProjectOutcome::Failed) => {
                failures.push_str(&project.project_id());
                failures.push('\n');
            }
            Some(ProjectOutcome::Skipped(reason)) => {
                skips.push_str(&project.project_id());
                skips.push_str(": ");
                skips.push_str(reason);
                skips.push('\n');
            }
            None => {}
        }
    }
    write_atomic(pass_file, passes.as_bytes())?;
    write_atomic(fail_file, failures.as_bytes())
        .and_then(|()| write_atomic(skip_file, skips.as_bytes()))
}

fn run_tater(
    context: &Context,
    output: &Path,
    options: RunOptions,
    interrupted: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("Processing {} projects", context.crates.len());
    let projects = output.join("projects");
    let results = output.join("results");
    let progress_file = output.join("progress");
    let pass_file = output.join("pass");
    let fail_file = output.join("fail");
    let skip_file = output.join("skip");
    if create_dir(&projects).is_err() {
        warn!("Projects directory already exists");
    }
    if create_dir(&results).is_err() {
        warn!("Results directory already exists");
    }
    let mut progress = get_progress(&progress_file, &pass_file, &fail_file, context)?;
    let completed = progress
        .outcomes
        .iter()
        .filter(|outcome| outcome.is_some())
        .count();
    if completed > 0 {
        info!("Resuming execution with {} completed projects", completed);
    }
    write_status_reports(&pass_file, &fail_file, &skip_file, context, &progress)?;

    if let Some(budget) = options.disk_budget {
        let used = directory_size(output)?;
        if used >= budget {
            return Err(format!("Disk budget reached: {} of {} bytes used", used, budget).into());
        }
    }

    let completed_snapshot = progress
        .outcomes
        .iter()
        .map(Option::is_some)
        .collect::<Vec<_>>();
    let next_project = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    enum WorkerMessage {
        Finished(usize, Result<RunOutcome, RunError>),
        Stopped(RunError),
    }
    let (result_sender, result_receiver) = mpsc::channel();
    let mut budget_failure = false;
    let mut stop_error = None;

    std::thread::scope(|scope| -> Result<(), Box<dyn std::error::Error>> {
        let projects_root = projects.as_path();
        let results_root = results.as_path();
        for _ in 0..options.project_jobs.min(context.crates.len().max(1)) {
            let sender = result_sender.clone();
            let completed = &completed_snapshot;
            let next = &next_project;
            let stop = &stop;
            let interrupted = &interrupted;
            scope.spawn(move || loop {
                if stop.load(Ordering::SeqCst) || interrupted.load(Ordering::SeqCst) {
                    break;
                }
                let index = next.fetch_add(1, Ordering::SeqCst);
                if index >= context.crates.len() {
                    break;
                }
                if completed[index] {
                    continue;
                }
                if let Some(budget) = options.disk_budget {
                    match directory_size(output) {
                        Ok(used) if used >= budget => {
                            stop.store(true, Ordering::SeqCst);
                            if sender
                                .send(WorkerMessage::Stopped(RunError::DiskBudgetExceeded {
                                    used,
                                    budget,
                                }))
                                .is_err()
                            {
                                break;
                            }
                            break;
                        }
                        Err(error) => {
                            stop.store(true, Ordering::SeqCst);
                            if sender
                                .send(WorkerMessage::Stopped(RunError::Output(error)))
                                .is_err()
                            {
                                break;
                            }
                            break;
                        }
                        _ => {}
                    }
                }
                let result = run_test(
                    index,
                    context,
                    &context.crates[index],
                    projects_root,
                    results_root,
                    &options,
                );
                if matches!(result, Err(RunError::DiskBudgetExceeded { .. })) {
                    stop.store(true, Ordering::SeqCst);
                }
                if sender.send(WorkerMessage::Finished(index, result)).is_err() {
                    break;
                }
            });
        }
        drop(result_sender);

        for message in result_receiver {
            let (index, result) = match message {
                WorkerMessage::Finished(index, result) => (index, result),
                WorkerMessage::Stopped(error) => {
                    if matches!(error, RunError::DiskBudgetExceeded { .. }) {
                        budget_failure = true;
                    }
                    error!("Stopped scheduling projects: {}", error);
                    stop_error = Some(error.to_string());
                    stop.store(true, Ordering::SeqCst);
                    continue;
                }
            };
            let project_id = context.crates[index].project_id();
            let outcome = match result {
                Ok(RunOutcome::Passed) => ProjectOutcome::Passed,
                Ok(RunOutcome::Skipped(reason)) => ProjectOutcome::Skipped(reason),
                Err(error) => {
                    if matches!(error, RunError::DiskBudgetExceeded { .. }) {
                        budget_failure = true;
                        stop.store(true, Ordering::SeqCst);
                    }
                    error!("Tarpaulin failed on {}: {}", project_id, error);
                    ProjectOutcome::Failed
                }
            };
            progress.outcomes[index] = Some(outcome);
            // The checkpoint is authoritative; reports are deterministic projections rebuilt from it.
            write_progress(&progress_file, &progress)?;
            write_status_reports(&pass_file, &fail_file, &skip_file, context, &progress)?;

            if let Some(budget) = options.disk_budget {
                let used = directory_size(output)?;
                if used > budget {
                    let retained_archive = results.join(&project_id).join("checkout.zip");
                    if retained_archive.is_file() {
                        remove_file(&retained_archive)?;
                        warn!(
                            "Removed retained checkout for {} to reclaim disk space",
                            project_id
                        );
                    }
                    budget_failure = true;
                    stop.store(true, Ordering::SeqCst);
                }
            }
        }
        Ok(())
    })?;

    let complete = progress.outcomes.iter().all(Option::is_some);
    let failures = progress
        .outcomes
        .iter()
        .filter(|outcome| matches!(outcome, Some(ProjectOutcome::Failed)))
        .count();
    if interrupted.load(Ordering::SeqCst) {
        info!("Pausing execution");
    }
    if let Some(error) = stop_error {
        return Err(format!("{}; progress has been saved", error).into());
    }
    if budget_failure {
        return Err("Disk budget exceeded; progress has been saved".into());
    }
    if !complete {
        if failures > 0 {
            return Err(format!(
                "Tarpaulin failed on {} completed projects before pausing",
                failures
            )
            .into());
        }
        return Ok(());
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
    use std::collections::HashMap;
    use url::Url;

    /// A completed run removes its checkpoint so the next invocation starts from the beginning.
    #[test]
    fn completed_run_clears_progress() {
        let output =
            std::env::temp_dir().join(format!("tater-progress-test-{}", std::process::id()));
        create_dir_all(&output).expect("test output directory should be created");
        let progress = output.join("progress");
        write_progress(
            &progress,
            &Progress {
                outcomes: Vec::new(),
            },
        )
        .expect("initial progress should be written");

        run_tater(
            &Context::default(),
            &output,
            RunOptions {
                jobs: None,
                project_jobs: 1,
                retain_failed: false,
                disk_budget: None,
            },
            Arc::new(AtomicBool::new(false)),
        )
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

        write_progress(
            &progress,
            &Progress {
                outcomes: vec![Some(ProjectOutcome::Passed), None],
            },
        )
        .expect("first progress value should be written");
        let replacement = Progress {
            outcomes: vec![Some(ProjectOutcome::Passed), Some(ProjectOutcome::Failed)],
        };
        write_progress(&progress, &replacement)
            .expect("replacement progress value should be written");

        assert_eq!(
            serde_json::from_str::<Progress>(
                &read_to_string(&progress).expect("progress should be readable")
            )
            .expect("progress should be valid"),
            replacement
        );
        assert!(!progress.with_extension("tmp").exists());
        std::fs::remove_dir_all(&output).expect("test output directory should be removed");
    }

    /// Human-readable decimal and binary disk budgets are accepted by the CLI.
    #[test]
    fn disk_budget_argument_accepts_human_readable_sizes() {
        let decimal = Args::from_iter_safe(&["tater", "--disk-budget", "5GB"])
            .expect("decimal disk budget should parse");
        let binary = Args::from_iter_safe(&["tater", "--disk-budget", "5GiB"])
            .expect("binary disk budget should parse");

        assert_eq!(decimal.disk_budget, Some(5_000_000_000));
        assert_eq!(binary.disk_budget, Some(5 * 1024 * 1024 * 1024));
    }

    /// Project concurrency must be positive and accepts an explicit worker count.
    #[test]
    fn project_jobs_argument_is_positive() {
        assert!(Args::from_iter_safe(&["tater", "--project-jobs", "0"]).is_err());
        let args = Args::from_iter_safe(&["tater", "--project-jobs", "3"])
            .expect("positive project worker count should parse");
        assert_eq!(args.project_jobs, 3);
    }

    /// Collector commands retain executable options without leaking CI expressions or secrets.
    #[test]
    fn collected_input_is_ready_to_execute() {
        let input = serde_json::from_str::<RepositoriesInput>(
            r#"[{"url":"https://github.com/example/project","command":["cargo","+nightly","tarpaulin","--features","full","--target","${{","matrix.target","}}","--coveralls","${{","secrets.TOKEN","}}","--release"],"command_file":"https://raw.githubusercontent.com/example/project/revision/.github/workflows/ci.yml"}]"#,
        )
        .expect("collector JSON should parse");
        let context = input.into_context().expect("collector JSON should convert");
        let project = &context.crates[0];

        assert_eq!(project.toolchain.as_deref(), Some("+nightly"));
        assert_eq!(project.args, vec!["--features", "full", "--release"]);
        assert!(project.ci.is_some());
    }

    /// Reaching the disk budget checkpoints no project and starts no clone.
    #[test]
    fn disk_budget_stops_before_next_project() {
        let output = std::env::temp_dir().join(format!("tater-budget-test-{}", std::process::id()));
        create_dir_all(&output).expect("test output directory should be created");
        let context = Context {
            toolchain: String::new(),
            target: None,
            crates: vec![CrateSpec {
                repository_url: Url::parse("https://example.invalid/owner/repository")
                    .expect("repository URL should be valid"),
                args: Vec::new(),
                env: HashMap::new(),
                toolchain: None,
                setup: None,
                teardown: None,
                ci: None,
            }],
            args: Vec::new(),
            env: HashMap::new(),
        };
        let error = run_tater(
            &context,
            &output,
            RunOptions {
                jobs: None,
                project_jobs: 1,
                retain_failed: false,
                disk_budget: Some(0),
            },
            Arc::new(AtomicBool::new(false)),
        )
        .expect_err("zero-byte budget should stop the run");

        assert!(error.to_string().contains("Disk budget reached"));
        assert!(!output.join("progress").exists());
        std::fs::remove_dir_all(&output).expect("test output directory should be removed");
    }
}
