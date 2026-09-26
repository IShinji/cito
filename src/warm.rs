use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use serde::Deserialize;

use crate::collector::FileTests;
use crate::runner::{make_chunks, report_chunk, schedule, ChunkReport, Outcome};

/// Each worker imports pytest once and then runs `pytest.main()` per chunk
/// in-process, killing the interpreter+import startup tax that the
/// subprocess runner pays per chunk. Execution stays inside real CPython, so
/// conftest, fixtures, and plugins keep working.
///
/// Freshness: before each chunk the worker checks the mtimes of every
/// module it has imported (plus the explicit `purge` list of changed files
/// from watch mode). If anything changed, every project-local module (file
/// under the request's `root`, excluding virtualenvs/site-packages) is
/// evicted from `sys.modules`, so dependents re-import instead of keeping
/// stale `from helper import f` bindings.
///
/// Protocol: requests and replies travel over private duplicates of the
/// original stdin/stdout. fd 0 is then pointed at /dev/null and fd 1 at
/// stderr, so tests that write to the real file descriptors (`os.system`,
/// subprocesses under `-s`) or read stdin cannot corrupt the stream.
const WORKER_SHIM: &str = r#"
import contextlib, importlib, io, json, os, sys

_req = os.fdopen(os.dup(0), "r", encoding="utf-8")
_rep = os.fdopen(os.dup(1), "w", encoding="utf-8")
_null = os.open(os.devnull, os.O_RDONLY)
os.dup2(_null, 0)
os.close(_null)
os.dup2(2, 1)

import pytest

_mtimes = {}

def _remember():
    for mod in list(sys.modules.values()):
        f = getattr(mod, "__file__", None)
        if f and f not in _mtimes:
            try:
                _mtimes[f] = os.stat(f).st_mtime_ns
            except (OSError, TypeError, ValueError):
                pass

def _real(path):
    try:
        return os.path.realpath(path)
    except (OSError, TypeError, ValueError):
        return None

def _under(path, root):
    return path == root or path.startswith(root.rstrip(os.sep) + os.sep)

def _project_local(f, root, excluded):
    f = _real(f)
    if not f or not root or not _under(f, root):
        return False
    parts = f.split(os.sep)
    if "site-packages" in parts or "dist-packages" in parts:
        return False
    return not any(_under(f, p) for p in excluded)

def _purge(targets, root):
    stale = {p for p in (_real(t) for t in targets) if p}
    for f, recorded in list(_mtimes.items()):
        try:
            current = os.stat(f).st_mtime_ns
        except OSError:
            current = None
        if current != recorded:
            stale.add(_real(f) or f)
            del _mtimes[f]
    if not stale:
        return
    root = _real(root) if root else None
    # Interpreter prefixes inside the project (./.venv) are not project code.
    excluded = set()
    if root:
        for p in {sys.prefix, sys.base_prefix, sys.exec_prefix}:
            p = _real(p)
            if p and p != root and _under(p, root):
                excluded.add(p)
    for name, mod in list(sys.modules.items()):
        try:
            f = getattr(mod, "__file__", None)
            if not f:
                continue
            if _project_local(f, root, excluded) or _real(f) in stale:
                del sys.modules[name]
                _mtimes.pop(f, None)
        except Exception:
            pass
    sys.path_importer_cache.clear()
    importlib.invalidate_caches()

for line in _req:
    req = json.loads(line)
    for key, value in (req.get("env") or {}).items():
        os.environ[key] = value
    _purge(req.get("purge") or (), req.get("root"))
    buf = io.StringIO()
    with contextlib.redirect_stdout(buf), contextlib.redirect_stderr(buf):
        try:
            code = int(pytest.main(req["args"]))
        except SystemExit as exc:
            # Mirror the interpreter: None -> 0, int -> itself, anything
            # else is printed and exits 1.
            if exc.code is None:
                code = 0
            elif isinstance(exc.code, int):
                code = exc.code
            else:
                print(exc.code, file=sys.stderr)
                code = 1
        except BaseException:
            import traceback
            traceback.print_exc()
            code = 3
    _remember()
    _rep.write(json.dumps({"code": code, "output": buf.getvalue()}) + "\n")
    _rep.flush()
"#;

#[derive(Deserialize)]
struct Reply {
    code: i32,
    output: String,
}

/// How pool workers are launched: working directory and (optionally) a
/// complete replacement environment. The daemon uses this to make workers
/// match the client that asked for them.
#[derive(Clone, Default, PartialEq, Eq, Hash)]
pub struct SpawnSpec {
    pub cwd: Option<PathBuf>,
    pub env: Option<Vec<(String, String)>>,
}

