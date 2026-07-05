use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "cito",
    version,
    about = "A fast, pytest-compatible test collector and runner."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Discover pytest-style tests and print their node IDs.
    Collect {
        /// Directories or files to search (defaults to the current directory).
        paths: Vec<PathBuf>,
        /// Emit JSON grouped by file instead of plain node IDs.
        #[arg(long)]
        json: bool,
        /// Print only the number of collected tests.
        #[arg(long, conflicts_with = "json")]
        count: bool,
        /// Probe this Python for module-level `pytest.importorskip(...)`
        /// dependencies, dropping modules pytest would skip in that
        /// environment. Without it, collection is fully static.
        #[arg(long)]
        python: Option<String>,
    },
    /// Run tests by fanning collected node IDs out across pytest processes (experimental).
    Run {
        /// Directories or files to search (defaults to the current directory).
        paths: Vec<PathBuf>,
        /// Number of worker processes (defaults to the number of logical CPUs).
        #[arg(short = 'n', long)]
        workers: Option<usize>,
        /// Maximum node IDs per pytest invocation.
        #[arg(long, default_value_t = 256)]
        chunk: usize,
        /// Python executable used to run pytest.
        #[arg(long, default_value = "python3")]
        python: String,
        /// Keep pytest workers warm across chunks (v0.2 preview): each worker
        /// imports pytest once and runs chunks in-process.
        #[arg(long)]
        warm: bool,
        /// Run only the tests that failed on the previous run.
        #[arg(long)]
        lf: bool,
        /// After running, watch for file changes and rerun affected test
        /// files (failed-first ordering applies on every rerun).
        #[arg(long)]
        watch: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Collect {
            paths,
            json,
            count,
            python,
        } => cito::commands::collect(paths, json, count, python),
        Command::Run {
            paths,
            workers,
            chunk,
            python,
            warm,
            lf,
            watch,
        } => cito::commands::run(paths, workers, chunk, python, warm, lf, watch),
    }
}
