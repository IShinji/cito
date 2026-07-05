use std::path::PathBuf;
use std::process::ExitCode;

use crate::{collector, runner};

fn roots(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    if paths.is_empty() {
        vec![PathBuf::from(".")]
    } else {
        paths
    }
}

pub fn collect(paths: Vec<PathBuf>, json: bool, count: bool) -> ExitCode {
    let files = collector::collect(&roots(paths));
    let total: usize = files.iter().map(|f| f.tests.len()).sum();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&files).expect("collection results serialize")
        );
    } else if count {
        println!("{total}");
    } else {
        for file in &files {
            for test in &file.tests {
                println!("{}::{}", file.path, test);
            }
        }
        eprintln!("cito: collected {total} tests");
    }
    ExitCode::SUCCESS
}

pub fn run(paths: Vec<PathBuf>, workers: Option<usize>, chunk: usize, python: String) -> ExitCode {
    let files = collector::collect(&roots(paths));
    let total: usize = files.iter().map(|f| f.tests.len()).sum();
    let workers = workers.unwrap_or_else(num_cpus::get);
    eprintln!("cito: running {total} tests across {workers} workers (experimental)");
    let outcome = runner::run(files, workers, chunk, &python);
    eprintln!(
        "cito: {} chunk(s), {} failed, {:.2}s wall",
        outcome.chunks, outcome.failed, outcome.seconds
    );
    if outcome.failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
