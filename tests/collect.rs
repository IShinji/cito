use std::path::PathBuf;

/// Pinned against `pytest --collect-only -q` output on the same tree; the
/// live equivalence check is `scripts/diff_collect.py`, run in CI.
#[test]
fn collects_fixture_tree_like_pytest() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/basic");
    let files = cito::collector::collect(std::slice::from_ref(&root));
    let prefix = format!("{}/", root.display());
    let mut ids: Vec<String> = files
        .iter()
        .flat_map(|file| {
            let rel = file
                .path
                .strip_prefix(&prefix)
                .unwrap_or(&file.path)
                .to_string();
            file.tests.iter().map(move |t| format!("{rel}::{t}"))
        })
        .collect();
    ids.sort();

    let mut expected: Vec<String> = [
        "helpers_test.py::test_suffix_pattern",
        "sub/test_inner.py::test_in_subdir",
        "test_sample.py::test_addition",
        "test_sample.py::test_async_thing",
        "test_sample.py::testnounderscore",
        "test_sample.py::TestWidget::test_render",
        "test_sample.py::TestWidget::TestNested::test_inner",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    expected.sort();

    assert_eq!(ids, expected);
}
