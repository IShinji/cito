use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use crate::config::Config;
use crate::warm::WarmPool;
use crate::{collector, keyword, runner};

// ---------------------------------------------------------------------------
// Argument handling: paths, testpaths, node-ID selectors
// ---------------------------------------------------------------------------

/// A `path::Class::test` argument: restrict that file to matching tests.
struct Selector {
    file: PathBuf,
    test: String,
}

/// Split CLI args into plain paths and node-ID selectors.
fn parse_selections(paths: Vec<PathBuf>, cwd: &Path) -> (Vec<PathBuf>, Vec<Selector>) {
    let mut roots = Vec::new();
    let mut selectors = Vec::new();
    for arg in paths {
        let text = arg.to_string_lossy().into_owned();
        match text.split_once("::") {
            Some((file, test)) if !test.is_empty() => {
                let path = PathBuf::from(file);
                let abs = if path.is_absolute() {
                    path.clone()
                } else {
                    cwd.join(&path)
                };
                let abs = abs.canonicalize().unwrap_or(abs);
                selectors.push(Selector {
                    file: abs,
                    test: test.to_string(),
                });
                roots.push(path);
            }
            _ => roots.push(arg),
        }
    }
    (roots, selectors)
}

/// `TestX` selects `TestX`, `TestX::test_y`, and `TestX[param]` alike.
fn selector_matches(test: &str, selector: &str) -> bool {
    test == selector
        || test
            .strip_prefix(selector)
            .is_some_and(|rest| rest.starts_with("::") || rest.starts_with('['))
}

fn apply_selectors(files: &mut [collector::FileTests], selectors: &[Selector]) {
    if selectors.is_empty() {
        return;
    }
    for file in files.iter_mut() {
        let own: Vec<&Selector> = selectors
            .iter()
            .filter(|s| s.file == file.abs_path)
            .collect();
        if own.is_empty() {
            continue;
        }
        file.tests
            .retain(|t| own.iter().any(|s| selector_matches(t, &s.test)));
    }
}

/// `-k` filtering against `basename::testid`, pytest-style approximation.
fn apply_keyword(files: &mut [collector::FileTests], expr: &keyword::KExpr) {
    for file in files.iter_mut() {
        let basename = file
            .path
            .rsplit('/')
            .next()
            .unwrap_or(&file.path)
            .to_string();
        file.tests.retain(|t| {
            let candidate = format!("{basename}::{t}").to_lowercase();
            expr.matches(&candidate)
        });
    }
}

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

struct Collected {
    files: Vec<collector::FileTests>,
    roots: Vec<PathBuf>,
    config: Config,
}

fn collect_files(paths: Vec<PathBuf>, probe_python: Option<&str>) -> Collected {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let (root_args, selectors) = parse_selections(paths, &cwd);
    let anchor = discovery_anchor(&root_args, &cwd);
    let config = Config::discover(&anchor);
    let roots = resolve_roots(root_args, &config, &cwd);
    let mut files = collector::collect(&roots, &config, probe_python);
    apply_selectors(&mut files, &selectors);
    Collected {
        files,
        roots,
        config,
    }
}

// ---------------------------------------------------------------------------
// Last-failed cache and scheduling
// ---------------------------------------------------------------------------

fn lastfailed_path(config: &Config) -> PathBuf {
    config.rootdir.join(".cito").join("lastfailed")
}

