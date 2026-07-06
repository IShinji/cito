use std::collections::HashMap;
use std::path::{Path, PathBuf};

use globset::{Glob, GlobMatcher};

/// pytest name patterns are "prefixes or glob-style patterns": a pattern
/// without glob metacharacters matches as a prefix.
#[derive(Clone)]
enum NamePattern {
    Prefix(String),
    Glob(GlobMatcher),
}

impl NamePattern {
    fn new(pattern: &str) -> Option<NamePattern> {
        if pattern.contains(['*', '?', '[']) {
            Glob::new(pattern)
                .ok()
                .map(|g| NamePattern::Glob(g.compile_matcher()))
        } else {
            Some(NamePattern::Prefix(pattern.to_string()))
        }
    }

    fn matches(&self, name: &str) -> bool {
        match self {
            NamePattern::Prefix(prefix) => name.starts_with(prefix),
            NamePattern::Glob(glob) => glob.is_match(name),
        }
    }
}

/// File/dir patterns: a pattern containing `/` matches against the
/// rootdir-relative path; otherwise against the basename (fnmatch-style).
#[derive(Clone)]
struct PathPatterns {
    names: Vec<GlobMatcher>,
    paths: Vec<GlobMatcher>,
}

impl PathPatterns {
    fn new(patterns: &[String]) -> PathPatterns {
        let mut names = Vec::new();
        let mut paths = Vec::new();
        for pattern in patterns {
            let Ok(glob) = Glob::new(pattern) else {
                continue;
            };
            if pattern.contains('/') {
                paths.push(glob.compile_matcher());
            } else {
                names.push(glob.compile_matcher());
            }
        }
        PathPatterns { names, paths }
    }

    fn matches(&self, name: &str, rel: Option<&Path>) -> bool {
        self.names.iter().any(|m| m.is_match(name))
            || rel.is_some_and(|rel| self.paths.iter().any(|m| m.is_match(rel)))
    }
}

/// The subset of pytest configuration that affects collection, resolved the
/// way pytest resolves it: walk upward from the invocation anchor looking
/// for `pytest.ini`, `pyproject.toml` (`[tool.pytest]` or
/// `[tool.pytest.ini_options]`), `tox.ini` (`[pytest]`), then `setup.cfg`
/// (`[tool:pytest]`). The directory holding the winning file becomes the
/// rootdir; node IDs are relative to it.
#[derive(Clone)]
pub struct Config {
    pub rootdir: PathBuf,
    pub source: Option<PathBuf>,
    pub testpaths: Vec<String>,
    /// Raw addopts entries (whitespace/array split) — used only to warn
    /// about interactions (e.g. pytest-xdist's -n).
    pub addopts: Vec<String>,
    python_files: PathPatterns,
    python_classes: Vec<NamePattern>,
    python_functions: Vec<NamePattern>,
    norecursedirs: PathPatterns,
}

const DEFAULT_FILES: &[&str] = &["test_*.py", "*_test.py"];
const DEFAULT_CLASSES: &[&str] = &["Test"];
const DEFAULT_FUNCTIONS: &[&str] = &["test"];
const DEFAULT_NORECURSE: &[&str] = &[
    "*.egg",
    ".*",
    "_darcs",
    "build",
    "CVS",
    "dist",
    "node_modules",
    "venv",
    "{arch}",
];

fn defaults(patterns: &[&str]) -> Vec<String> {
    patterns.iter().map(|s| s.to_string()).collect()
}

