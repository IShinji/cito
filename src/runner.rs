use std::collections::VecDeque;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use crate::collector::FileTests;

pub struct Outcome {
    pub chunks: usize,
    pub failed: usize,
    /// Chunks abandoned because `--maxfail` tripped.
    pub skipped_chunks: usize,
    pub seconds: f64,
    pub counts: Counts,
    /// Node IDs pytest reported as FAILED/ERROR (rootdir-relative).
    pub failed_ids: Vec<String>,
    /// Captured output of failed chunks, printed by the caller.
    pub failure_output: Vec<String>,
}

/// Test totals parsed from pytest's summary lines.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Counts {
    pub passed: u32,
    pub failed: u32,
    pub skipped: u32,
}

impl Counts {
    pub(crate) fn add(&mut self, other: Counts) {
        self.passed += other.passed;
        self.failed += other.failed;
        self.skipped += other.skipped;
    }
}

/// Parse "N passed, M failed, K skipped in X.XXs" from the last nonempty
/// line of a pytest run.
pub fn summary_counts(stdout: &str) -> Counts {
    let Some(line) = stdout.lines().rev().find(|l| !l.trim().is_empty()) else {
        return Counts::default();
    };
    let mut counts = Counts::default();
    let mut pending: Option<u32> = None;
    for token in line.split(|c: char| !c.is_ascii_alphanumeric()) {
        if token.is_empty() {
            continue;
        }
        if let Ok(n) = token.parse::<u32>() {
            pending = Some(n);
            continue;
        }
        match token {
            "passed" => counts.passed += pending.take().unwrap_or(0),
            "failed" | "error" | "errors" => counts.failed += pending.take().unwrap_or(0),
            "skipped" => counts.skipped += pending.take().unwrap_or(0),
            _ => pending = None,
        }
    }
    counts
}

/// Partition node IDs into chunks, keeping whole files together so fixture
/// scoping behaves like `pytest-xdist --dist loadfile`. IDs are built from
/// absolute paths so workers resolve them from any cwd.
pub fn make_chunks(files: &[FileTests], chunk_size: usize) -> Vec<Vec<String>> {
    let mut chunks: Vec<Vec<String>> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    for file in files {
        if file.tests.is_empty() {
            continue;
        }
        if !current.is_empty() && current.len() + file.tests.len() > chunk_size {
            chunks.push(std::mem::take(&mut current));
        }
        let path = file.abs_path.to_string_lossy();
        current.extend(file.tests.iter().map(|t| format!("{path}::{t}")));
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Node IDs from pytest's `-rfE` short summary lines.
pub fn failed_ids(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let rest = line
                .strip_prefix("FAILED ")
                .or_else(|| line.strip_prefix("ERROR "))?;
            Some(rest.split(" - ").next().unwrap_or(rest).trim().to_string())
        })
        .collect()
}

pub struct ChunkReport {
    pub failed: bool,
    pub counts: Counts,
    pub failed_ids: Vec<String>,
    /// Full output, present only for failed chunks.
    pub output: Option<String>,
}

/// Quiet on success; failed chunks carry their output for the caller.
pub fn report_chunk(code: Option<i32>, stdout: &str, stderr: &str) -> ChunkReport {
    let counts = summary_counts(stdout);
    let ids = failed_ids(stdout);
    match code {
        // Exit code 5 means "no tests collected"; harmless here.
        Some(0) | Some(5) => ChunkReport {
            failed: false,
            counts,
            failed_ids: ids,
            output: None,
        },
        _ => ChunkReport {
            failed: true,
            counts,
            failed_ids: ids,
            output: Some(format!("{stdout}{stderr}")),
        },
    }
}

/// Fan chunks out across fresh `python -m pytest` subprocesses.
pub fn run(
    files: Vec<FileTests>,
    workers: usize,
    chunk_size: usize,
    python: &str,
    maxfail: usize,
    extra_args: &[String],
    coverage_base: Option<&str>,
) -> Outcome {
    let chunks = make_chunks(&files, chunk_size);
    let total = chunks.len();
    let queue = Mutex::new(VecDeque::from(chunks));
    let failed = Mutex::new(0usize);
    let skipped = Mutex::new(0usize);
    let totals = Mutex::new(Counts::default());
    let failures: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let outputs: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let chunk_seq = AtomicUsize::new(0);
    let start = Instant::now();
    std::thread::scope(|scope| {
        for _ in 0..workers.max(1) {
            scope.spawn(|| loop {
                let Some(ids) = queue.lock().expect("queue lock").pop_front() else {
                    break;
                };
                let mut command = Command::new(python);
                command
                    .args(["-m", "pytest", "-q", "--no-header", "-rfE"])
                    .args(extra_args)
                    .args(&ids);
                if let Some(base) = coverage_base {
                    // Unique per chunk so parallel pytest-cov runs never
                    // clobber each other; combined after the run.
                    let seq = chunk_seq.fetch_add(1, Ordering::Relaxed);
                    command.env("COVERAGE_FILE", format!("{base}.{seq}"));
                }
                let output = command.output();
                let report = match output {
                    Ok(out) => report_chunk(
                        out.status.code(),
                        &String::from_utf8_lossy(&out.stdout),
                        &String::from_utf8_lossy(&out.stderr),
                    ),
                    Err(err) => ChunkReport {
                        failed: true,
                        counts: Counts::default(),
                        failed_ids: Vec::new(),
                        output: Some(format!("cito: failed to spawn {python}: {err}\n")),
                    },
                };
                {
                    let mut totals = totals.lock().expect("totals lock");
                    totals.add(report.counts);
                    if report.failed {
                        *failed.lock().expect("failed lock") += 1;
                    }
                    if maxfail > 0 && totals.failed as usize >= maxfail {
                        let mut queue = queue.lock().expect("queue lock");
                        *skipped.lock().expect("skipped lock") += queue.len();
                        queue.clear();
                    }
                }
                failures
                    .lock()
                    .expect("failures lock")
                    .extend(report.failed_ids);
                if let Some(output) = report.output {
                    outputs.lock().expect("outputs lock").push(output);
                }
            });
        }
    });

    let failed = *failed.lock().expect("failed lock");
    let counts = *totals.lock().expect("totals lock");
    let failed_ids = failures.into_inner().expect("failures lock");
    let skipped_chunks = *skipped.lock().expect("skipped lock");
    let failure_output = outputs.into_inner().expect("outputs lock");
    Outcome {
        chunks: total,
        failed,
        skipped_chunks,
        seconds: start.elapsed().as_secs_f64(),
        counts,
        failed_ids,
        failure_output,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pytest_summary_lines() {
        let pass = summary_counts("....\n11000 passed in 1.79s\n");
        assert_eq!((pass.passed, pass.failed, pass.skipped), (11000, 0, 0));
        let mixed = summary_counts("1 failed, 10 passed, 2 skipped in 0.21s\n");
        assert_eq!((mixed.passed, mixed.failed, mixed.skipped), (10, 1, 2));
        let errors = summary_counts("3 errors in 0.10s");
        assert_eq!(errors.failed, 3);
        assert_eq!(summary_counts(""), Counts::default());
    }
}
