use std::path::{Path, PathBuf};

use cito::config::Config;

fn collect_ids(root: &Path, walk: &Path) -> Vec<String> {
    let config = Config::discover(root);
    let files = cito::collector::collect(
        std::slice::from_ref(&walk.to_path_buf()),
        &config,
        None,
        None,
    );
    let mut ids: Vec<String> = files
        .iter()
        .flat_map(|file| {
            file.tests
                .iter()
                .map(move |t| format!("{}::{}", file.path, t))
        })
        .collect();
    ids.sort();
    ids
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Pinned against `pytest --collect-only -q` on the same tree; the live
/// equivalence check is `scripts/diff_collect.py`, run in CI.
#[test]
fn basic_tree_matches_pytest() {
    let root = fixture("basic");
    let ids = collect_ids(&root, &root);
    let mut expected: Vec<String> = [
        "helpers_test.py::test_suffix_pattern",
        "sub/test_inner.py::test_in_subdir",
        "test_inherit.py::LegacySuite::test_unittest_style",
        "test_marks.py::test_marked_slow",
        "test_marks.py::TestMarkedClass::test_net_call",
        "test_inherit.py::TestInherited::test_own",
        "test_inherit.py::TestInherited::test_second_level",
        "test_inherit.py::TestInherited::test_from_mixin",
        "test_inherit.py::TestOverride::test_from_mixin",
        "test_params.py::test_ints[1]",
        "test_params.py::test_ints[2]",
        "test_params.py::test_ints[3]",
        "test_params.py::test_strs[red]",
        "test_params.py::test_strs[blue]",
        "test_params.py::test_specials[None]",
        "test_params.py::test_specials[True]",
        "test_params.py::test_specials[False]",
        "test_params.py::test_pairs[1-a]",
        "test_params.py::test_pairs[2-b]",
        "test_params.py::test_stacked[x-1]",
        "test_params.py::test_stacked[x-2]",
        "test_params.py::test_stacked[y-1]",
        "test_params.py::test_stacked[y-2]",
        "test_params.py::test_floats",
        "test_params.py::test_ids[one]",
        "test_params.py::test_ids[two]",
        "test_params.py::test_complex",
        "test_params.py::test_param_objects[1]",
        "test_params.py::test_param_objects[two]",
        "test_params.py::test_param_objects[3]",
        "test_params.py::TestClassParamObjects::test_with_ext[.xlsx]",
        "test_params.py::TestClassParamObjects::test_with_ext[.ods]",
        "test_params.py::TestClsParams::test_m[1]",
        "test_params.py::TestClsParams::test_m[2]",
        "test_params.py::TestClassLevel::test_via_class[p]",
        "test_params.py::TestClassLevel::test_via_class[q]",
        "test_params.py::TestClassLevel::test_combined[1-p]",
        "test_params.py::TestClassLevel::test_combined[2-p]",
        "test_params.py::TestClassLevel::test_combined[1-q]",
        "test_params.py::TestClassLevel::test_combined[2-q]",
        "test_condeps.py::test_needs_missing_dep",
        "test_sample.py::test_addition",
        "test_shadowing.py::test_shadowed",
        "test_shadowing.py::TestShadow::test_dup",
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

/// Plugin-heavy decorators must not disturb collection: hypothesis @given
/// and mock.patch are ID-neutral; parametrize still expands next to them.
#[test]
fn plugins_tree_matches_pytest() {
    let root = fixture("plugins");
    let ids = collect_ids(&root, &root);
    let mut expected: Vec<String> = [
        "test_plugins.py::test_asyncio_auto",
        "test_plugins.py::test_hypothesis_given",
        "test_plugins.py::test_mock_patch",
        "test_plugins.py::test_mock_plus_params[1]",
        "test_plugins.py::test_mock_plus_params[2]",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    expected.sort();
    assert_eq!(ids, expected);
}

/// The configured tree exercises pytest.ini overrides: python_files,
/// python_classes, python_functions, norecursedirs. testpaths is exercised
/// by walking `suite` the way `commands::resolve_roots` would.
#[test]
fn configured_tree_honors_pytest_ini() {
    let root = fixture("configured");
    let ids = collect_ids(&root, &root.join("suite"));
    let mut expected: Vec<String> = [
        "suite/check_alpha.py::check_one",
        "suite/check_alpha.py::spec_two",
        "suite/check_alpha.py::SuiteAlpha::check_method",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    expected.sort();
    assert_eq!(ids, expected);
}

/// Collection-semantics edge cases, pinned IN ORDER against
/// `pytest --collect-only -q` (pytest 9.0.3): `__test__` switches, inherited
/// constructors, conftest collect_ignore directories / nearest-conftest
/// shadowing / fnmatch globs, static elif chains, and pytest's ordering
/// rules — module and class namespaces are dicts (a redefinition keeps its
/// first slot, `del` frees it, imports sit where the import is), classes
/// yield inherited members first in reverse C3 MRO order with nested
/// classes interleaved, and unittest methods come sorted by name.
#[test]
fn edge_tree_matches_pytest_in_order() {
    let root = fixture("edge");
    let config = Config::discover(&root);
    let files = cito::collector::collect(std::slice::from_ref(&root), &config, None, None);
    let ids: Vec<String> = files
        .iter()
        .flat_map(|file| {
            file.tests
                .iter()
                .map(move |t| format!("{}::{}", file.path, t))
        })
        .collect();
    let expected = [
        "globbed/test_keep.py::test_keep",
        "shadowed/test_kept.py::test_kept",
        "test_classorder.py::TestOuter::test_1",
        "test_classorder.py::TestOuter::TestInner::test_in",
        "test_classorder.py::TestOuter::test_2",
        "test_classorder.py::TestChild::test_base",
        "test_classorder.py::TestChild::test_child",
        "test_classorder.py::TestChild::test_overridden",
        "test_classorder.py::TestDiamond::test_a1",
        "test_classorder.py::TestDiamond::test_c",
        "test_classorder.py::TestDiamond::test_m",
        "test_classorder.py::TestHasInherited::TestInherited::test_i",
        "test_classorder.py::TestHasInherited::test_h",
        "test_classorder.py::TestAttrs::test_overridden",
        "test_classorder.py::TestAttrs::test_lambda",
        "test_classorder.py::TestAttrs::test_d",
        "test_classorder.py::UT::test_M",
        "test_classorder.py::UT::test_a",
        "test_classorder.py::UT::test_z",
        "test_ctor.py::TestNoCtor::test_z",
        "test_dunder.py::TestBackOn::test_base",
        "test_dunder.py::TestBackOn::test_on",
        "test_dunder.py::NotNamedLikeATest::test_nose_style",
        "test_dunder.py::test_fn_on",
        "test_elif.py::test_first_arm",
        "test_elif.py::test_live_elif",
        "test_modorder.py::test_a",
        "test_modorder.py::test_imported",
        "test_modorder.py::test_b",
        "test_modorder.py::test_in_with",
        "test_modorder.py::test_c",
    ];
    assert_eq!(ids, expected);
}