/// Quote-aware splitting for `addopts = "-m 'not stress'"` style values.
fn shell_split(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for c in input.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => current.push(c),
            None => match c {
                '\'' | '"' => quote = Some(c),
                c if c.is_whitespace() => {
                    if !current.is_empty() {
                        out.push(std::mem::take(&mut current));
                    }
                }
                c => current.push(c),
            },
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

impl Config {
    /// Convenience wrapper: discover with the invocation dir as the only arg.
    pub fn discover(start: &Path) -> Config {
        Config::discover_for(start, &[])
    }

    /// Mirror of pytest's `determine_setup` (src/_pytest/config/findpaths.py):
    /// 1. compute the common ancestor of the argument directories;
    /// 2. walk it upward for a config file (pytest.toml, .pytest.toml,
    ///    pytest.ini, .pytest.ini, pyproject.toml with a [tool.pytest*]
    ///    table, tox.ini with [pytest], setup.cfg with [tool:pytest]);
    ///    a section-less pyproject.toml is only remembered as a last resort;
    /// 3. else walk upward for setup.py;
    /// 4. else repeat the config search from each argument dir separately;
    /// 5. else rootdir = common ancestor of the invocation dir and the args.
    pub fn discover_for(invocation_dir: &Path, arg_dirs: &[PathBuf]) -> Config {
        let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        let invocation_dir = canon(invocation_dir);
        let dirs: Vec<PathBuf> = arg_dirs
            .iter()
            .map(|p| canon(p))
            .filter(|p| p.exists())
            .collect();
        let ancestor = common_ancestor(&invocation_dir, &dirs);

        if let Some((root, source, options)) = locate_config(std::slice::from_ref(&ancestor)) {
            return Config::build(root, Some(source), options);
        }
        for dir in ancestor.ancestors() {
            if dir.join("setup.py").is_file() {
                return Config::build(dir.to_path_buf(), None, HashMap::new());
            }
        }
        if dirs.as_slice() != std::slice::from_ref(&ancestor) && !dirs.is_empty() {
            if let Some((root, source, options)) = locate_config(&dirs) {
                return Config::build(root, Some(source), options);
            }
        }
        let mut root =
            common_ancestor(&invocation_dir, &[invocation_dir.clone(), ancestor.clone()]);
        if root.parent().is_none() {
            root = ancestor;
        }
        Config::build(root, None, HashMap::new())
    }

    fn build(
        rootdir: PathBuf,
        source: Option<PathBuf>,
        options: HashMap<String, Vec<String>>,
    ) -> Config {
        let get = |key: &str, fallback: &[&str]| -> Vec<String> {
            options
                .get(key)
                .cloned()
                .unwrap_or_else(|| defaults(fallback))
        };
        // `__pycache__` never holds sources, so skip it unconditionally.
        let mut norecurse = get("norecursedirs", DEFAULT_NORECURSE);
        norecurse.push("__pycache__".to_string());
        Config {
            rootdir,
            source,
            testpaths: options.get("testpaths").cloned().unwrap_or_default(),
            addopts: options.get("addopts").cloned().unwrap_or_default(),
            python_files: PathPatterns::new(&get("python_files", DEFAULT_FILES)),
            python_classes: get("python_classes", DEFAULT_CLASSES)
                .iter()
                .filter_map(|p| NamePattern::new(p))
                .collect(),
            python_functions: get("python_functions", DEFAULT_FUNCTIONS)
                .iter()
                .filter_map(|p| NamePattern::new(p))
                .collect(),
            norecursedirs: PathPatterns::new(&norecurse),
        }
    }

    /// `rel` is the rootdir-relative path, when the file is under rootdir.
    pub fn is_test_file(&self, name: &str, rel: Option<&Path>) -> bool {
        name.ends_with(".py") && self.python_files.matches(name, rel)
    }

    pub fn class_matches(&self, name: &str) -> bool {
        self.python_classes.iter().any(|p| p.matches(name))
    }

    pub fn function_matches(&self, name: &str) -> bool {
        self.python_functions.iter().any(|p| p.matches(name))
    }

    pub fn skip_dir(&self, dir: &Path, rel: Option<&Path>) -> bool {
        let Some(name) = dir.file_name().and_then(|n| n.to_str()) else {
            return false;
        };
        // pytest also refuses to descend into virtualenvs regardless of name.
        self.norecursedirs.matches(name, rel) || dir.join("pyvenv.cfg").is_file()
    }

    pub fn relative_to_root<'a>(&self, abs: &'a Path) -> Option<&'a Path> {
        abs.strip_prefix(&self.rootdir).ok()
    }

    /// The value of a flag inside addopts (`-m X`, `-m=X`); last one wins,
    /// mirroring how pytest prepends addopts to the CLI.
    pub fn addopts_flag(&self, flag: &str) -> Option<String> {
        let mut found = None;
        let mut iter = self.addopts.iter();
        while let Some(arg) = iter.next() {
            if arg == flag {
                if let Some(value) = iter.next() {
                    found = Some(value.clone());
                }
            } else if let Some(rest) = arg.strip_prefix(&format!("{flag}=")) {
                found = Some(rest.to_string());
            }
        }
        found
    }
}

