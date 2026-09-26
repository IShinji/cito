use std::collections::VecDeque;
use std::path::{Component, Path, PathBuf};
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
    /// Node IDs pytest reported as FAILED/ERROR, as pytest printed them
    /// (relative to the pytest process's working directory). A bare path
    /// is a file-level collection error.
    pub failed_ids: Vec<String>,
    /// Captured output of failed chunks, printed by the caller.
    pub failure_output: Vec<String>,
    /// Combined pytest exit code across chunks (see [`combine_codes`]); 5
    /// when no chunk ran.
    pub exit_code: i32,
    /// Absolute node IDs whose outcome is unknown: chunks skipped by
    /// `--maxfail`, lost to a dead worker, or aborted by pytest (exit 2-4
    /// not attributable to a single file).
    pub unverified: Vec<String>,
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

/// The file part of a node ID (`path::test` -> `path`).
pub fn id_file(id: &str) -> &str {
    id.split_once("::").map_or(id, |(file, _)| file)
}

/// Resolve `.`/`..` without touching the filesystem.
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Absolute, normalized form of a path pytest printed relative to its
/// working directory `cwd` (canonical when the file exists).
pub fn resolve_reported(file: &str, cwd: &Path) -> PathBuf {
    let joined = lexical_normalize(&cwd.join(file));
    joined.canonicalize().unwrap_or(joined)
}

/// Rewrite a pytest-reported node ID (cwd-relative) as rootdir-relative
/// with forward slashes — the `FileTests::path` convention. IDs outside the
/// rootdir are returned unchanged.
pub fn rootdir_relative_id(id: &str, cwd: &Path, rootdir: &Path) -> String {
    let (file, rest) = match id.split_once("::") {
        Some((file, rest)) => (file, Some(rest)),
        None => (id, None),
    };
    let abs = resolve_reported(file, cwd);
    let root = rootdir
        .canonicalize()
        .unwrap_or_else(|_| lexical_normalize(rootdir));
    let Ok(rel) = abs.strip_prefix(&root) else {
        return id.to_string();
    };
    let rel: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let rel = rel.join("/");
    match rest {
        Some(rest) => format!("{rel}::{rest}"),
        None => rel,
    }
}

/// Merge two pytest exit codes by severity. 5 ("no tests collected") only
/// survives when every chunk reported it; otherwise the larger of the
/// remaining codes wins (0 ok < 1 test failures < 2 interrupted < 3
/// internal error < 4 usage error), with unknown codes most severe.
pub fn combine_codes(a: i32, b: i32) -> i32 {
    fn rank(code: i32) -> i32 {
        match code {
            5 => 0,
            0..=4 => code + 1,
            _ => 6,
        }
    }
    if rank(a) >= rank(b) {
        a
    } else {
        b
    }
}

pub struct ChunkReport {
    pub failed: bool,
    /// pytest's exit code for the chunk (3 when the process died).
    pub code: i32,
    pub counts: Counts,
    pub failed_ids: Vec<String>,
    /// Node IDs of this chunk whose outcome is unknown.
    pub unverified: Vec<String>,
    /// Full output, present only for failed chunks.
    pub output: Option<String>,
}

impl ChunkReport {
    /// A chunk that produced no pytest result at all (spawn failure, dead
    /// worker, broken protocol): an internal error, never a pass.
    pub fn crashed(message: String) -> ChunkReport {
        ChunkReport {
            failed: true,
            code: 3,
            counts: Counts::default(),
            failed_ids: Vec::new(),
            unverified: Vec::new(),
            output: Some(message),
        }
    }
}

/// Quiet on success; failed chunks carry their output for the caller.
pub fn report_chunk(code: Option<i32>, stdout: &str, stderr: &str) -> ChunkReport {
    let counts = summary_counts(stdout);
    let ids = failed_ids(stdout);
    // Killed by a signal: no exit code at all.
    let code = code.unwrap_or(3);
    // Exit code 5 means "no tests collected"; harmless here.
    let failed = !matches!(code, 0 | 5);
    ChunkReport {
        failed,
        code,
        counts,
        failed_ids: ids,
        unverified: Vec::new(),
        output: failed.then(|| format!("{stdout}{stderr}")),
    }
}

