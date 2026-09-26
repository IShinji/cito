//! Cross-session warm-worker daemon (Unix only). One daemon per rootdir,
//! reached over a Unix socket in a private per-user directory
//! (`$XDG_RUNTIME_DIR/cito`, else `$TMPDIR/cito-<uid>`, mode 0700 and
//! ownership-checked; rootdir paths can exceed the 104-byte macOS socket
//! limit, so the socket name is keyed by a hash). A lock file next to the
//! socket guarantees a single serving daemon per rootdir. The daemon holds
//! a [`WarmPool`] keyed by the client's interpreter, worker count, working
//! directory, and full environment; clients collect locally (fast) and ship
//! chunks over. Worker freshness across sessions is handled inside the
//! pytest workers themselves (see `warm.rs`).
#![cfg(unix)]

use std::fs::{DirBuilder, File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::collector::FileTests;
use crate::runner::{Counts, Outcome};
use crate::warm::{SpawnSpec, WarmPool};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long a connected client may take to send its request line.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// How long control commands (ping/shutdown) wait for a reply.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);

/// The private per-user directory holding sockets and lock files. Created
/// 0700; refused when it is not a real directory owned by us with no
/// group/other permissions.
fn private_dir() -> std::io::Result<PathBuf> {
    let uid = unsafe { libc::getuid() };
    let dir = match std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
        Some(runtime) if runtime.is_absolute() && runtime.is_dir() => runtime.join("cito"),
        _ => std::env::temp_dir().join(format!("cito-{uid}")),
    };
    match DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(err) => return Err(err),
    }
    let meta = std::fs::symlink_metadata(&dir)?;
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "{} is not a private directory owned by the current user",
                dir.display()
            ),
        ));
    }
    Ok(dir)
}

fn rootdir_key(rootdir: &Path) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    rootdir.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

pub fn socket_path(rootdir: &Path) -> std::io::Result<PathBuf> {
    Ok(private_dir()?.join(format!("cito-{}.sock", rootdir_key(rootdir))))
}

fn lock_path(rootdir: &Path) -> std::io::Result<PathBuf> {
    Ok(private_dir()?.join(format!("cito-{}.lock", rootdir_key(rootdir))))
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
    cwd: Option<&'a Path>,
    env: Option<&'a [(String, String)]>,
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
    /// The client's working directory and environment; workers are spawned
    /// to match them.
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(default)]
    env: Option<Vec<(String, String)>>,
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
            cwd: None,
            env: None,
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
    #[serde(default)]
    exit_code: i32,
    #[serde(default)]
    unverified: Vec<String>,
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

type PoolKey = (String, usize, SpawnSpec);
type SharedPool = Arc<Mutex<Option<(PoolKey, Arc<WarmPool>)>>>;

/// Take the per-rootdir daemon lock without waiting. None means another
/// daemon holds it (and serves, or is about to).
fn acquire_lock(rootdir: &Path) -> std::io::Result<Option<File>> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(lock_path(rootdir)?)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(err)) => Err(err),
    }
}

/// Foreground serve loop; the client launches this detached via
/// `cito daemon serve`. Exits quietly when another daemon already serves
/// this rootdir.
pub fn serve(rootdir: &Path) -> std::io::Result<()> {
    let Some(lock) = acquire_lock(rootdir)? else {
        return Ok(());
    };
    let path = socket_path(rootdir)?;
    // Only the lock holder touches the socket, so a leftover file is stale.
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    let pool: SharedPool = Arc::new(Mutex::new(None));

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let pool = Arc::clone(&pool);
        let path = path.clone();
        // One thread per connection: a slow or idle client never blocks
        // others, and requests must arrive within REQUEST_TIMEOUT.
        std::thread::spawn(move || {
            if handle(&stream, &pool) {
                let _ = std::fs::remove_file(&path);
                std::process::exit(0);
            }
        });
    }
    drop(lock);
    Ok(())
}