/// pytest's common-ancestor computation (existing paths only; files count
/// via their own path; empty input falls back to the invocation dir).
fn common_ancestor(invocation_dir: &Path, paths: &[PathBuf]) -> PathBuf {
    let mut ancestor: Option<PathBuf> = None;
    for path in paths {
        if !path.exists() {
            continue;
        }
        ancestor = Some(match ancestor {
            None => path.clone(),
            Some(current) => {
                if path.starts_with(&current) {
                    current
                } else if current.starts_with(path) {
                    path.clone()
                } else {
                    let mut shared = PathBuf::new();
                    for (a, b) in current.components().zip(path.components()) {
                        if a != b {
                            break;
                        }
                        shared.push(a);
                    }
                    shared
                }
            }
        });
    }
    let ancestor = ancestor.unwrap_or_else(|| invocation_dir.to_path_buf());
    if ancestor.is_file() {
        ancestor.parent().unwrap_or(&ancestor).to_path_buf()
    } else {
        ancestor
    }
}

const CONFIG_NAMES: &[&str] = &[
    "pytest.toml",
    ".pytest.toml",
    "pytest.ini",
    ".pytest.ini",
    "pyproject.toml",
    "tox.ini",
    "setup.cfg",
];

/// pytest's `locate_config`: walk each arg and its parents, trying the
/// config names in order. A section-less pyproject.toml never wins directly
/// but the first one seen becomes the fallback rootdir anchor.
fn locate_config(args: &[PathBuf]) -> Option<(PathBuf, PathBuf, HashMap<String, Vec<String>>)> {
    let mut bare_pyproject: Option<PathBuf> = None;
    for arg in args {
        let chain = std::iter::once(arg.as_path()).chain(arg.ancestors().skip(1));
        for base in chain {
            for name in CONFIG_NAMES {
                let candidate = base.join(name);
                if !candidate.is_file() {
                    continue;
                }
                if *name == "pyproject.toml" && bare_pyproject.is_none() {
                    bare_pyproject = Some(candidate.clone());
                }
                if let Some(options) = load_config_file(&candidate) {
                    return Some((base.to_path_buf(), candidate, options));
                }
            }
        }
    }
    bare_pyproject.map(|p| {
        let parent = p.parent().unwrap_or(Path::new("/")).to_path_buf();
        (parent, p, HashMap::new())
    })
}

/// pytest's `load_config_dict_from_file`: None = this file is not a pytest
/// config (a bare pyproject.toml, a tox.ini without [pytest], ...).
fn load_config_file(path: &Path) -> Option<HashMap<String, Vec<String>>> {
    let name = path.file_name()?.to_str()?;
    let text = std::fs::read_to_string(path).unwrap_or_default();
    match name {
        // Always a config source, even when empty.
        "pytest.ini" | ".pytest.ini" => Some(ini_section(&text, "pytest").unwrap_or_default()),
        "pytest.toml" | ".pytest.toml" => {
            let value: toml::Table = text.parse().ok()?;
            let table = value.get("pytest").and_then(|v| v.as_table());
            Some(table.map(extract_toml_options).unwrap_or_default())
        }
        "pyproject.toml" => pyproject_options(&text),
        "tox.ini" => ini_section(&text, "pytest"),
        "setup.cfg" => ini_section(&text, "tool:pytest"),
        _ => None,
    }
}

/// pytest 9 accepts both `[tool.pytest]` (canonical) and the legacy
/// `[tool.pytest.ini_options]`; when both exist, ini_options wins per key
/// order here (checked first).
fn extract_toml_options(table: &toml::Table) -> HashMap<String, Vec<String>> {
    let mut options = HashMap::new();
    for (key, value) in table {
        let values = match value {
            toml::Value::String(s) if key == "addopts" => shell_split(s),
            toml::Value::String(s) => s.split_whitespace().map(str::to_string).collect(),
            toml::Value::Array(items) => items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            _ => continue,
        };
        options.insert(key.clone(), values);
    }
    options
}

fn pyproject_options(text: &str) -> Option<HashMap<String, Vec<String>>> {
    let value: toml::Table = text.parse().ok()?;
    let pytest = value.get("tool")?.as_table()?.get("pytest")?.as_table()?;
    // [tool.pytest.ini_options] (ini mode) or [tool.pytest] minus that key
    // (toml mode); a [tool.pytest] table holding ONLY ini_options is not
    // itself a config.
    if let Some(ini) = pytest.get("ini_options").and_then(|v| v.as_table()) {
        return Some(extract_toml_options(ini));
    }
    if pytest.is_empty() {
        return None;
    }
    Some(extract_toml_options(pytest))
}