/// Run one chunk through `exec`, isolating module-level collection errors:
/// pytest aborts the whole invocation (exit 2 "errors during collection" or
/// exit 4 "found no collectors") when any listed file fails to import, which
/// would hide every other test in the chunk. The broken files are reported
/// as failed (`ERROR path` entries) and the rest of the chunk is rerun.
pub fn run_chunk_isolated(
    mut ids: Vec<String>,
    cwd: &Path,
    mut exec: impl FnMut(&[String]) -> ChunkReport,
) -> ChunkReport {
    let mut merged: Option<ChunkReport> = None;
    loop {
        let mut report = exec(&ids);
        let mut retry = false;
        if matches!(report.code, 2 | 4) {
            let broken: Vec<PathBuf> = report
                .failed_ids
                .iter()
                .filter(|id| !id.contains("::"))
                .map(|id| resolve_reported(id, cwd))
                .collect();
            let before = ids.len();
            if !broken.is_empty() {
                ids.retain(|id| {
                    let file = resolve_reported(id_file(id), cwd);
                    !broken.contains(&file)
                });
            }
            if ids.len() < before {
                // Attributed to specific files: those count as failures,
                // and the remaining tests still get their run.
                report.code = 1;
                retry = !ids.is_empty();
            }
        }
        if !matches!(report.code, 0 | 1 | 5) {
            report.unverified = ids.clone();
        }
        merged = Some(match merged {
            None => report,
            Some(mut acc) => {
                acc.failed |= report.failed;
                acc.code = combine_codes(acc.code, report.code);
                acc.counts.add(report.counts);
                acc.failed_ids.extend(report.failed_ids);
                acc.unverified.extend(report.unverified);
                if let Some(output) = report.output {
                    acc.output.get_or_insert_with(String::new).push_str(&output);
                }
                acc
            }
        });
        if !retry {
            return merged.expect("at least one attempt");
        }
    }
}

/// Fan chunks out across `states.len()` threads; `exec` runs one chunk with
/// the thread's state. Handles `--maxfail` and aggregates the outcome.
pub(crate) fn schedule<S: Send>(
    chunks: Vec<Vec<String>>,
    states: Vec<S>,
    maxfail: usize,
    cwd: &Path,
    exec: &(dyn Fn(&mut S, &[String]) -> ChunkReport + Sync),
) -> Outcome {
    struct Acc {
        failed: usize,
        skipped: usize,
        counts: Counts,
        failed_ids: Vec<String>,
        outputs: Vec<String>,
        code: i32,
        unverified: Vec<String>,
    }
    let total = chunks.len();
    let queue = Mutex::new(VecDeque::from(chunks));
    let acc = Mutex::new(Acc {
        failed: 0,
        skipped: 0,
        counts: Counts::default(),
        failed_ids: Vec::new(),
        outputs: Vec::new(),
        code: 5,
        unverified: Vec::new(),
    });
    let start = Instant::now();
    std::thread::scope(|scope| {
        for mut state in states {
            let (queue, acc) = (&queue, &acc);
            scope.spawn(move || loop {
                let Some(ids) = queue.lock().expect("queue lock").pop_front() else {
                    break;
                };
                let report = run_chunk_isolated(ids, cwd, |ids| exec(&mut state, ids));
                let mut acc = acc.lock().expect("outcome lock");
                acc.counts.add(report.counts);
                acc.code = combine_codes(acc.code, report.code);
                if report.failed {
                    acc.failed += 1;
                }
                acc.failed_ids.extend(report.failed_ids);
                acc.unverified.extend(report.unverified);
                if let Some(output) = report.output {
                    acc.outputs.push(output);
                }
                if maxfail > 0 && acc.counts.failed as usize >= maxfail {
                    let mut queue = queue.lock().expect("queue lock");
                    acc.skipped += queue.len();
                    for ids in queue.drain(..) {
                        acc.unverified.extend(ids);
                    }
                }
            });
        }
    });
    let acc = acc.into_inner().expect("outcome lock");
    Outcome {
        chunks: total,
        failed: acc.failed,
        skipped_chunks: acc.skipped,
        seconds: start.elapsed().as_secs_f64(),
        counts: acc.counts,
        failed_ids: acc.failed_ids,
        failure_output: acc.outputs,
        exit_code: acc.code,
        unverified: acc.unverified,
    }
}

