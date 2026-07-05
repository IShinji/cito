use std::path::{Path, PathBuf};

use ignore::WalkBuilder;
use rayon::prelude::*;
use ruff_python_ast::Stmt;
use serde::Serialize;

/// pytest's default `norecursedirs`: '*.egg', '.*', '_darcs', 'build', 'CVS',
/// 'dist', 'node_modules', 'venv', '{arch}'. Hidden entries are skipped by the
/// walker itself; `__pycache__` never contains test sources.
const SKIP_DIRS: &[&str] = &[
    "_darcs",
    "build",
    "CVS",
    "dist",
    "node_modules",
    "venv",
    "{arch}",
    "__pycache__",
];

#[derive(Debug, Serialize)]
pub struct FileTests {
    pub path: String,
    pub tests: Vec<String>,
}

/// pytest's default `python_files`: `test_*.py` or `*_test.py`.
pub fn is_test_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    name.ends_with(".py") && (name.starts_with("test_") || name.ends_with("_test.py"))
}

fn discover(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for root in roots {
        if root.is_file() {
            if is_test_file(root) {
                files.push(root.clone());
            }
            continue;
        }
        let walker = WalkBuilder::new(root)
            .standard_filters(false)
            .hidden(true)
            .filter_entry(|entry| {
                let name = entry.file_name().to_string_lossy();
                let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
                !(is_dir && (SKIP_DIRS.contains(&name.as_ref()) || name.ends_with(".egg")))
            })
            .build();
        for entry in walker.flatten() {
            if entry.file_type().is_some_and(|t| t.is_file()) && is_test_file(entry.path()) {
                files.push(entry.into_path());
            }
        }
    }
    files.sort();
    files.dedup();
    files
}

/// Collect tests from all `roots`, in parallel. Files that fail to read or
/// parse are reported on stderr and yield no tests, mirroring pytest's
/// per-file collection errors rather than aborting the whole run.
pub fn collect(roots: &[PathBuf]) -> Vec<FileTests> {
    discover(roots)
        .par_iter()
        .map(|path| {
            let tests = match std::fs::read_to_string(path) {
                Ok(source) => match collect_source(&source) {
                    Ok(tests) => tests,
                    Err(err) => {
                        eprintln!(
                            "cito: warning: skipping {} (parse error: {err})",
                            path.display()
                        );
                        Vec::new()
                    }
                },
                Err(err) => {
                    eprintln!("cito: warning: skipping {} ({err})", path.display());
                    Vec::new()
                }
            };
            FileTests {
                path: node_path(path),
                tests,
            }
        })
        .collect()
}

/// Node IDs use forward slashes relative to the invocation directory,
/// mirroring pytest's `path::Class::func` format.
fn node_path(path: &Path) -> String {
    let path = path.strip_prefix(".").unwrap_or(path);
    let s = path.to_string_lossy();
    if std::path::MAIN_SEPARATOR == '/' {
        s.into_owned()
    } else {
        s.replace(std::path::MAIN_SEPARATOR, "/")
    }
}

/// Collect test names (`func`, `TestClass::method`, ...) from one module's
/// source, in definition order.
pub fn collect_source(source: &str) -> Result<Vec<String>, ruff_python_parser::ParseError> {
    let module = ruff_python_parser::parse_module(source)?.into_syntax();
    let mut tests = Vec::new();
    walk(&module.body, &mut Vec::new(), &mut tests);
    Ok(tests)
}

/// pytest defaults: functions/methods matching `test*` are tests; classes
/// matching `Test*` are collected (recursively) unless they define
/// `__init__`. Behavior is pinned against real pytest by
/// `scripts/diff_collect.py`.
fn walk(body: &[Stmt], class_stack: &mut Vec<String>, tests: &mut Vec<String>) {
    for stmt in body {
        match stmt {
            Stmt::FunctionDef(func) => {
                if func.name.as_str().starts_with("test") {
                    let mut parts: Vec<&str> = class_stack.iter().map(String::as_str).collect();
                    parts.push(func.name.as_str());
                    tests.push(parts.join("::"));
                }
            }
            Stmt::ClassDef(class)
                if class.name.as_str().starts_with("Test") && !defines_init(&class.body) =>
            {
                class_stack.push(class.name.to_string());
                walk(&class.body, class_stack, tests);
                class_stack.pop();
            }
            _ => {}
        }
    }
}

fn defines_init(body: &[Stmt]) -> bool {
    body.iter()
        .any(|stmt| matches!(stmt, Stmt::FunctionDef(func) if func.name.as_str() == "__init__"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_file_patterns() {
        assert!(is_test_file(Path::new("test_foo.py")));
        assert!(is_test_file(Path::new("a/b/foo_test.py")));
        assert!(!is_test_file(Path::new("foo.py")));
        assert!(!is_test_file(Path::new("test_foo.txt")));
        assert!(!is_test_file(Path::new("contest.py")));
    }

    #[test]
    fn collects_functions_classes_and_nesting() {
        let source = r#"
def test_one():
    pass

async def test_async():
    pass

def helper():
    pass

class TestThing:
    def test_method(self):
        pass

    class TestNested:
        def test_inner(self):
            pass

class TestWithInit:
    def __init__(self):
        pass

    def test_skipped(self):
        pass

class Plain:
    def test_not_collected(self):
        pass
"#;
        let tests = collect_source(source).unwrap();
        assert_eq!(
            tests,
            vec![
                "test_one",
                "test_async",
                "TestThing::test_method",
                "TestThing::TestNested::test_inner",
            ]
        );
    }
}
