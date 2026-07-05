use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::config::Config;
use crate::{collector, runner, warm};

/// pytest's argument fallback: explicit paths, else `testpaths` from the
/// config (relative to rootdir), else the invocation directory.
fn resolve_roots(paths: Vec<PathBuf>, config: &Config, cwd: &Path) -> Vec<PathBuf> {
    if !paths.is_empty() {
        return paths;
    }
    if !config.testpaths.is_empty() {
        let testpaths: Vec<PathBuf> = config
            .testpaths
            .iter()
            .map(|t| config.rootdir.join(t))
            .filter(|p| p.exists())
            .collect();
        if !testpaths.is_empty() {
            return testpaths;
        }
    }
    vec![cwd.to_path_buf()]
}

/// pytest anchors config/rootdir discovery at the common ancestor of the
/// given paths, falling back to the invocation directory.
fn discovery_anchor(paths: &[PathBuf], cwd: &Path) -> PathBuf {
    let mut ancestor: Option<PathBuf> = None;
    for path in paths {
        let abs = if path.is_absolute() {
            path.clone()
        } else {
            cwd.join(path)
        };
        let abs = abs.canonicalize().unwrap_or(abs);
        let dir = if abs.is_file() {
            abs.parent().unwrap_or(&abs).to_path_buf()
        } else {
            abs
        };
        ancestor = Some(match ancestor {
            None => dir,
            Some(current) => {
                let mut shared = PathBuf::new();
                for (a, b) in current.components().zip(dir.components()) {
                    if a != b {
                        break;
                    }
                    shared.push(a);
                }
                shared
            }
        });
    }
    ancestor.unwrap_or_else(|| cwd.to_path_buf())
}

fn collect_files(
    paths: Vec<PathBuf>,
    probe_python: Option<&str>,
) -> (Vec<collector::FileTests>, Config) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let anchor = discovery_anchor(&paths, &cwd);
    let config = Config::discover(&anchor);
    let roots = resolve_roots(paths, &config, &cwd);
    let files = collector::collect(&roots, &config, probe_python);
    (files, config)
}

pub fn collect(paths: Vec<PathBuf>, json: bool, count: bool, python: Option<String>) -> ExitCode {
    let (files, _config) = collect_files(paths, python.as_deref());
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

pub fn run(
    paths: Vec<PathBuf>,
    workers: Option<usize>,
    chunk: usize,
    python: String,
    warm_workers: bool,
) -> ExitCode {
    let (files, _config) = collect_files(paths, Some(&python));
    let total: usize = files.iter().map(|f| f.tests.len()).sum();
    let workers = workers.unwrap_or_else(num_cpus::get);
    let mode = if warm_workers { "warm" } else { "subprocess" };
    eprintln!("cito: running {total} tests across {workers} {mode} workers (experimental)");
    let outcome = if warm_workers {
        warm::run_warm(files, workers, chunk, &python)
    } else {
        runner::run(files, workers, chunk, &python)
    };
    eprintln!(
        "cito: {} passed, {} failed, {} skipped across {} chunk(s) ({} failed) in {:.2}s",
        outcome.counts.passed,
        outcome.counts.failed,
        outcome.counts.skipped,
        outcome.chunks,
        outcome.failed,
        outcome.seconds
    );
    if outcome.failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
