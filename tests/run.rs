//! End-to-end tests for `cito run`: the built binary against copies of the
//! `tests/fixtures/run_*` trees, executed by the real pytest. Every test is
//! skipped (passes vacuously, with a note) when `python3 -m pytest` is not
//! available.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

const PYTHON: &str = if cfg!(windows) { "python" } else { "python3" };

fn have_pytest() -> bool {
    let ok = Command::new(PYTHON)
        .args(["-m", "pytest", "--version"])
        .output()
        .is_ok_and(|o| o.status.success());
    if !ok {
        eprintln!("skipping: `{PYTHON} -m pytest` is unavailable");
    }
    ok
}

/// A private copy of a fixture tree, removed on drop.
struct Tree(PathBuf);

impl Tree {
    fn new(fixture: &str) -> Tree {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let dest = std::env::temp_dir().join(format!(
            "cito-run-{fixture}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dest);
        let src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(fixture);
        copy_tree(&src, &dest);
        // Canonical, so paths compare cleanly on macOS (/var -> /private/var).
        Tree(dest.canonicalize().expect("fixture copy exists"))
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write(&self, rel: &str, body: &str) {
        let path = self.0.join(rel);
        std::fs::write(&path, body).expect("write fixture file");
        // Guarantee an observable mtime change even on coarse filesystems.
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(2);
        let file = std::fs::File::options()
            .write(true)
            .open(&path)
            .expect("open fixture file");
        let _ = file.set_modified(later);
    }

    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.0.join(rel)).unwrap_or_default()
    }

    fn cito(&self, args: &[&str]) -> Output {
        self.cito_in(&self.0, args, &[])
    }

    fn cito_in(&self, cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cito"));
        command
            .args(args)
            .current_dir(cwd)
            .env_remove("PYTEST_ADDOPTS")
            .env("PYTHONDONTWRITEBYTECODE", "1");
        for (key, value) in env {
            command.env(key, value);
        }
        command.output().expect("run cito")
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn copy_tree(src: &Path, dest: &Path) {
    std::fs::create_dir_all(dest).expect("create fixture copy");
    for entry in std::fs::read_dir(src).expect("read fixture") {
        let entry = entry.expect("fixture entry");
        let name = entry.file_name();
        if name == "__pycache__" || name == ".pytest_cache" || name == ".cito" {
            continue;
        }
        let target = dest.join(&name);
        if entry.file_type().expect("file type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy fixture file");
        }
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn all_output(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// The "cito: running N tests" count from a run's stderr.
fn ran(out: &Output) -> usize {
    let text = stderr(out);
    let line = text
        .lines()
        .find(|l| l.starts_with("cito: running "))
        .unwrap_or_else(|| panic!("no running line in:\n{text}"));
    line["cito: running ".len()..]
        .split_whitespace()
        .next()
        .and_then(|n| n.parse().ok())
        .expect("test count")
}

fn code(out: &Output) -> i32 {
    out.status.code().expect("exit code")
}

// ---------------------------------------------------------------------------
// --changed bookkeeping
// ---------------------------------------------------------------------------

#[test]
fn changed_keeps_rerunning_failures_until_fixed() {
    if !have_pytest() {
        return;
    }
    let tree = Tree::new("run_changed");
    let first = tree.cito(&["run", "-n", "1", "--changed"]);
    assert_eq!(code(&first), 0, "{}", all_output(&first));
    assert_eq!(ran(&first), 3);

    tree.write("helper.py", "VALUE = 2\n");
    let broken = tree.cito(&["run", "-n", "1", "--changed"]);
    assert_eq!(code(&broken), 1, "{}", all_output(&broken));
    assert_eq!(ran(&broken), 2, "both helper importers are impacted");

    // Nothing changed since, but test_uses never passed: it must run again.
    let again = tree.cito(&["run", "-n", "1", "--changed"]);
    assert_eq!(code(&again), 1, "{}", all_output(&again));
    assert_eq!(ran(&again), 1, "only the still-failing file reruns");

    tree.write("helper.py", "VALUE = 1\n");
    let fixed = tree.cito(&["run", "-n", "1", "--changed"]);
    assert_eq!(code(&fixed), 0, "{}", all_output(&fixed));
    assert_eq!(ran(&fixed), 2);

    let idle = tree.cito(&["run", "-n", "1", "--changed"]);
    assert_eq!(code(&idle), 0, "{}", all_output(&idle));
    assert_eq!(ran(&idle), 0);
}

#[test]
fn changed_does_not_record_files_filtered_out_by_k() {
    if !have_pytest() {
        return;
    }
    let tree = Tree::new("run_changed");
    let first = tree.cito(&["run", "-n", "1", "--changed"]);
    assert_eq!(code(&first), 0, "{}", all_output(&first));

    tree.write("helper.py", "VALUE = 3\n");
    let filtered = tree.cito(&["run", "-n", "1", "--changed", "-k", "also"]);
    assert_eq!(code(&filtered), 0, "{}", all_output(&filtered));
    assert_eq!(ran(&filtered), 1);

    // test_uses depends on the changed helper and has not run yet.
    let rest = tree.cito(&["run", "-n", "1", "--changed"]);
    assert_eq!(ran(&rest), 1, "{}", all_output(&rest));
    assert_eq!(code(&rest), 1, "VALUE == 3 fails test_uses");
}

#[test]
fn changed_does_not_record_files_skipped_by_maxfail() {
    if !have_pytest() {
        return;
    }
    let tree = Tree::new("run_changed");
    let first = tree.cito(&["run", "-n", "1", "--changed"]);
    assert_eq!(code(&first), 0, "{}", all_output(&first));

    // Break both importers; with one test per chunk and -x, only the first
    // chunk runs.
    tree.write("helper.py", "VALUE = -1\n");
    let stopped = tree.cito(&["run", "-n", "1", "--chunk", "1", "-x", "--changed"]);
    assert_eq!(code(&stopped), 1, "{}", all_output(&stopped));
    assert!(stderr(&stopped).contains("stopped early"));

    let rest = tree.cito(&["run", "-n", "1", "--changed"]);
    assert_eq!(ran(&rest), 2, "{}", all_output(&rest));
}

// ---------------------------------------------------------------------------
// Collection errors, --lf
// ---------------------------------------------------------------------------

#[test]
fn collection_error_does_not_hide_the_rest_of_the_chunk() {
    if !have_pytest() {
        return;
    }
    let tree = Tree::new("run_lf");
    let out = tree.cito(&["run", "-n", "1", "--json"]);
    let text = all_output(&out);
    assert_eq!(code(&out), 1, "{text}");
    let summary: serde_json::Value =
        serde_json::from_slice(&out.stdout).unwrap_or_else(|_| panic!("json in {text}"));
    assert_eq!(summary["passed"], 2, "{text}");
    assert_eq!(summary["failed"], 1, "{text}");
    assert_eq!(summary["failed_ids"], serde_json::json!(["test_bad.py"]));
}

#[test]
fn lf_reruns_files_with_collection_errors() {
    if !have_pytest() {
        return;
    }
    let tree = Tree::new("run_lf");
    let first = tree.cito(&["run", "-n", "1"]);
    assert_eq!(code(&first), 1, "{}", all_output(&first));
    assert_eq!(tree.read(".cito/lastfailed"), "test_bad.py\n");

    let lf = tree.cito(&["run", "-n", "1", "--lf"]);
    assert_eq!(ran(&lf), 1, "{}", all_output(&lf));
    assert_eq!(code(&lf), 1);

    // Once the file imports again the entry clears.
    tree.write("test_bad.py", "def test_unreachable():\n    pass\n");
    let fixed = tree.cito(&["run", "-n", "1", "--lf"]);
    assert_eq!(code(&fixed), 0, "{}", all_output(&fixed));
    assert_eq!(ran(&fixed), 1);
    assert_eq!(tree.read(".cito/lastfailed"), "");
}

#[test]
fn lf_matches_on_path_boundaries_and_from_subdirectories() {
    if !have_pytest() {
        return;
    }
    let tree = Tree::new("run_boundary");
    // Run from a subdirectory: pytest prints cwd-relative IDs
    // (`../test_a.py::test_x`), the cache must still be rootdir-relative.
    let sub = tree.path().join("sub");
    let first = tree.cito_in(&sub, &["run", "-n", "1", ".."], &[]);
    assert_eq!(code(&first), 1, "{}", all_output(&first));
    assert_eq!(tree.read(".cito/lastfailed"), "test_a.py::test_x\n");

    // `test_a.py::test_x` must not select `sub/test_a.py::test_x`.
    let lf = tree.cito(&["run", "-n", "1", "--lf"]);
    assert_eq!(ran(&lf), 1, "{}", all_output(&lf));
}

// ---------------------------------------------------------------------------
// Exit codes
// ---------------------------------------------------------------------------

#[test]
fn pytest_usage_errors_propagate() {
    if !have_pytest() {
        return;
    }
    let tree = Tree::new("run_boundary");
    let out = tree.cito(&["run", "-n", "1", "--", "--cito-no-such-option"]);
    assert_eq!(code(&out), 4, "{}", all_output(&out));
    let warm = tree.cito(&["run", "--warm", "-n", "1", "--", "--cito-no-such-option"]);
    assert_eq!(code(&warm), 4, "{}", all_output(&warm));
}

#[test]
fn failures_exit_one_and_success_exits_zero() {
    if !have_pytest() {
        return;
    }
    let tree = Tree::new("run_boundary");
    assert_eq!(code(&tree.cito(&["run", "-n", "2", "--chunk", "1"])), 1);
    assert_eq!(code(&tree.cito(&["run", "-n", "1", "sub"])), 0);
}

// ---------------------------------------------------------------------------
// Warm workers
// ---------------------------------------------------------------------------

#[test]
fn warm_worker_survives_writes_to_real_stdout() {
    if !have_pytest() {
        return;
    }
    let tree = Tree::new("run_warm");
    let out = tree.cito(&["run", "--warm", "-n", "1", "--json", "--", "-s"]);
    let text = all_output(&out);
    assert_eq!(code(&out), 0, "{text}");
    let summary: serde_json::Value =
        serde_json::from_slice(&out.stdout).unwrap_or_else(|_| panic!("json in {text}"));
    assert_eq!(summary["passed"], 3, "{text}");
    assert_eq!(summary["failed_chunks"], 0, "{text}");
}

#[test]
fn warm_worker_exit_via_string_system_exit_is_a_failure() {
    if !have_pytest() {
        return;
    }
    let tree = Tree::new("run_warm");
    tree.write(
        "conftest.py",
        "def pytest_sessionfinish(session):\n    raise SystemExit('bye')\n",
    );
    let out = tree.cito(&["run", "--warm", "-n", "1"]);
    let text = all_output(&out);
    assert_ne!(code(&out), 0, "{text}");
    assert!(!text.contains("worker died"), "{text}");
}

// ---------------------------------------------------------------------------
// Daemon
// ---------------------------------------------------------------------------

/// Stops the tree's daemon however the test ends.
#[cfg(unix)]
struct DaemonGuard<'a>(&'a Tree);

#[cfg(unix)]
impl Drop for DaemonGuard<'_> {
    fn drop(&mut self) {
        let _ = self.0.cito(&["daemon", "stop"]);
    }
}

#[cfg(unix)]
#[test]
fn daemon_reloads_dependents_and_follows_client_env() {
    if !have_pytest() {
        return;
    }
    let tree = Tree::new("run_daemon");
    let _guard = DaemonGuard(&tree);
    let root = tree.path().to_path_buf();
    let on = [("CITO_FIXTURE_FLAG", "on")];

    let first = tree.cito_in(&root, &["run", "--daemon", "-n", "1"], &on);
    assert_eq!(code(&first), 0, "{}", all_output(&first));

    // A different environment must reach the workers.
    let off = tree.cito_in(&root, &["run", "--daemon", "-n", "1"], &[]);
    assert_eq!(code(&off), 1, "{}", all_output(&off));
    assert!(all_output(&off).contains("test_env"));

    // Editing a helper must invalidate `from helper import f` in the test.
    tree.write("helper.py", "def f():\n    return 2\n");
    let stale = tree.cito_in(&root, &["run", "--daemon", "-n", "1"], &on);
    assert_eq!(code(&stale), 1, "{}", all_output(&stale));
    assert!(all_output(&stale).contains("test_f"));

    let stop = tree.cito(&["daemon", "stop"]);
    assert!(
        stderr(&stop).contains("daemon stopped"),
        "{}",
        stderr(&stop)
    );
    let status = tree.cito(&["daemon", "status"]);
    assert!(stderr(&status).contains("no daemon running"));
}

#[cfg(unix)]
#[test]
fn concurrent_daemon_starts_leave_a_single_daemon() {
    if !have_pytest() {
        return;
    }
    let tree = Tree::new("run_daemon");
    let _guard = DaemonGuard(&tree);
    let children: Vec<_> = (0..4)
        .map(|_| {
            Command::new(env!("CARGO_BIN_EXE_cito"))
                .args(["daemon", "start"])
                .current_dir(tree.path())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("spawn cito")
        })
        .collect();
    for child in children {
        let out = child.wait_with_output().expect("start daemon");
        assert!(out.status.success(), "{}", stderr(&out));
    }
    let stop = tree.cito(&["daemon", "stop"]);
    assert!(stderr(&stop).contains("daemon stopped"));
    // A straggler that grabbed the serve lock after the stop would answer.
    for _ in 0..4 {
        std::thread::sleep(std::time::Duration::from_millis(250));
        let status = tree.cito(&["daemon", "status"]);
        assert!(
            stderr(&status).contains("no daemon running"),
            "{}",
            stderr(&status)
        );
    }
}