fn read_lastfailed(config: &Config) -> Vec<String> {
    std::fs::read_to_string(lastfailed_path(config))
        .map(|text| {
            text.lines()
                .filter(|l| !l.trim().is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Did `candidate` (rootdir-relative `path::test`) fail last time? Bare
/// candidates also match their bracketed parametrizations.
fn matches_failure(candidate: &str, entry: &str) -> bool {
    candidate == entry
        || candidate.ends_with(entry)
        || entry
            .strip_prefix(candidate)
            .is_some_and(|rest| rest.starts_with('['))
}

fn file_has_failure(file: &collector::FileTests, previous: &[String]) -> bool {
    file.tests.iter().any(|t| {
        let candidate = format!("{}::{}", file.path, t);
        previous.iter().any(|p| matches_failure(&candidate, p))
    })
}

/// Failed-first, then most-recently-modified first.
fn order_files(files: Vec<collector::FileTests>, previous: &[String]) -> Vec<collector::FileTests> {
    let mut keyed: Vec<(bool, std::cmp::Reverse<SystemTime>, collector::FileTests)> = files
        .into_iter()
        .map(|f| {
            let has_failure = !previous.is_empty() && file_has_failure(&f, previous);
            let mtime = f
                .abs_path
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            (!has_failure, std::cmp::Reverse(mtime), f)
        })
        .collect();
    keyed.sort_by_key(|entry| (entry.0, entry.1));
    keyed.into_iter().map(|(_, _, f)| f).collect()
}

fn filter_lastfailed(files: &mut [collector::FileTests], previous: &[String]) {
    for file in files.iter_mut() {
        file.tests.retain(|t| {
            let candidate = format!("{}::{}", file.path, t);
            previous.iter().any(|p| matches_failure(&candidate, p))
        });
    }
}

/// Merge this run's failures into the cache: entries belonging to files that
/// were just rerun are replaced, everything else is preserved.
fn write_lastfailed(
    config: &Config,
    previous: &[String],
    rerun_files: &[String],
    new_failed: &[String],
) {
    let mut merged: Vec<String> = previous
        .iter()
        .filter(|entry| {
            !rerun_files
                .iter()
                .any(|path| entry.starts_with(&format!("{path}::")))
        })
        .cloned()
        .collect();
    for id in new_failed {
        if !merged.iter().any(|m| m == id) {
            merged.push(id.clone());
        }
    }
    let path = lastfailed_path(config);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
        // Cache directories ignore themselves, like .pytest_cache.
        let marker = dir.join(".gitignore");
        if !marker.exists() {
            let _ = std::fs::write(&marker, "*\n");
        }
    }
    let mut body = merged.join("\n");
    if !body.is_empty() {
        body.push('\n');
    }
    let _ = std::fs::write(&path, body);
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

pub fn collect(
    paths: Vec<PathBuf>,
    json: bool,
    count: bool,
    python: Option<String>,
    kexpr: Option<String>,
) -> ExitCode {
    let kexpr = match kexpr.as_deref().map(keyword::parse).transpose() {
        Ok(expr) => expr,
        Err(err) => {
            eprintln!("cito: invalid -k expression: {err}");
            return ExitCode::FAILURE;
        }
    };
    let Collected { mut files, .. } = collect_files(paths, python.as_deref());
    if let Some(expr) = &kexpr {
        apply_keyword(&mut files, expr);
    }
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

fn print_summary(outcome: &runner::Outcome) {
    eprintln!(
        "cito: {} passed, {} failed, {} skipped across {} chunk(s) ({} failed) in {:.2}s",
        outcome.counts.passed,
        outcome.counts.failed,
        outcome.counts.skipped,
        outcome.chunks,
        outcome.failed,
        outcome.seconds
    );
}

struct RunOptions {
    workers: usize,
    chunk: usize,
    python: String,
    maxfail: usize,
    extra_args: Vec<String>,
    coverage_base: String,
}

/// If any per-chunk coverage files were produced (the runners point
/// pytest-cov at `.coverage.cito.N`), merge them into the standard
/// `.coverage` so `coverage report` works as usual.
fn combine_coverage(config: &Config, options: &RunOptions) {
    let Ok(entries) = std::fs::read_dir(&config.rootdir) else {
        return;
    };
    let fragments: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(".coverage.cito."))
        })
        .collect();
    if fragments.is_empty() {
        return;
    }
    let status = std::process::Command::new(&options.python)
        .args(["-m", "coverage", "combine"])
        .args(&fragments)
        .current_dir(&config.rootdir)
        .output();
    match status {
        Ok(out) if out.status.success() => {
            eprintln!(
                "cito: combined {} coverage fragment(s) into .coverage",
                fragments.len()
            );
        }
        _ => eprintln!(
            "cito: warning: {} coverage fragment(s) left uncombined (is `coverage` installed?)",
            fragments.len()
        ),
    }
}

/// Order, run, report, and update the last-failed cache. Returns the outcome.
fn run_once(
    files: Vec<collector::FileTests>,
    config: &Config,
    options: &RunOptions,
    pool: Option<&WarmPool>,
    purge: &[String],
) -> runner::Outcome {
    let previous = read_lastfailed(config);
    let files = order_files(files, &previous);
    let rerun_files: Vec<String> = files
        .iter()
        .filter(|f| !f.tests.is_empty())
        .map(|f| f.path.clone())
        .collect();
    let total: usize = files.iter().map(|f| f.tests.len()).sum();
    let mode = if pool.is_some() { "warm" } else { "subprocess" };
    eprintln!(
        "cito: running {total} tests across {} {mode} workers",
        options.workers
    );
    let outcome = match pool {
        Some(pool) => pool.run(
            files,
            options.chunk,
            options.maxfail,
            purge,
            &options.extra_args,
            Some(&options.coverage_base),
        ),
        None => runner::run(
            files,
            options.workers,
            options.chunk,
            &options.python,
            options.maxfail,
            &options.extra_args,
            Some(&options.coverage_base),
        ),
    };
    print_summary(&outcome);
    combine_coverage(config, options);
    if outcome.skipped_chunks > 0 {
        eprintln!(
            "cito: stopped early (--maxfail): {} chunk(s) not run",
            outcome.skipped_chunks
        );
    }
    write_lastfailed(config, &previous, &rerun_files, &outcome.failed_ids);
    outcome
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    paths: Vec<PathBuf>,
    workers: Option<usize>,
    chunk: usize,
    python: String,
    warm_workers: bool,
    lf: bool,
    watch: bool,
    maxfail: usize,
    kexpr: Option<String>,
    json: bool,
    pytest_args: Vec<String>,
) -> ExitCode {
    let kexpr = match kexpr.as_deref().map(keyword::parse).transpose() {
        Ok(expr) => expr,
        Err(err) => {
            eprintln!("cito: invalid -k expression: {err}");
            return ExitCode::FAILURE;
        }
    };
    let Collected {
        mut files,
        roots,
        config,
    } = collect_files(paths, Some(&python));
    if let Some(expr) = &kexpr {
        apply_keyword(&mut files, expr);
    }
    if lf {
        let previous = read_lastfailed(&config);
        if previous.is_empty() {
            eprintln!("cito: no previously failed tests recorded; running everything");
        } else {
            filter_lastfailed(&mut files, &previous);
        }
    }
    let options = RunOptions {
        workers: workers.unwrap_or_else(num_cpus::get),
        chunk,
        python,
        maxfail,
        extra_args: pytest_args,
        coverage_base: config.rootdir.join(".coverage.cito").display().to_string(),
    };
    let pool = warm_workers.then(|| WarmPool::new(&options.python, options.workers));
    let collected: usize = files.iter().map(|f| f.tests.len()).sum();
    let outcome = run_once(files, &config, &options, pool.as_ref(), &[]);
    if json {
        println!(
            "{}",
            serde_json::json!({
                "collected": collected,
                "passed": outcome.counts.passed,
                "failed": outcome.counts.failed,
                "skipped": outcome.counts.skipped,
                "chunks": outcome.chunks,
                "failed_chunks": outcome.failed,
                "skipped_chunks": outcome.skipped_chunks,
                "seconds": outcome.seconds,
                "failed_ids": outcome.failed_ids,
            })
        );
    }
    if watch {
        return watch_loop(&config, &roots, &options, pool.as_ref(), kexpr.as_ref());
    }
    if collected == 0 {
        // pytest exit code 5: no tests were collected/ran.
        ExitCode::from(5)
    } else if outcome.failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

// ---------------------------------------------------------------------------
// Watch mode
// ---------------------------------------------------------------------------

fn note_event(event: Result<notify::Event, notify::Error>, changed: &mut BTreeSet<PathBuf>) {
    if let Ok(event) = event {
        for path in event.paths {
            let canonical = path.canonicalize().unwrap_or(path);
            changed.insert(canonical);
        }
    }
}

fn watch_loop(
    config: &Config,
    roots: &[PathBuf],
    options: &RunOptions,
    pool: Option<&WarmPool>,
    kexpr: Option<&keyword::KExpr>,
) -> ExitCode {
    use notify::{RecursiveMode, Watcher};

    let (tx, rx) = std::sync::mpsc::channel();
    let mut watcher = match notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    }) {
        Ok(watcher) => watcher,
        Err(err) => {
            eprintln!("cito: failed to start watcher: {err}");
            return ExitCode::FAILURE;
        }
    };
    for root in roots {
        let target = if root.is_file() {
            root.parent().unwrap_or(Path::new(".")).to_path_buf()
        } else {
            root.clone()
        };
        if let Err(err) = watcher.watch(&target, RecursiveMode::Recursive) {
            eprintln!("cito: cannot watch {}: {err}", target.display());
        }
    }
    eprintln!("cito: watching for changes (Ctrl-C to stop)");

    let mut pending_purge: Vec<String> = Vec::new();
    loop {
        let Ok(first) = rx.recv() else {
            return ExitCode::SUCCESS;
        };
        let mut changed = BTreeSet::new();
        note_event(first, &mut changed);
        // Debounce: absorb the burst an editor save produces.
        while let Ok(more) = rx.recv_timeout(Duration::from_millis(250)) {
            note_event(more, &mut changed);
        }
        let changed_py: Vec<PathBuf> = changed
            .into_iter()
            .filter(|p| p.is_file())
            .filter(|p| p.extension().is_some_and(|e| e == "py"))
            .filter(|p| {
                !p.components().any(|c| {
                    matches!(
                        c.as_os_str().to_str(),
                        Some(".git") | Some("target") | Some(".cito") | Some("__pycache__")
                    )
                })
            })
            .collect();
        if changed_py.is_empty() {
            continue;
        }
        // Warm workers must drop cached modules for every changed .py file,
        // not just test files (support modules go stale too).
        pending_purge.extend(changed_py.iter().map(|p| p.to_string_lossy().into_owned()));
        pending_purge.sort();
        pending_purge.dedup();
        let test_files: Vec<PathBuf> = changed_py
            .into_iter()
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| config.is_test_file(n, config.relative_to_root(p)))
            })
            .collect();
        if test_files.is_empty() {
            continue;
        }
        eprintln!(
            "cito: change detected in {} test file(s); rerunning",
            test_files.len()
        );
        let mut files = collector::collect(&test_files, config, Some(&options.python));
        if let Some(expr) = kexpr {
            apply_keyword(&mut files, expr);
        }
        run_once(files, config, options, pool, &pending_purge);
        pending_purge.clear();
        eprintln!("cito: watching for changes (Ctrl-C to stop)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selector_prefix_rules() {
        assert!(selector_matches("test_x", "test_x"));
        assert!(selector_matches("test_x[1]", "test_x"));
        assert!(selector_matches("TestA::test_y", "TestA"));
        assert!(!selector_matches("test_xy", "test_x"));
        assert!(!selector_matches("TestAB::test_y", "TestA"));
    }

    #[test]
    fn failure_matching_rules() {
        assert!(matches_failure("tests/a.py::test_x", "tests/a.py::test_x"));
        // Bare collected id covers its bracketed failures.
        assert!(matches_failure(
            "tests/a.py::test_x",
            "tests/a.py::test_x[1]"
        ));
        // Rootdir mismatch tolerated by suffix matching.
        assert!(matches_failure(
            "pkg/tests/a.py::test_x",
            "tests/a.py::test_x"
        ));
        assert!(!matches_failure("tests/a.py::test_y", "tests/a.py::test_x"));
    }
}