struct Worker {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Worker {
    fn spawn(python: &str, spec: &SpawnSpec) -> Result<Worker, String> {
        let mut command = Command::new(python);
        command
            .args(["-c", WORKER_SHIM])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        if let Some(cwd) = &spec.cwd {
            command.current_dir(cwd);
        }
        if let Some(env) = &spec.env {
            command.env_clear().envs(env.iter().map(|(k, v)| (k, v)));
        }
        let mut child = command
            .spawn()
            .map_err(|err| format!("cito: failed to spawn {python}: {err}\n"))?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        Ok(Worker {
            child,
            stdin,
            stdout,
        })
    }

    /// Send one chunk; None means the worker died (its chunk is lost).
    fn run_chunk(
        &mut self,
        args: &[String],
        purge: &[String],
        env: &serde_json::Value,
        root: &Path,
    ) -> Option<Reply> {
        let request = serde_json::json!({
            "args": args,
            "purge": purge,
            "env": env,
            "root": root.to_string_lossy(),
        });
        writeln!(self.stdin, "{request}").ok()?;
        self.stdin.flush().ok()?;
        let mut line = String::new();
        match self.stdout.read_line(&mut line) {
            Ok(n) if n > 0 => serde_json::from_str(&line)
                .map_err(|err| eprintln!("cito: bad worker reply: {err}"))
                .ok(),
            _ => None,
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A pool of warm pytest workers that can outlive a single run (watch mode
/// and the daemon reuse it; workers re-check module freshness per chunk).
pub struct WarmPool {
    python: String,
    spec: SpawnSpec,
    /// Where workers run; pytest reports node IDs relative to it.
    cwd: PathBuf,
    workers: Vec<Mutex<Option<Worker>>>,
}

impl WarmPool {
    pub fn new(python: &str, size: usize, spec: SpawnSpec) -> WarmPool {
        let cwd = spec
            .cwd
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        WarmPool {
            python: python.to_string(),
            spec,
            cwd,
            workers: (0..size.max(1)).map(|_| Mutex::new(None)).collect(),
        }
    }

    /// Run `files`; `root` is the project rootdir (modules under it are
    /// evicted when anything changed), `purge` lists files known to have
    /// changed since the pool last ran.
    #[allow(clippy::too_many_arguments)]
    pub fn run(
        &self,
        files: Vec<FileTests>,
        chunk_size: usize,
        maxfail: usize,
        purge: &[String],
        extra_args: &[String],
        coverage_base: Option<&str>,
        root: &Path,
    ) -> Outcome {
        struct Slot<'a> {
            worker: &'a Mutex<Option<Worker>>,
            /// The purge list only needs delivering once per worker per run.
            purged: bool,
        }
        let chunks = make_chunks(&files, chunk_size);
        let chunk_seq = AtomicUsize::new(0);
        let exec = |state: &mut Slot, ids: &[String]| -> ChunkReport {
            let mut slot = state.worker.lock().expect("worker slot");
            if slot.is_none() {
                match Worker::spawn(&self.python, &self.spec) {
                    Ok(worker) => *slot = Some(worker),
                    Err(message) => return ChunkReport::crashed(message),
                }
            }
            let worker = slot.as_mut().expect("worker just ensured");
            let mut args = vec![
                "-q".to_string(),
                "--no-header".to_string(),
                "-rfE".to_string(),
            ];
            args.extend(extra_args.iter().cloned());
            args.extend(ids.iter().cloned());
            let env = match coverage_base {
                Some(base) => {
                    let seq = chunk_seq.fetch_add(1, Ordering::Relaxed);
                    serde_json::json!({ "COVERAGE_FILE": format!("{base}.{seq}") })
                }
                None => serde_json::Value::Null,
            };
            let purge_now = if state.purged { &[][..] } else { purge };
            state.purged = true;
            match worker.run_chunk(&args, purge_now, &env, root) {
                Some(reply) => report_chunk(Some(reply.code), &reply.output, ""),
                None => {
                    // Dead or desynchronized: drop (kills) and respawn later.
                    *slot = None;
                    ChunkReport::crashed(
                        "cito: pytest worker died; its chunk is marked failed\n".to_string(),
                    )
                }
            }
        };
        let states = self
            .workers
            .iter()
            .map(|worker| Slot {
                worker,
                purged: false,
            })
            .collect();
        schedule(chunks, states, maxfail, &self.cwd, &exec)
    }
}