/// Serve one connection; true when the daemon should shut down.
fn handle(stream: &UnixStream, pool: &SharedPool) -> bool {
    let _ = stream.set_read_timeout(Some(REQUEST_TIMEOUT));
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
        return false;
    }
    let Ok(request) = serde_json::from_str::<Request>(&line) else {
        return false;
    };
    let mut response = Response {
        version: VERSION.to_string(),
        ..Response::default()
    };
    match request.cmd.as_str() {
        "ping" => {}
        "shutdown" => {
            let _ = send(stream, &response);
            return true;
        }
        "run" => {
            let spec = SpawnSpec {
                cwd: request.cwd.clone(),
                env: request.env.clone(),
            };
            let key: PoolKey = (request.python.clone(), request.workers, spec.clone());
            let current = {
                let mut slot = pool.lock().expect("pool lock");
                match slot.as_ref() {
                    Some((existing, pool)) if *existing == key => Arc::clone(pool),
                    _ => {
                        // Replaced pools die once in-flight runs finish.
                        let fresh = Arc::new(WarmPool::new(&request.python, request.workers, spec));
                        *slot = Some((key, Arc::clone(&fresh)));
                        fresh
                    }
                }
            };
            let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let outcome = current.run(
                request.files,
                request.chunk,
                request.maxfail,
                &[],
                &request.extra_args,
                request.coverage_base.as_deref(),
                &root,
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
            response.exit_code = outcome.exit_code;
            response.unverified = outcome.unverified;
        }
        _ => {}
    }
    let _ = send(stream, &response);
    false
}

fn send(mut stream: &UnixStream, response: &Response) -> std::io::Result<()> {
    let mut body = serde_json::to_string(response).expect("response serializes");
    body.push('\n');
    stream.write_all(body.as_bytes())
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

fn roundtrip<T: Serialize>(
    path: &Path,
    request: &T,
    timeout: Option<Duration>,
) -> Option<Response> {
    let mut stream = UnixStream::connect(path).ok()?;
    stream.set_read_timeout(timeout).ok()?;
    let mut body = serde_json::to_string(request).expect("request serializes");
    body.push('\n');
    stream.write_all(body.as_bytes()).ok()?;
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    serde_json::from_str(&line).ok()
}

fn control(path: &Path, cmd: &str) -> Option<Response> {
    roundtrip(path, &Request::control(cmd), Some(CONTROL_TIMEOUT))
}

fn ping(path: &Path) -> Option<String> {
    control(path, "ping").map(|r| r.version)
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

/// Block until this client may spawn a daemon for `rootdir` (released on
/// drop). Best effort: without a lock file, spawning proceeds unguarded.
fn spawn_lock(rootdir: &Path) -> Option<File> {
    let path = private_dir()
        .ok()?
        .join(format!("cito-{}.spawn.lock", rootdir_key(rootdir)));
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(path)
        .ok()?;
    file.lock().ok()?;
    Some(file)
}

/// Connect to a fresh, version-matched daemon, starting or replacing one as
/// needed. Returns the socket path. Concurrent callers may each spawn a
/// daemon; the lock lets exactly one of them serve.
pub fn ensure(rootdir: &Path) -> Option<PathBuf> {
    let path = match socket_path(rootdir) {
        Ok(path) => path,
        Err(err) => {
            eprintln!("cito: daemon socket unavailable: {err}");
            return None;
        }
    };
    if ping(&path).as_deref() == Some(VERSION) {
        return Some(path);
    }
    // Serialize spawning across clients so concurrent first runs start one
    // daemon, not a crowd of losers racing for the serve lock.
    let _spawn_lock = spawn_lock(rootdir);
    match ping(&path) {
        Some(version) if version == VERSION => return Some(path),
        Some(_) => {
            // Version skew: retire the old daemon.
            let _ = control(&path, "shutdown");
        }
        None => {}
    }
    // A retiring daemon may still hold the serve lock for a moment, making
    // the new one bow out; respawn periodically until one answers.
    let deadline = Instant::now() + Duration::from_secs(6);
    let mut next_spawn = Instant::now();
    while Instant::now() < deadline {
        if Instant::now() >= next_spawn {
            if spawn_daemon(rootdir).is_err() {
                return None;
            }
            next_spawn = Instant::now() + Duration::from_secs(2);
        }
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
    cwd: &Path,
) -> Option<Outcome> {
    let path = ensure(rootdir)?;
    let env: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .collect();
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
        cwd: Some(cwd),
        env: Some(&env),
    };
    let response = roundtrip(&path, &request, None)?;
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
        exit_code: response.exit_code,
        unverified: response.unverified,
    })
}

pub fn stop(rootdir: &Path) -> bool {
    // Pre-0.4 daemons listened directly in $TMPDIR; retire ours too.
    let legacy = std::env::temp_dir().join(format!("cito-{}.sock", rootdir_key(rootdir)));
    let mut stopped = false;
    let ours = std::fs::symlink_metadata(&legacy)
        .is_ok_and(|meta| meta.uid() == unsafe { libc::getuid() });
    if ours {
        stopped |= control(&legacy, "shutdown").is_some();
    }
    if let Ok(path) = socket_path(rootdir) {
        stopped |= control(&path, "shutdown").is_some();
    }
    stopped
}

pub fn status(rootdir: &Path) -> Option<String> {
    ping(&socket_path(rootdir).ok()?)
}
