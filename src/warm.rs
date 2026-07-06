use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;
use std::time::Instant;

use serde::Deserialize;

use crate::collector::FileTests;
use crate::runner::{make_chunks, report_chunk, ChunkReport, Counts, Outcome};

/// Each worker imports pytest once and then runs `pytest.main()` per chunk
/// in-process, killing the interpreter+import startup tax that the
/// subprocess runner pays per chunk. Execution stays inside real CPython, so
/// conftest, fixtures, and plugins keep working. A `purge` list of absolute
/// file paths evicts stale modules before running — required when the pool
/// outlives file edits (watch mode).
const WORKER_SHIM: &str = r#"
import contextlib, importlib, io, json, os, sys
import pytest

_mtimes = {}

def _remember():
    for mod in list(sys.modules.values()):
        f = getattr(mod, "__file__", None)
        if f and f not in _mtimes:
            try:
                _mtimes[f] = os.stat(f).st_mtime_ns
            except OSError:
                pass

def _purge(targets):
    stale = set(targets)
    for f, recorded in list(_mtimes.items()):
        try:
            current = os.stat(f).st_mtime_ns
        except OSError:
            current = None
        if current != recorded:
            stale.add(f)
            del _mtimes[f]
    if not stale:
        return
    for name, mod in list(sys.modules.items()):
        try:
            if getattr(mod, "__file__", None) in stale:
                del sys.modules[name]
        except Exception:
            pass
    importlib.invalidate_caches()

for line in sys.stdin:
    req = json.loads(line)
    for key, value in (req.get("env") or {}).items():
        os.environ[key] = value
    _purge(req.get("purge") or ())
    buf = io.StringIO()
    with contextlib.redirect_stdout(buf), contextlib.redirect_stderr(buf):
        try:
            code = int(pytest.main(req["args"]))
        except SystemExit as exc:
            code = int(exc.code or 0)
        except BaseException:
            import traceback
            traceback.print_exc()
            code = 3
    _remember()
    sys.stdout.write(json.dumps({"code": code, "output": buf.getvalue()}) + "\n")
    sys.stdout.flush()
"#;

#[derive(Deserialize)]
struct Reply {
    code: i32,
    output: String,
}

struct Worker {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Worker {
    fn spawn(python: &str) -> Option<Worker> {
        let mut child = Command::new(python)
            .args(["-c", WORKER_SHIM])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|err| eprintln!("cito: failed to spawn {python}: {err}"))
            .ok()?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        Some(Worker {
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
    ) -> Option<Reply> {
        let request = serde_json::json!({ "args": args, "purge": purge, "env": env });
        writeln!(self.stdin, "{request}").ok()?;
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
/// reuses it across iterations, passing changed files as `purge`).
pub struct WarmPool {
    python: String,
    workers: Vec<Mutex<Option<Worker>>>,
}

impl WarmPool {
    pub fn new(python: &str, size: usize) -> WarmPool {
        WarmPool {
            python: python.to_string(),
            workers: (0..size.max(1)).map(|_| Mutex::new(None)).collect(),
        }
    }

    pub fn run(
        &self,
        files: Vec<FileTests>,
        chunk_size: usize,
        maxfail: usize,
        purge: &[String],
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
        let chunk_seq = std::sync::atomic::AtomicUsize::new(0);
        let start = Instant::now();
        std::thread::scope(|scope| {
            for slot in &self.workers {
                scope.spawn(|| {
                    let mut slot = slot.lock().expect("worker slot");
                    loop {
                        let Some(ids) = queue.lock().expect("queue lock").pop_front() else {
                            break;
                        };
                        if slot.is_none() {
                            *slot = Worker::spawn(&self.python);
                        }
                        let Some(worker) = slot.as_mut() else {
                            *failed.lock().expect("failed lock") += 1;
                            continue;
                        };
                        let mut args = vec![
                            "-q".to_string(),
                            "--no-header".to_string(),
                            "-rfE".to_string(),
                        ];
                        args.extend(extra_args.iter().cloned());
                        args.extend(ids);
                        let env = match coverage_base {
                            Some(base) => {
                                let seq =
                                    chunk_seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                serde_json::json!({ "COVERAGE_FILE": format!("{base}.{seq}") })
                            }
                            None => serde_json::Value::Null,
                        };
                        let report = match worker.run_chunk(&args, purge, &env) {
                            Some(reply) => report_chunk(Some(reply.code), &reply.output, ""),
                            None => {
                                *slot = None;
                                ChunkReport {
                                    failed: true,
                                    counts: Counts::default(),
                                    failed_ids: Vec::new(),
                                    output: Some(
                                        "cito: pytest worker died; its chunk is marked failed\n"
                                            .to_string(),
                                    ),
                                }
                            }
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
                    }
                });
            }
        });

        // Chunks left behind by dead workers count as failures, not silence.
        let leftover = queue.into_inner().expect("queue lock").len();
        let failed = *failed.lock().expect("failed lock") + leftover;
        let skipped_chunks = *skipped.lock().expect("skipped lock");
        let counts = *totals.lock().expect("totals lock");
        let failed_ids = failures.into_inner().expect("failures lock");
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
}