/// Fan chunks out across fresh `python -m pytest` subprocesses, run from
/// `cwd` (pytest prints node IDs relative to it).
#[allow(clippy::too_many_arguments)]
pub fn run(
    files: Vec<FileTests>,
    workers: usize,
    chunk_size: usize,
    python: &str,
    maxfail: usize,
    extra_args: &[String],
    coverage_base: Option<&str>,
    cwd: &Path,
) -> Outcome {
    let chunks = make_chunks(&files, chunk_size);
    let chunk_seq = AtomicUsize::new(0);
    let exec = |_: &mut (), ids: &[String]| {
        let mut command = Command::new(python);
        command
            .args(["-m", "pytest", "-q", "--no-header", "-rfE"])
            .args(extra_args)
            .args(ids)
            .current_dir(cwd);
        if let Some(base) = coverage_base {
            // Unique per chunk so parallel pytest-cov runs never
            // clobber each other; combined after the run.
            let seq = chunk_seq.fetch_add(1, Ordering::Relaxed);
            command.env("COVERAGE_FILE", format!("{base}.{seq}"));
        }
        match command.output() {
            Ok(out) => report_chunk(
                out.status.code(),
                &String::from_utf8_lossy(&out.stdout),
                &String::from_utf8_lossy(&out.stderr),
            ),
            Err(err) => ChunkReport::crashed(format!("cito: failed to spawn {python}: {err}\n")),
        }
    };
    schedule(chunks, vec![(); workers.max(1)], maxfail, cwd, &exec)
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

    #[test]
    fn exit_codes_combine_by_severity() {
        assert_eq!(combine_codes(5, 5), 5);
        assert_eq!(combine_codes(5, 0), 0);
        assert_eq!(combine_codes(0, 5), 0);
        assert_eq!(combine_codes(0, 1), 1);
        assert_eq!(combine_codes(1, 5), 1);
        assert_eq!(combine_codes(2, 1), 2);
        assert_eq!(combine_codes(3, 4), 4);
        assert_eq!(combine_codes(4, 1), 4);
    }

    #[test]
    fn reported_ids_become_rootdir_relative() {
        let root = Path::new("/nonexistent-cito-root/proj");
        assert_eq!(
            rootdir_relative_id("../tests/a.py::test_x", &root.join("sub"), root),
            "tests/a.py::test_x"
        );
        assert_eq!(
            rootdir_relative_id("a.py::T::test[x::y]", root, root),
            "a.py::T::test[x::y]"
        );
        assert_eq!(
            rootdir_relative_id("tests/bad.py", root, root),
            "tests/bad.py"
        );
        assert_eq!(
            rootdir_relative_id("/elsewhere/a.py::t", root, root),
            "/elsewhere/a.py::t"
        );
    }

    #[test]
    fn collection_errors_are_isolated() {
        let cwd = Path::new("/nonexistent-cito-root");
        let ids = vec![
            "/nonexistent-cito-root/bad.py::test_a".to_string(),
            "/nonexistent-cito-root/ok.py::test_ok".to_string(),
        ];
        let mut calls = Vec::new();
        let report = run_chunk_isolated(ids, cwd, |ids| {
            calls.push(ids.to_vec());
            if ids.len() == 2 {
                report_chunk(Some(4), "ERROR bad.py\n1 error in 0.01s\n", "")
            } else {
                report_chunk(Some(0), "1 passed in 0.01s\n", "")
            }
        });
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1], vec!["/nonexistent-cito-root/ok.py::test_ok"]);
        assert_eq!(report.code, 1);
        assert!(report.failed);
        assert_eq!(report.failed_ids, vec!["bad.py"]);
        assert_eq!((report.counts.passed, report.counts.failed), (1, 1));
        assert!(report.unverified.is_empty());
    }

    #[test]
    fn unattributable_aborts_leave_chunk_unverified() {
        let cwd = Path::new("/nonexistent-cito-root");
        let ids = vec!["/nonexistent-cito-root/a.py::t".to_string()];
        let report = run_chunk_isolated(ids.clone(), cwd, |_| {
            report_chunk(Some(3), "INTERNALERROR> boom\n", "")
        });
        assert_eq!(report.code, 3);
        assert_eq!(report.unverified, ids);
    }
}
