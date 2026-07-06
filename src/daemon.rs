//! Cross-session warm-worker daemon (Unix only). One daemon per rootdir,
//! reached over a Unix socket in the temp dir (rootdir paths can exceed the
//! 104-byte macOS socket limit, so the path is keyed by a hash). The daemon
//! holds a [`WarmPool`]; clients collect locally (fast) and ship chunks over.
//! Worker freshness across sessions is handled inside the pytest workers
//! themselves: the shim purges any imported module whose file mtime changed.
#![cfg(unix)]

use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::collector::FileTests;
use crate::runner::{Counts, Outcome};
use crate::warm::WarmPool;

const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn socket_path(rootdir: &Path) -> PathBuf {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    rootdir.hash(&mut hasher);
    std::env::temp_dir().join(format!("cito-{:016x}.sock", hasher.finish()))
}

#[derive(Serialize)]
struct RequestRef<'a> {
    version: &'a str,
    cmd: &'a str,
    python: &'a str,
    workers: usize,
    chunk: usize,
    maxfail: usize,
    extra_args: &'a [String],
    coverage_base: Option<&'a str>,
    files: &'a [FileTests],
}

#[derive(Serialize, Deserialize)]
struct Request {
    version: String,
    cmd: String, // "ping" | "shutdown" | "run"
    python: String,
    workers: usize,
    chunk: usize,
    maxfail: usize,
    extra_args: Vec<String>,
    coverage_base: Option<String>,
    files: Vec<FileTests>,
}

impl Request {
    fn control(cmd: &str) -> Request {
        Request {
            version: VERSION.to_string(),
            cmd: cmd.to_string(),
            python: String::new(),
            workers: 0,
            chunk: 0,
            maxfail: 0,
            extra_args: Vec::new(),
            coverage_base: None,
            files: Vec::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Default)]
struct Response {
    version: String,
    chunks: usize,
    failed: usize,
    skipped_chunks: usize,
    seconds: f64,
    passed: u32,
    failed_tests: u32,
    skipped: u32,
    failed_ids: Vec<String>,
    failure_output: Vec<String>,
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// Foreground serve loop; the client launches this detached via
/// `cito daemon serve`.
pub fn serve(rootdir: &Path) -> std::io::Result<()> {
    let path = socket_path(rootdir);
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    // (python, workers) -> pool; rebuilt when the client asks differently.
    let mut pool: Option<(String, usize, WarmPool)> = None;

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let mut reader = BufReader::new(&stream);
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
            continue;
        }
        let Ok(request) = serde_json::from_str::<Request>(&line) else {
            continue;
        };
        let mut response = Response {
            version: VERSION.to_string(),
            ..Response::default()
        };
        match request.cmd.as_str() {
            "ping" => {}
            "shutdown" => {
                let _ = send(&stream, &response);
                break;
            }
            "run" => {
                let rebuild = match &pool {
                    Some((python, workers, _)) => {
                        python != &request.python || *workers != request.workers
                    }
                    None => true,
                };
                if rebuild {
                    pool = Some((
                        request.python.clone(),
                        request.workers,
                        WarmPool::new(&request.python, request.workers),
                    ));
                }
                let (_, _, pool) = pool.as_ref().expect("pool just ensured");
                let outcome = pool.run(
                    request.files,
                    request.chunk,
                    request.maxfail,
                    &[],
                    &request.extra_args,
                    request.coverage_base.as_deref(),
                );
                response.chunks = outcome.chunks;
                response.failed = outcome.failed;
                response.skipped_chunks = outcome.skipped_chunks;
                response.seconds = outcome.seconds;
                response.passed = outcome.counts.passed;
                response.failed_tests = outcome.counts.failed;
                response.skipped = outcome.counts.skipped;
                response.failed_ids = outcome.failed_ids;
                response.failure_output = outcome.failure_output;
            }
            _ => {}
        }
        let _ = send(&stream, &response);
    }
    let _ = std::fs::remove_file(&path);
    Ok(())
}

fn send(mut stream: &UnixStream, response: &Response) -> std::io::Result<()> {
    let mut body = serde_json::to_string(response).expect("response serializes");
    body.push('\n');
    stream.write_all(body.as_bytes())
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

fn roundtrip<T: Serialize>(path: &Path, request: &T) -> Option<Response> {
    let mut stream = UnixStream::connect(path).ok()?;
    let mut body = serde_json::to_string(request).expect("request serializes");
    body.push('\n');
    stream.write_all(body.as_bytes()).ok()?;
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    serde_json::from_str(&line).ok()
}

fn ping(path: &Path) -> Option<String> {
    roundtrip(path, &Request::control("ping")).map(|r| r.version)
}

fn spawn_daemon(rootdir: &Path) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe()?;
    std::process::Command::new(exe)
        .args(["daemon", "serve"])
        .current_dir(rootdir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()?;
    Ok(())
}

/// Connect to a fresh, version-matched daemon, starting or replacing one as
/// needed. Returns the socket path.
pub fn ensure(rootdir: &Path) -> Option<PathBuf> {
    let path = socket_path(rootdir);
    match ping(&path) {
        Some(version) if version == VERSION => return Some(path),
        Some(_) => {
            // Version skew: retire the old daemon.
            let _ = roundtrip(&path, &Request::control("shutdown"));
        }
        None => {}
    }
    let _ = std::fs::remove_file(&path);
    if spawn_daemon(rootdir).is_err() {
        return None;
    }
    for _ in 0..60 {
        std::thread::sleep(Duration::from_millis(50));
        if ping(&path).as_deref() == Some(VERSION) {
            return Some(path);
        }
    }
    None
}

/// Run chunks on the daemon; falls back to None if it cannot be reached
/// (the caller then runs locally).
#[allow(clippy::too_many_arguments)]
pub fn run(
    rootdir: &Path,
    files: &[FileTests],
    python: &str,
    workers: usize,
    chunk: usize,
    maxfail: usize,
    extra_args: &[String],
    coverage_base: &str,
) -> Option<Outcome> {
    let path = ensure(rootdir)?;
    let request = RequestRef {
        version: VERSION,
        cmd: "run",
        python,
        workers,
        chunk,
        maxfail,
        extra_args,
        coverage_base: Some(coverage_base),
        files,
    };
    let response = roundtrip(&path, &request)?;
    Some(Outcome {
        chunks: response.chunks,
        failed: response.failed,
        skipped_chunks: response.skipped_chunks,
        seconds: response.seconds,
        counts: Counts {
            passed: response.passed,
            failed: response.failed_tests,
            skipped: response.skipped,
        },
        failed_ids: response.failed_ids,
        failure_output: response.failure_output,
    })
}

pub fn stop(rootdir: &Path) -> bool {
    let path = socket_path(rootdir);
    roundtrip(&path, &Request::control("shutdown")).is_some()
}

pub fn status(rootdir: &Path) -> Option<String> {
    ping(&socket_path(rootdir))
}
