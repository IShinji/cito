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

/// Experimental: partition node IDs into chunks (whole files stay together,
/// so fixture scoping behaves like `pytest-xdist --dist loadfile`) and fan
/// them out across `workers` pytest processes.
pub fn run(files: Vec<FileTests>, workers: usize, chunk_size: usize, python: &str) -> Outcome {
    let mut chunks: Vec<Vec<String>> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    for file in files {
        if file.tests.is_empty() {
            continue;
        }
        if !current.is_empty() && current.len() + file.tests.len() > chunk_size {
            chunks.push(std::mem::take(&mut current));
        }
        current.extend(file.tests.iter().map(|t| format!("{}::{}", file.path, t)));
    }
    if !current.is_empty() {
        chunks.push(current);
    }

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
                match output {
                    // Exit code 5 means "no tests collected"; harmless here.
                    Ok(out) if out.status.success() || out.status.code() == Some(5) => {
                        let stdout = String::from_utf8_lossy(&out.stdout);
                        if let Some(summary) = stdout.lines().rev().find(|l| !l.trim().is_empty()) {
                            println!("{}", summary.trim());
                        }
                    }
                    Ok(out) => {
                        *failed.lock().expect("failed lock") += 1;
                        print!("{}", String::from_utf8_lossy(&out.stdout));
                        eprint!("{}", String::from_utf8_lossy(&out.stderr));
                    }
                    Err(err) => {
                        *failed.lock().expect("failed lock") += 1;
                        eprintln!("cito: failed to spawn {python}: {err}");
                    }
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
