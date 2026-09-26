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

/// pytest's `KeywordMatcher.from_item` name set for one test: every node
/// name up the chain except the Session and the rootdir Directory (so each
/// rootdir-relative directory, the file basename, each class, and the item
/// name with its parameter suffix), plus the test's marker names and the
/// `pytestmark` attribute of directly-marked functions. Lowercased.
fn keyword_names(file: &collector::FileTests, test: &str) -> Vec<String> {
    let mut names: Vec<String> = file
        .path
        .split('/')
        .filter(|part| !part.is_empty())
        .map(str::to_lowercase)
        .collect();
    let (base, params) = match test.find('[') {
        Some(i) => test.split_at(i),
        None => (test, ""),
    };
    let mut parts: Vec<&str> = base.split("::").collect();
    let item = parts.pop().unwrap_or_default();
    names.extend(parts.iter().map(|p| p.to_lowercase()));
    names.push(format!("{item}{params}").to_lowercase());
    if let Some(words) = file.keywords.get(base) {
        names.extend(words.iter().map(|w| w.to_lowercase()));
    }
    names
}

/// `-k` filtering: each fragment must be a substring of one keyword name.
fn apply_keyword(files: &mut [collector::FileTests], expr: &keyword::KExpr) {
    for file in files.iter_mut() {
        let keep: Vec<bool> = file
            .tests
            .iter()
            .map(|t| expr.matches(&keyword_names(file, t)))
            .collect();
        let mut keep = keep.into_iter();
        file.tests.retain(|_| keep.next().unwrap_or(false));
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

/// pytest's `get_dirs_from_args`: absolutized argument paths that exist,
/// files replaced by their parent directory.
fn dirs_from_args(paths: &[PathBuf], cwd: &Path) -> Vec<PathBuf> {
    paths
        .iter()
        .filter_map(|path| {
            let abs = if path.is_absolute() {
                path.clone()
            } else {
                cwd.join(path)
            };
            if !abs.exists() {
                return None;
            }
            if abs.is_file() {
                abs.parent().map(Path::to_path_buf)
            } else {
                Some(abs)
            }
        })
        .collect()
}

struct Collected {
    files: Vec<collector::FileTests>,
    roots: Vec<PathBuf>,
    config: Config,
}

/// CLI `-m` wins over an `-m` inside config addopts (pytest prepends
/// addopts, so a later CLI flag overrides it).
fn collect_files(
    paths: Vec<PathBuf>,
    probe_python: Option<&str>,
    marker_cli: Option<String>,
    ignore: &[PathBuf],
) -> Result<Collected, String> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let (root_args, selectors) = parse_selections(paths, &cwd);
    let arg_dirs = dirs_from_args(&root_args, &cwd);
    let config = Config::discover_for(&cwd, &arg_dirs);
    if let Some(err) = &config.error {
        return Err(err.clone());
    }
    let marker = marker_cli
        .or_else(|| config.addopts_flag("-m"))
        .map(|m| keyword::parse_marker_option(&m))
        .transpose()
        .map_err(|e| format!("invalid -m expression: {e}"))?
        .flatten();
    let roots = resolve_roots(root_args, &config, &cwd);
    // addopts `--ignore` entries apply alongside the CLI ones.
    let addopts_ignore: Vec<PathBuf> = config
        .addopts_values("--ignore")
        .into_iter()
        .map(PathBuf::from)
        .collect();
    let ignored: Vec<PathBuf> = ignore
        .iter()
        .chain(addopts_ignore.iter())
        .map(|p| {
            let abs = if p.is_absolute() {
                p.clone()
            } else {
                cwd.join(p)
            };
            abs.canonicalize().unwrap_or(abs)
        })
        .collect();
    let mut files = collector::collect(&roots, &config, probe_python, marker.as_ref());
    if !ignored.is_empty() {
        files.retain(|f| !ignored.iter().any(|ig| f.abs_path.starts_with(ig)));
    }
    apply_selectors(&mut files, &selectors);
    Ok(Collected {
        files,
        roots,
        config,
    })
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

fn hashes_path(config: &Config) -> PathBuf {
    config.rootdir.join(".cito").join("hashes")
}

fn read_hashes(config: &Config) -> std::collections::HashMap<String, String> {
    std::fs::read_to_string(hashes_path(config))
        .map(|text| {
            text.lines()
                .filter_map(|l| l.split_once('\t'))
                .map(|(h, p)| (p.to_string(), h.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn file_hash(path: &Path) -> Option<String> {
    use std::hash::{Hash, Hasher};
    let bytes = std::fs::read(path).ok()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    Some(format!("{:016x}", hasher.finish()))
}

/// Merge content hashes for verified files into the cache and forget the
/// hashes in `removals`, so those files count as changed next time.
fn write_hashes(
    config: &Config,
    updates: &std::collections::HashMap<String, String>,
    removals: &std::collections::HashSet<String>,
) {
    let mut merged = read_hashes(config);
    merged.extend(updates.clone());
    merged.retain(|path, _| !removals.contains(path));
    let path = hashes_path(config);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut body: Vec<String> = merged.iter().map(|(p, h)| format!("{h}\t{p}")).collect();
    body.sort();
    let _ = std::fs::write(&path, body.join("\n") + "\n");
}

/// Did `candidate` (rootdir-relative `path::test`) fail last time? Entries
/// are rootdir-relative node IDs. An entry naming a file (a collection
/// error) or a class covers every test beneath it, and a bare candidate
/// covers its bracketed parametrizations. Matching respects path
/// boundaries: `test_a.py::t` never matches `sub/test_a.py::t`.
fn matches_failure(candidate: &str, entry: &str) -> bool {
    candidate == entry
        || candidate
            .strip_prefix(entry)
            .is_some_and(|rest| rest.starts_with("::"))
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

/// Failed-first, then content-changed-since-last-run, then most-recently
/// modified.
fn order_files(
    files: Vec<collector::FileTests>,
    previous: &[String],
    changed: &std::collections::HashSet<String>,
) -> Vec<collector::FileTests> {
    let mut keyed: Vec<(
        bool,
        bool,
        std::cmp::Reverse<SystemTime>,
        collector::FileTests,
    )> = files
        .into_iter()
        .map(|f| {
            let has_failure = !previous.is_empty() && file_has_failure(&f, previous);
            let mtime = f
                .abs_path
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            let is_changed = changed.contains(&f.path);
            (!has_failure, !is_changed, std::cmp::Reverse(mtime), f)
        })
        .collect();
    keyed.sort_by_key(|entry| (entry.0, entry.1, entry.2));
    keyed.into_iter().map(|(_, _, _, f)| f).collect()
}

fn filter_lastfailed(files: &mut [collector::FileTests], previous: &[String]) {
    for file in files.iter_mut() {
        file.tests.retain(|t| {
            let candidate = format!("{}::{}", file.path, t);
            previous.iter().any(|p| matches_failure(&candidate, p))
        });
    }
}

/// Merge this run's failures into the cache: entries covering a test that
/// just ran to completion (`ran`, rootdir-relative node IDs) are replaced by
/// this run's results; everything else (other files, tests skipped by
/// --maxfail) is preserved.
fn write_lastfailed(config: &Config, previous: &[String], ran: &[String], new_failed: &[String]) {
    let mut merged: Vec<String> = previous
        .iter()
        .filter(|entry| {
            !ran.iter()
                .any(|candidate| matches_failure(candidate, entry))
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

#[allow(clippy::too_many_arguments)]
pub fn collect(
    paths: Vec<PathBuf>,
    json: bool,
    count: bool,
    python: Option<String>,
    kexpr: Option<String>,
    marker: Option<String>,
    ignore: Vec<PathBuf>,
) -> ExitCode {
    let Collected {
        mut files, config, ..
    } = match collect_files(paths, python.as_deref(), marker, &ignore) {
        Ok(collected) => collected,
        Err(err) => {
            eprintln!("cito: {err}");
            return ExitCode::FAILURE;
        }
    };
    let kexpr = kexpr
        .or_else(|| config.addopts_flag("-k"))
        .map(|k| keyword::parse_keyword_option(&k));
    let kexpr = match kexpr.transpose() {
        Ok(expr) => expr.flatten(),
        Err(err) => {
            eprintln!("cito: invalid -k expression: {err}");
            return ExitCode::FAILURE;
        }
    };
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

fn print_summary(outcome: &runner::Outcome, stdout_reserved: bool) {
    // With --json, stdout carries only the machine-readable summary.
    for output in &outcome.failure_output {
        if stdout_reserved {
            eprint!("{output}");
        } else {
            print!("{output}");
        }
    }
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
    daemon: bool,
    /// Only run tests impacted by changes since the last run (AST-level:
    /// own file, conftest chain, config, or transitive project imports).
    changed_only: bool,
    /// `--json`: stdout is reserved for the summary object.
    json: bool,
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
    let cwd = std::env::current_dir().unwrap_or_else(|_| config.rootdir.clone());
    let previous = read_lastfailed(config);
    let stored_hashes = read_hashes(config);
    // Under --changed, impact analysis hashes every file in each test
    // file's dependency closure: a test is impacted when anything it
    // depends on is new, modified, or gone since the last run. Plain runs
    // hash only the test files themselves (a dep without a recorded
    // baseline counts as changed on the next --changed run — conservative
    // in the safe direction).
    let mut closures = std::collections::HashMap::new();
    let (changed, current_hashes) = if options.changed_only {
        closures = collector::impact_closures(config, &files);
        let mut current: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for dep in closures.values().flatten() {
            if !current.contains_key(dep) {
                if let Some(hash) = file_hash(&config.rootdir.join(dep)) {
                    current.insert(dep.clone(), hash);
                }
            }
        }
        let changed: std::collections::HashSet<String> = files
            .iter()
            .filter(|f| {
                closures.get(&f.path).is_none_or(|deps| {
                    deps.iter()
                        .any(|dep| current.get(dep) != stored_hashes.get(dep))
                })
            })
            .map(|f| f.path.clone())
            .collect();
        (changed, current)
    } else {
        let current: std::collections::HashMap<String, String> = files
            .iter()
            .filter(|f| !f.tests.is_empty())
            .filter_map(|f| file_hash(&f.abs_path).map(|h| (f.path.clone(), h)))
            .collect();
        let changed: std::collections::HashSet<String> = current
            .iter()
            .filter(|(p, h)| stored_hashes.get(*p) != Some(*h))
            .map(|(p, _)| p.clone())
            .collect();
        (changed, current)
    };
    let mut files = files;
    if options.changed_only {
        if stored_hashes.is_empty() {
            eprintln!("cito: no previous run recorded; running everything");
        } else {
            for file in files.iter_mut() {
                if !changed.contains(&file.path) {
                    file.tests.clear();
                }
            }
        }
    }
    let files = order_files(files, &previous, &changed);
    // (rootdir-relative path, absolute path, tests) of every file about to
    // run, for cache bookkeeping once the runners consumed `files`.
    let scheduled: Vec<(String, String, Vec<String>)> = files
        .iter()
        .filter(|f| !f.tests.is_empty())
        .map(|f| {
            (
                f.path.clone(),
                f.abs_path.to_string_lossy().into_owned(),
                f.tests.clone(),
            )
        })
        .collect();
    // Impacted by a change but filtered out (-k, --lf, selectors): their
    // closure was not verified, so they must stay "changed".
    let impacted_unrun: Vec<String> = files
        .iter()
        .filter(|f| f.tests.is_empty() && changed.contains(&f.path))
        .map(|f| f.path.clone())
        .collect();
    let total: usize = files.iter().map(|f| f.tests.len()).sum();
    let mode = if options.daemon {
        "daemon"
    } else if pool.is_some() {
        "warm"
    } else {
        "subprocess"
    };
    eprintln!(
        "cito: running {total} tests across {} {mode} workers",
        options.workers
    );
    let outcome = 'exec: {
        #[cfg(unix)]
        if options.daemon {
            if let Some(outcome) = crate::daemon::run(
                &config.rootdir,
                &files,
                &options.python,
                options.workers,
                options.chunk,
                options.maxfail,
                &options.extra_args,
                &options.coverage_base,
                &cwd,
            ) {
                break 'exec outcome;
            }
            eprintln!("cito: daemon unreachable; falling back to local workers");
        }
        match pool {
            Some(pool) => pool.run(
                files,
                options.chunk,
                options.maxfail,
                purge,
                &options.extra_args,
                Some(&options.coverage_base),
                &config.rootdir,
            ),
            None => runner::run(
                files,
                options.workers,
                options.chunk,
                &options.python,
                options.maxfail,
                &options.extra_args,
                Some(&options.coverage_base),
                &cwd,
            ),
        }
    };
    let mut outcome = outcome;
    // pytest prints IDs relative to its working directory; the caches and
    // matching speak rootdir-relative IDs.
    for id in outcome.failed_ids.iter_mut() {
        *id = runner::rootdir_relative_id(id, &cwd, &config.rootdir);
    }
    print_summary(&outcome, options.json);
    combine_coverage(config, options);
    if outcome.skipped_chunks > 0 {
        eprintln!(
            "cito: stopped early (--maxfail): {} chunk(s) not run",
            outcome.skipped_chunks
        );
    }
    // A file is verified only when every one of its tests ran and passed.
    let unverified: std::collections::HashSet<&str> = outcome
        .unverified
        .iter()
        .map(|id| runner::id_file(id))
        .collect();
    let failed_files: std::collections::HashSet<&str> = outcome
        .failed_ids
        .iter()
        .map(|id| runner::id_file(id))
        .collect();
    let mut ran: Vec<String> = Vec::new();
    let mut verified: Vec<&str> = Vec::new();
    let mut dirty: std::collections::HashSet<String> = impacted_unrun.into_iter().collect();
    for (path, abs, tests) in &scheduled {
        if unverified.contains(abs.as_str()) {
            // Partially run at best (--maxfail, crashed chunk): record only
            // the tests whose chunk completed.
            let skipped: std::collections::HashSet<&str> = outcome
                .unverified
                .iter()
                .filter(|id| runner::id_file(id) == abs)
                .filter_map(|id| id.split_once("::").map(|(_, t)| t))
                .collect();
            ran.extend(
                tests
                    .iter()
                    .filter(|t| !skipped.contains(t.as_str()))
                    .map(|t| format!("{path}::{t}")),
            );
            dirty.insert(path.clone());
        } else {
            ran.extend(tests.iter().map(|t| format!("{path}::{t}")));
            if failed_files.contains(path.as_str()) {
                dirty.insert(path.clone());
            } else {
                verified.push(path);
            }
        }
    }
    write_lastfailed(config, &previous, &ran, &outcome.failed_ids);
    // Hashes record "known good": only verified files contribute (their own
    // hash, or under --changed their whole dependency closure). Failed or
    // unverified files forget their own hash so the next --changed run
    // picks them up again even when nothing else changed.
    let mut updates: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut record = |dep: &str| {
        if let Some(hash) = current_hashes.get(dep) {
            updates.insert(dep.to_string(), hash.clone());
        }
    };
    for path in verified {
        match closures.get(path) {
            Some(deps) => deps.iter().for_each(|dep| record(dep)),
            None => record(path),
        }
    }
    write_hashes(config, &updates, &dirty);
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
    marker: Option<String>,
    ignore: Vec<PathBuf>,
    changed_only: bool,
    use_daemon: bool,
) -> ExitCode {
    let watch_args = (paths.clone(), marker.clone());
    let Collected {
        mut files,
        roots,
        config,
    } = match collect_files(paths, Some(&python), marker, &ignore) {
        Ok(collected) => collected,
        Err(err) => {
            eprintln!("cito: {err}");
            return ExitCode::FAILURE;
        }
    };
    let kexpr = kexpr
        .or_else(|| config.addopts_flag("-k"))
        .map(|k| keyword::parse_keyword_option(&k));
    let kexpr = match kexpr.transpose() {
        Ok(expr) => expr.flatten(),
        Err(err) => {
            eprintln!("cito: invalid -k expression: {err}");
            return ExitCode::FAILURE;
        }
    };
    if config
        .addopts
        .iter()
        .any(|a| a == "-n" || a.starts_with("--numprocesses") || a == "--dist")
    {
        eprintln!(
            "cito: warning: addopts contains pytest-xdist options (-n/--dist); \
             each cito chunk will nest its own xdist workers — consider removing \
             them for cito runs"
        );
    }
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
    if use_daemon && !cfg!(unix) {
        eprintln!("cito: --daemon is only supported on unix platforms");
        return ExitCode::FAILURE;
    }
    let options = RunOptions {
        workers: workers.unwrap_or_else(num_cpus::get),
        chunk,
        python,
        maxfail,
        extra_args: pytest_args,
        coverage_base: config.rootdir.join(".coverage.cito").display().to_string(),
        daemon: use_daemon,
        changed_only,
        json,
    };
    let pool = (warm_workers && !use_daemon)
        .then(|| WarmPool::new(&options.python, options.workers, Default::default()));
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
                "exit_code": exit_code(collected, &outcome),
            })
        );
    }
    if watch {
        let (watch_paths, watch_marker) = watch_args;
        let recollect = || -> Vec<collector::FileTests> {
            match collect_files(
                watch_paths.clone(),
                Some(&options.python),
                watch_marker.clone(),
                &ignore,
            ) {
                Ok(collected) => {
                    let mut files = collected.files;
                    if let Some(expr) = &kexpr {
                        apply_keyword(&mut files, expr);
                    }
                    files
                }
                Err(err) => {
                    eprintln!("cito: {err}");
                    Vec::new()
                }
            }
        };
        return watch_loop(&config, &roots, &options, pool.as_ref(), &recollect);
    }
    ExitCode::from(exit_code(collected, &outcome))
}

/// pytest-compatible exit status: 5 when nothing was collected, 0 when
/// nothing needed running (e.g. `--changed` with no changes), otherwise the
/// chunks' combined code (1 test failures, 2 interrupted, 3 internal
/// error, 4 usage error).
fn exit_code(collected: usize, outcome: &runner::Outcome) -> u8 {
    if collected == 0 {
        5
    } else if outcome.chunks == 0 {
        0
    } else {
        u8::try_from(outcome.exit_code).unwrap_or(1)
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

/// Config files whose change can alter every test's behavior.
const CONFIG_NAMES: &[&str] = &[
    "pytest.ini",
    ".pytest.ini",
    "pyproject.toml",
    "tox.ini",
    "setup.cfg",
];

/// Forward-slash path of `path` relative to `root` (the `FileTests::path`
/// convention), if it lies inside it.
fn root_relative(path: &Path, root: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    Some(parts.join("/"))
}

fn is_noise(path: &Path) -> bool {
    path.components().any(|c| {
        matches!(
            c.as_os_str().to_str(),
            Some(
                ".git"
                    | "target"
                    | ".cito"
                    | "__pycache__"
                    | ".pytest_cache"
                    | ".venv"
                    | "venv"
                    | ".tox"
                    | "node_modules"
            )
        )
    })
}

/// Rerun whatever a batch of file changes affects: a config change reruns
/// everything; a `.py` change (test, helper, or conftest) reruns every test
/// file whose impact closure — the same import graph `--changed` uses —
/// contains it.
fn watch_loop(
    config: &Config,
    roots: &[PathBuf],
    options: &RunOptions,
    pool: Option<&WarmPool>,
    recollect: &dyn Fn() -> Vec<collector::FileTests>,
) -> ExitCode {
    use notify::{RecursiveMode, Watcher};

    let root = config
        .rootdir
        .canonicalize()
        .unwrap_or_else(|_| config.rootdir.clone());
    let config_source = config
        .source
        .as_ref()
        .map(|s| s.canonicalize().unwrap_or_else(|_| s.clone()));

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
    let mut recursive: Vec<PathBuf> = Vec::new();
    for target in roots {
        let target = if target.is_file() {
            target.parent().unwrap_or(Path::new(".")).to_path_buf()
        } else {
            target.clone()
        };
        let target = target.canonicalize().unwrap_or(target);
        if let Err(err) = watcher.watch(&target, RecursiveMode::Recursive) {
            eprintln!("cito: cannot watch {}: {err}", target.display());
        }
        recursive.push(target);
    }
    // Helpers and config can live outside the test roots: also watch the
    // rootdir itself and every directory holding a dependency, shallowly.
    let mut shallow: BTreeSet<PathBuf> = BTreeSet::new();
    shallow.insert(root.clone());
    let initial = recollect();
    for deps in collector::impact_closures(config, &initial).values() {
        for dep in deps {
            if let Some(dir) = root.join(dep).parent() {
                shallow.insert(dir.to_path_buf());
            }
        }
    }
    for dir in shallow {
        if !recursive.iter().any(|r| dir.starts_with(r)) {
            let _ = watcher.watch(&dir, RecursiveMode::NonRecursive);
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
        // Judge noise below the rootdir only: a project may itself live
        // under a directory called `target` or `venv`.
        let changed: Vec<PathBuf> = changed
            .into_iter()
            .filter(|p| !is_noise(p.strip_prefix(&root).unwrap_or(p)))
            .collect();
        let config_changed = changed.iter().any(|p| {
            config_source.as_ref() == Some(p)
                || (p.parent() == Some(root.as_path())
                    && p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| CONFIG_NAMES.contains(&n)))
        });
        // Deleted modules matter too, so no is_file() filter here.
        let changed_py: Vec<PathBuf> = changed
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "py") && !p.is_dir())
            .collect();
        if changed_py.is_empty() && !config_changed {
            continue;
        }
        // Warm workers drop every project module once anything changed;
        // the explicit list also covers files they never imported.
        pending_purge.extend(changed_py.iter().map(|p| p.to_string_lossy().into_owned()));
        pending_purge.sort();
        pending_purge.dedup();

        let mut files = recollect();
        if config_changed {
            eprintln!("cito: config changed; rerunning everything");
        } else {
            let changed_rel: std::collections::HashSet<String> = changed_py
                .iter()
                .filter_map(|p| root_relative(p, &root))
                .collect();
            let closures = collector::impact_closures(config, &files);
            let mut affected = 0usize;
            for file in files.iter_mut() {
                let hit = closures
                    .get(&file.path)
                    .is_some_and(|deps| deps.iter().any(|d| changed_rel.contains(d)))
                    || changed_rel.contains(&file.path);
                if hit {
                    affected += 1;
                } else {
                    file.tests.clear();
                }
            }
            if affected == 0 {
                continue;
            }
            eprintln!(
                "cito: change detected ({} file(s)); rerunning {affected} affected test file(s)",
                changed_rel.len()
            );
        }
        run_once(files, config, options, pool, &pending_purge);
        pending_purge.clear();
        eprintln!("cito: watching for changes (Ctrl-C to stop)");
    }
}

fn daemon_rootdir() -> Config {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    Config::discover(&cwd)
}

pub fn daemon_command(action: &str) -> ExitCode {
    #[cfg(not(unix))]
    {
        let _ = action;
        eprintln!("cito: the daemon is only supported on unix platforms");
        ExitCode::FAILURE
    }
    #[cfg(unix)]
    {
        let config = daemon_rootdir();
        match action {
            "serve" => match crate::daemon::serve(&config.rootdir) {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("cito: daemon failed: {err}");
                    ExitCode::FAILURE
                }
            },
            "start" => match crate::daemon::ensure(&config.rootdir) {
                Some(path) => {
                    eprintln!("cito: daemon ready at {}", path.display());
                    ExitCode::SUCCESS
                }
                None => {
                    eprintln!("cito: failed to start daemon");
                    ExitCode::FAILURE
                }
            },
            "stop" => {
                if crate::daemon::stop(&config.rootdir) {
                    eprintln!("cito: daemon stopped");
                } else {
                    eprintln!("cito: no daemon running");
                }
                ExitCode::SUCCESS
            }
            "status" => {
                match crate::daemon::status(&config.rootdir) {
                    Some(version) => eprintln!("cito: daemon running (v{version})"),
                    None => eprintln!("cito: no daemon running"),
                }
                ExitCode::SUCCESS
            }
            _ => ExitCode::FAILURE,
        }
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
        // No suffix matching across path boundaries.
        assert!(!matches_failure(
            "pkg/tests/a.py::test_x",
            "tests/a.py::test_x"
        ));
        assert!(!matches_failure(
            "sub/test_a.py::test_x",
            "test_a.py::test_x"
        ));
        assert!(!matches_failure("tests/a.py::test_y", "tests/a.py::test_x"));
        assert!(!matches_failure(
            "tests/a.py::test_xy",
            "tests/a.py::test_x"
        ));
        // File-level (collection error) and class-level entries cover
        // everything beneath them.
        assert!(matches_failure("tests/bad.py::test_a", "tests/bad.py"));
        assert!(matches_failure(
            "tests/a.py::TestK::test_y",
            "tests/a.py::TestK"
        ));
        assert!(!matches_failure("tests/bad.py2::test_a", "tests/bad.py"));
    }
}