/// Minimal INI reader: `key = value` pairs plus indented continuation lines,
/// which is all pytest configs use. Returns None if the section is absent.
fn ini_section(text: &str, section: &str) -> Option<HashMap<String, Vec<String>>> {
    let mut in_section = false;
    let mut found = false;
    let mut values: HashMap<String, String> = HashMap::new();
    let mut current_key: Option<String> = None;
    for raw in text.lines() {
        let line = raw.trim_end();
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') || trimmed.starts_with(';') || trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_section = trimmed[1..trimmed.len() - 1].trim() == section;
            found |= in_section;
            current_key = None;
            continue;
        }
        if !in_section {
            continue;
        }
        if raw.starts_with([' ', '\t']) {
            if let Some(key) = &current_key {
                let entry = values.entry(key.clone()).or_default();
                entry.push(' ');
                entry.push_str(trimmed);
            }
            continue;
        }
        if let Some(eq) = line.find('=') {
            let key = line[..eq].trim().to_string();
            let value = line[eq + 1..].trim().to_string();
            values.insert(key.clone(), value);
            current_key = Some(key);
        }
    }
    if !found {
        return None;
    }
    Some(
        values
            .into_iter()
            .map(|(k, v)| {
                let split = if k == "addopts" {
                    shell_split(&v)
                } else {
                    v.split_whitespace().map(str::to_string).collect()
                };
                (k, split)
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ini_parsing_with_continuations() {
        let text = "[pytest]\npython_files = check_*.py\nnorecursedirs =\n    skipme\n    other\n";
        let options = ini_section(text, "pytest").unwrap();
        assert_eq!(options["python_files"], vec!["check_*.py"]);
        assert_eq!(options["norecursedirs"], vec!["skipme", "other"]);
    }

    #[test]
    fn missing_section_is_none() {
        assert!(ini_section("[other]\nx = 1\n", "pytest").is_none());
    }

    #[test]
    fn pyproject_arrays_and_strings() {
        let text = "[tool.pytest.ini_options]\npython_files = [\"check_*.py\", \"spec_*.py\"]\ntestpaths = \"suite lib\"\n";
        let options = pyproject_options(text).unwrap();
        assert_eq!(options["python_files"], vec!["check_*.py", "spec_*.py"]);
        assert_eq!(options["testpaths"], vec!["suite", "lib"]);
    }

    #[test]
    fn pyproject_tool_pytest_table() {
        let text = "[tool.pytest]\npython_classes = [\"Test\", \"Acceptance\"]\ntestpaths = [\"testing\"]\n";
        let options = pyproject_options(text).unwrap();
        assert_eq!(options["python_classes"], vec!["Test", "Acceptance"]);
        assert_eq!(options["testpaths"], vec!["testing"]);
    }

    #[test]
    fn default_patterns() {
        let config = Config::build(PathBuf::from("."), None, HashMap::new());
        assert!(config.is_test_file("test_x.py", None));
        assert!(config.is_test_file("x_test.py", None));
        assert!(!config.is_test_file("x.py", None));
        assert!(config.class_matches("TestFoo"));
        assert!(!config.class_matches("Foo"));
        assert!(config.function_matches("testfoo"));
        assert!(config.function_matches("test_foo"));
        assert!(!config.function_matches("foo_test"));
    }

    #[test]
    fn prefix_and_path_patterns() {
        let mut options = HashMap::new();
        options.insert(
            "python_files".to_string(),
            vec!["test_*.py".to_string(), "testing/python/*.py".to_string()],
        );
        options.insert(
            "python_classes".to_string(),
            vec!["Test".to_string(), "Acceptance".to_string()],
        );
        options.insert(
            "norecursedirs".to_string(),
            vec![".*".to_string(), "testing/example_scripts".to_string()],
        );
        let config = Config::build(PathBuf::from("/r"), None, options);
        assert!(config.is_test_file("approx.py", Some(Path::new("testing/python/approx.py"))));
        assert!(!config.is_test_file("approx.py", Some(Path::new("other/approx.py"))));
        assert!(config.class_matches("AcceptanceThing"));
        assert!(config.skip_dir(
            Path::new("/r/testing/example_scripts"),
            Some(Path::new("testing/example_scripts"))
        ));
        assert!(!config.skip_dir(
            Path::new("/r/testing/other"),
            Some(Path::new("testing/other"))
        ));
    }
}
