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
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Collect { paths, json, count } => cito::commands::collect(paths, json, count),
        Command::Run {
            paths,
            workers,
            chunk,
            python,
        } => cito::commands::run(paths, workers, chunk, python),
    }
}
