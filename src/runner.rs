use std::collections::VecDeque;
use std::process::Command;
use std::sync::Mutex;
use std::time::Instant;

use crate::collector::FileTests;

pub struct Outcome {
    pub chunks: usize,
    pub failed: usize,
    pub seconds: f64,
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

/// Print the pytest summary line for a passing chunk, or the full output for
/// a failing one. Returns whether the chunk counts as failed.
pub fn report_chunk(code: Option<i32>, stdout: &str, stderr: &str) -> bool {
    match code {
        // Exit code 5 means "no tests collected"; harmless here.
        Some(0) | Some(5) => {
            if let Some(summary) = stdout.lines().rev().find(|l| !l.trim().is_empty()) {
                println!("{}", summary.trim());
            }
            false
        }
        _ => {
            print!("{stdout}");
            eprint!("{stderr}");
            true
        }
    }
}

/// Fan chunks out across fresh `python -m pytest` subprocesses.
pub fn run(files: Vec<FileTests>, workers: usize, chunk_size: usize, python: &str) -> Outcome {
    let chunks = make_chunks(&files, chunk_size);
    let total = chunks.len();
    let queue = Mutex::new(VecDeque::from(chunks));
    let failed = Mutex::new(0usize);
    let start = Instant::now();
    std::thread::scope(|scope| {
        for _ in 0..workers.max(1) {
            scope.spawn(|| loop {
                let Some(ids) = queue.lock().expect("queue lock").pop_front() else {
                    break;
                };
                let output = Command::new(python)
                    .args(["-m", "pytest", "-q", "--no-header"])
                    .args(&ids)
                    .output();
                let chunk_failed = match output {
                    Ok(out) => report_chunk(
                        out.status.code(),
                        &String::from_utf8_lossy(&out.stdout),
                        &String::from_utf8_lossy(&out.stderr),
                    ),
                    Err(err) => {
                        eprintln!("cito: failed to spawn {python}: {err}");
                        true
                    }
                };
                if chunk_failed {
                    *failed.lock().expect("failed lock") += 1;
                }
            });
        }
    });

    let failed = *failed.lock().expect("failed lock");
    Outcome {
        chunks: total,
        failed,
        seconds: start.elapsed().as_secs_f64(),
    }
}
