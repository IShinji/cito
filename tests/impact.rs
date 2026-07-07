use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use cito::config::Config;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/impact")
}

fn closures() -> HashMap<String, HashSet<String>> {
    let root = fixture();
    let config = Config::discover(&root);
    let files = cito::collector::collect(std::slice::from_ref(&root), &config, None, None);
    cito::collector::impact_closures(&config, &files)
}

#[test]
fn closure_contains_direct_project_import() {
    let closures = closures();
    let core = &closures["test_core.py"];
    assert!(core.contains("test_core.py"));
    assert!(core.contains("pkg/core.py"));
}

#[test]
fn closure_is_transitive_through_project_modules() {
    let closures = closures();
    let util = &closures["test_util.py"];
    assert!(util.contains("pkg/util.py"));
    assert!(
        util.contains("pkg/core.py"),
        "test_util imports pkg.util which imports pkg.core"
    );
}

#[test]
fn unrelated_test_is_not_impacted_by_package_files() {
    let closures = closures();
    let free = &closures["test_free.py"];
    assert!(free.contains("test_free.py"));
    assert!(!free.contains("pkg/core.py"));
    assert!(!free.contains("pkg/util.py"));
}

#[test]
fn conftest_and_config_join_every_closure() {
    let closures = closures();
    for (file, closure) in &closures {
        assert!(closure.contains("conftest.py"), "{file} misses conftest");
        assert!(closure.contains("pytest.ini"), "{file} misses config");
    }
}
