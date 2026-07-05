use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Instant;

use serde::Deserialize;

use crate::collector::FileTests;
use crate::runner::{make_chunks, report_chunk, Counts, Outcome};

/// Each worker imports pytest once and then runs `pytest.main()` per chunk
/// in-process, killing the interpreter+import startup tax that the
/// subprocess runner pays per chunk. Execution stays inside real CPython, so
/// conftest, fixtures, and plugins keep working.
const WORKER_SHIM: &str = r#"
import contextlib, io, json, sys
import pytest

for line in sys.stdin:
    req = json.loads(line)
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
    sys.stdout.write(json.dumps({"code": code, "output": buf.getvalue()}) + "\n")
    sys.stdout.flush()
"#;

#[derive(Deserialize)]
struct Reply {
    code: i32,
    output: String,
}

pub fn run_warm(files: Vec<FileTests>, workers: usize, chunk_size: usize, python: &str) -> Outcome {
    let chunks = make_chunks(&files, chunk_size);
    let total = chunks.len();
    let queue = Mutex::new(VecDeque::from(chunks));
    let failed = Mutex::new(0usize);
    let totals = Mutex::new(Counts::default());
    let failures: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let start = Instant::now();
    std::thread::scope(|scope| {
        for _ in 0..workers.max(1) {
            scope.spawn(|| {
                let child = Command::new(python)
                    .args(["-c", WORKER_SHIM])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .spawn();
                let mut child = match child {
                    Ok(child) => child,
                    Err(err) => {
                        eprintln!("cito: failed to spawn {python}: {err}");
                        return;
                    }
                };
                let mut stdin = child.stdin.take().expect("piped stdin");
                let mut stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
                loop {
                    let Some(ids) = queue.lock().expect("queue lock").pop_front() else {
                        break;
                    };
                    let mut args = vec![
                        "-q".to_string(),
                        "--no-header".to_string(),
                        "-rfE".to_string(),
                    ];
                    args.extend(ids);
                    let request = serde_json::json!({ "args": args });
                    let mut line = String::new();
                    let ok = writeln!(stdin, "{request}").is_ok()
                        && matches!(stdout.read_line(&mut line), Ok(n) if n > 0);
                    let (chunk_failed, counts, ids_failed) = if ok {
                        match serde_json::from_str::<Reply>(&line) {
                            Ok(reply) => report_chunk(Some(reply.code), &reply.output, ""),
                            Err(err) => {
                                eprintln!("cito: bad worker reply: {err}");
                                (true, Counts::default(), Vec::new())
                            }
                        }
                    } else {
                        eprintln!("cito: pytest worker died; its chunk is marked failed");
                        (true, Counts::default(), Vec::new())
                    };
                    totals.lock().expect("totals lock").add(counts);
                    failures.lock().expect("failures lock").extend(ids_failed);
                    if chunk_failed {
                        *failed.lock().expect("failed lock") += 1;
                    }
                    if !ok {
                        break;
                    }
                }
                drop(stdin);
                let _ = child.wait();
            });
        }
    });

    // Chunks left behind by dead workers count as failures, never as silence.
    let leftover = queue.into_inner().expect("queue lock").len();
    let failed = *failed.lock().expect("failed lock") + leftover;
    let counts = *totals.lock().expect("totals lock");
    let failed_ids = failures.into_inner().expect("failures lock");
    Outcome {
        chunks: total,
        failed,
        seconds: start.elapsed().as_secs_f64(),
        counts,
        failed_ids,
    }
}
