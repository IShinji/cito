//! Config-file parsing, -k/-m selection, and addopts handling, exercised
//! through the `cito collect` binary. Expected outputs are pinned against
//! `pytest --collect-only -q` (pytest 9.0.3) on the same trees.

use std::path::{Path, PathBuf};
use std::process::Command;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Run `cito collect ARGS` in `dir`: (success, sorted node IDs, stderr).
fn cito(dir: &Path, args: &[&str]) -> (bool, Vec<String>, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_cito"))
        .arg("collect")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run cito");
    let mut ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.contains("::"))
        .map(str::to_string)
        .collect();
    ids.sort();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    (out.status.success(), ids, stderr)
}

fn ids(dir: &Path, args: &[&str]) -> Vec<String> {
    let (ok, ids, stderr) = cito(dir, args);
    assert!(ok, "cito failed: {stderr}");
    ids
}

fn sorted(items: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = items.iter().map(|s| s.to_string()).collect();
    v.sort();
    v
}

/// A scratch tree under the system temp dir, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str, files: &[(&str, &str)]) -> Scratch {
        let root = std::env::temp_dir().join(format!("cito-config-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (path, content) in files {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        Scratch(root)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const TEST_A: &str = "def test_a():\n    pass\n";

#[test]
fn iniconfig_tree_matches_pytest() {
    // `[pytest]  # comment` header, `key: value`, shlex-quoted patterns,
    // separator patterns matched at any depth (`*/` prefix), fnmatch
    // classes in python_classes, literal `{arch}` (no brace expansion),
    // and big non-decimal int parametrize IDs rendered in decimal.
    let expected = sorted(&[
        "arch/test_arch.py::test_arch",
        "deep/sub/extra/y_case.py::test_deep",
        "gen/test_kept.py::test_kept",
        "spec_one.py::test_spec",
        "sub/extra/x_case.py::test_case",
        "sub/test_k.py::test_a",
        "sub/test_k.py::TestC::test_b",
        "sub/test_k.py::CheckA::test_checked",
        "sub/test_k.py::test_c",
        "sub/test_k.py::test_big[1208925819614629174706175]",
        "sub/test_k.py::test_big[0]",
        "sub/test_k.py::test_big[37778931862957161709567]",
        "sub/test_k.py::test_big[36893488147419103231]",
        "sub/test_k.py::test_big[-18446744073709551616]",
    ]);
    assert_eq!(ids(&fixture("iniconfig"), &[]), expected);
}

#[test]
fn keyword_matches_pytest_name_set() {
    let root = fixture("iniconfig");
    let count = |args: &[&str]| ids(&root, args).len();
    // Marker names are keywords.
    assert_eq!(
        ids(&root, &["-k", "slow"]),
        sorted(&["sub/test_k.py::test_a"])
    );
    // Directory names in the node chain are keywords...
    assert_eq!(count(&["-k", "sub"]), 11);
    assert_eq!(count(&["-k", "extra"]), 2);
    // ...but not the rootdir itself.
    assert_eq!(count(&["-k", "iniconfig"]), 0);
    // A fragment must fit inside one name; `::` never spans names.
    assert_eq!(count(&["-k", "py::TestC"]), 0);
    // `parametrize` is a mark; directly-marked functions carry `pytestmark`.
    assert_eq!(count(&["-k", "parametrize"]), 5);
    assert_eq!(count(&["-k", "pytestmark"]), 6);
    assert_eq!(count(&["-k", "checked and not un"]), 1);
}

#[test]
fn empty_expressions() {
    let root = fixture("iniconfig");
    let all = ids(&root, &[]).len();
    assert_eq!(ids(&root, &["-k", ""]).len(), all);
    assert_eq!(ids(&root, &["-k", "  "]).len(), all);
    assert_eq!(ids(&root, &["-m", ""]).len(), all);
    // pytest: a whitespace-only -m is the empty expression, i.e. False.
    assert_eq!(ids(&root, &["-m", " "]).len(), 0);
}

#[test]
fn addopts_attached_forms_and_ignore() {
    let tree = Scratch::new(
        "addopts",
        &[
            (
                "pytest.ini",
                "[pytest]\naddopts = -kslow --ignore=sub \"-m=not fast\"\n",
            ),
            (
                "test_m.py",
                "import pytest\n@pytest.mark.slow\ndef test_s(): pass\n\
                 @pytest.mark.fast\ndef test_slow_fast(): pass\ndef test_f(): pass\n",
            ),
            ("sub/test_x.py", "def test_slow_sub(): pass\n"),
        ],
    );
    assert_eq!(ids(&tree.0, &[]), sorted(&["test_m.py::test_s"]));
}

#[test]
fn norecursedirs_default_arch_is_literal() {
    let tree = Scratch::new(
        "arch",
        &[
            ("setup.py", ""),
            ("arch/test_a.py", TEST_A),
            ("{arch}/test_a.py", TEST_A),
        ],
    );
    assert_eq!(ids(&tree.0, &[]), sorted(&["arch/test_a.py::test_a"]));
}

#[test]
fn spaced_section_name_is_not_pytest() {
    // iniconfig does not trim section names: `[ pytest ]` is not [pytest],
    // so tox.ini is not a config and python_files keeps its default.
    let tree = Scratch::new(
        "spaced",
        &[
            ("tox.ini", "[ pytest ]\npython_files = check_*.py\n"),
            ("setup.py", ""),
            ("test_a.py", TEST_A),
            ("check_b.py", TEST_A),
        ],
    );
    assert_eq!(ids(&tree.0, &[]), sorted(&["test_a.py::test_a"]));
}

#[test]
fn config_errors_abort_like_pytest() {
    let cases: &[(&str, &str, &str)] = &[
        (
            "setup.cfg",
            "[pytest]\npython_files = x\n",
            "[pytest] section in setup.cfg files is no longer supported",
        ),
        (
            "pyproject.toml",
            "[tool.pytest]\ntestpaths = [\"t\"]\n[tool.pytest.ini_options]\ntestpaths = \"t\"\n",
            "Cannot use both [tool.pytest]",
        ),
        (
            "pyproject.toml",
            "[tool.pytest]\ntestpaths = \"t\"\n",
            "config option 'testpaths' expects a list for type 'args', got str: 't'",
        ),
        (
            "tox.ini",
            "[pytest]\nfoo\n",
            "tox.ini:2: unexpected line: 'foo'",
        ),
    ];
    for (i, (name, content, message)) in cases.iter().enumerate() {
        let tree = Scratch::new(
            &format!("err{i}"),
            &[(name, content), ("test_a.py", TEST_A)],
        );
        let (ok, _, stderr) = cito(&tree.0, &[]);
        assert!(!ok, "{name} should fail");
        assert!(stderr.contains(message), "{name}: {stderr}");
    }
}
