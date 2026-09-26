use std::collections::HashMap;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// fnmatch
// ---------------------------------------------------------------------------

/// Python's `fnmatch.fnmatchcase`: `*` matches any run of characters
/// (including `/` — fnmatch knows nothing about separators), `?` one
/// character, `[seq]` / `[!seq]` a character class with `a-z` ranges. There
/// is no brace alternation, so pytest's default `{arch}` is a literal name.
/// An unterminated `[` is a literal bracket.
pub fn fnmatch(name: &str, pattern: &str) -> bool {
    let tokens = fn_tokens(pattern);
    let name: Vec<char> = name.chars().collect();
    fn_match(&tokens, &name)
}

enum FnToken {
    Lit(char),
    One,
    Any,
    /// (negated, inclusive ranges); a single char is the range (c, c).
    Class(bool, Vec<(char, char)>),
}

fn fn_tokens(pattern: &str) -> Vec<FnToken> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        i += 1;
        match c {
            '*' => {
                if !matches!(tokens.last(), Some(FnToken::Any)) {
                    tokens.push(FnToken::Any);
                }
            }
            '?' => tokens.push(FnToken::One),
            '[' => {
                // fnmatch.translate: optional `!`, then a leading `]` is
                // literal, then scan to the closing `]`.
                let mut j = i;
                if j < chars.len() && chars[j] == '!' {
                    j += 1;
                }
                if j < chars.len() && chars[j] == ']' {
                    j += 1;
                }
                while j < chars.len() && chars[j] != ']' {
                    j += 1;
                }
                if j >= chars.len() {
                    tokens.push(FnToken::Lit('['));
                    continue;
                }
                let mut body = &chars[i..j];
                i = j + 1;
                let negated = body.first() == Some(&'!');
                if negated {
                    body = &body[1..];
                }
                let mut ranges = Vec::new();
                let mut k = 0;
                while k < body.len() {
                    if k + 2 < body.len() && body[k + 1] == '-' {
                        // Reversed ranges are empty (Python drops them).
                        if body[k] <= body[k + 2] {
                            ranges.push((body[k], body[k + 2]));
                        }
                        k += 3;
                    } else {
                        ranges.push((body[k], body[k]));
                        k += 1;
                    }
                }
                tokens.push(FnToken::Class(negated, ranges));
            }
            c => tokens.push(FnToken::Lit(c)),
        }
    }
    tokens
}

fn fn_match(tokens: &[FnToken], name: &[char]) -> bool {
    // Classic star backtracking: remember the last `*` and retry it one
    // character further on a mismatch.
    let (mut t, mut n) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        let step = match tokens.get(t) {
            Some(FnToken::Any) => {
                star = Some((t, n));
                t += 1;
                continue;
            }
            Some(FnToken::Lit(c)) => *c == name[n],
            Some(FnToken::One) => true,
            Some(FnToken::Class(negated, ranges)) => {
                let c = name[n];
                ranges.iter().any(|&(lo, hi)| lo <= c && c <= hi) != *negated
            }
            None => false,
        };
        if step {
            t += 1;
            n += 1;
        } else if let Some((st, sn)) = star {
            t = st + 1;
            n = sn + 1;
            star = Some((st, sn + 1));
        } else {
            return false;
        }
    }
    tokens[t..].iter().all(|tok| matches!(tok, FnToken::Any))
}

/// `fnmatch.fnmatch`: `os.path.normcase` first, which on Windows lowercases
/// and unifies separators; elsewhere a plain case-sensitive match.
fn fnmatch_normcase(name: &str, pattern: &str) -> bool {
    if cfg!(windows) {
        fnmatch(
            &name.to_lowercase().replace('\\', "/"),
            &pattern.to_lowercase().replace('\\', "/"),
        )
    } else {
        fnmatch(name, pattern)
    }
}

// ---------------------------------------------------------------------------
// Collection patterns
// ---------------------------------------------------------------------------

/// pytest name patterns (`python_classes`, `python_functions`): a name
/// matches if it starts with the pattern, or — for patterns that look like
/// globs — if it fnmatches it (`_matches_prefix_or_glob_option`).
#[derive(Clone)]
struct NamePattern(String);

impl NamePattern {
    fn matches(&self, name: &str) -> bool {
        name.starts_with(&self.0)
            || (self.0.contains(['*', '?', '[']) && fnmatch_normcase(name, &self.0))
    }
}

/// pytest's `fnmatch_ex` (python_files, norecursedirs): a pattern without a
/// path separator matches the basename; one with a separator matches the
/// whole absolute path, with `*/` prepended when the pattern is relative
/// (so `gen/out` matches any `.../gen/out`, at any depth).
#[derive(Clone)]
struct PathPatterns {
    names: Vec<String>,
    paths: Vec<String>,
}

impl PathPatterns {
    fn new(patterns: &[String]) -> PathPatterns {
        let mut names = Vec::new();
        let mut paths = Vec::new();
        for pattern in patterns {
            let has_sep = pattern.contains('/') || (cfg!(windows) && pattern.contains('\\'));
            if !has_sep {
                names.push(pattern.clone());
            } else if Path::new(pattern).is_absolute() {
                paths.push(pattern.clone());
            } else {
                paths.push(format!("*/{pattern}"));
            }
        }
        PathPatterns { names, paths }
    }

    fn matches(&self, name: &str, abs: Option<&Path>) -> bool {
        if self.names.iter().any(|p| fnmatch_normcase(name, p)) {
            return true;
        }
        let Some(abs) = abs else {
            return false;
        };
        let full = abs.to_string_lossy();
        self.paths.iter().any(|p| fnmatch_normcase(&full, p))
    }
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

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
    /// addopts entries (shlex-split, or the TOML array) — consulted for
    /// `-k`/`-m`/`--ignore` and to warn about interactions (xdist's -n).
    pub addopts: Vec<String>,
    /// A configuration error pytest would refuse to start with (malformed
    /// ini, `[pytest]` in setup.cfg, both `[tool.pytest]` tables, ...).
    /// Collection must not proceed when this is set.
    pub error: Option<String>,
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

/// The options cito reads; all are pytest ini type "args".
const ARGS_OPTIONS: &[&str] = &[
    "addopts",
    "python_files",
    "python_classes",
    "python_functions",
    "norecursedirs",
    "testpaths",
];

type Options = HashMap<String, Vec<String>>;

fn defaults(patterns: &[&str]) -> Vec<String> {
    patterns.iter().map(|s| s.to_string()).collect()
}

/// Python's `shlex.split` (POSIX mode, no comments): whitespace separates
/// tokens; single quotes are literal; inside double quotes a backslash only
/// escapes `"` and `\`; outside quotes a backslash escapes any character;
/// `''` yields an empty token. An unclosed quote (a ValueError in Python)
/// is tolerated by taking the rest of the input.
pub fn shlex_split(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_token = false;
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' | '\r' | '\n' => {
                if in_token {
                    out.push(std::mem::take(&mut current));
                    in_token = false;
                }
            }
            '\\' => {
                in_token = true;
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            '\'' => {
                in_token = true;
                for q in chars.by_ref() {
                    if q == '\'' {
                        break;
                    }
                    current.push(q);
                }
            }
            '"' => {
                in_token = true;
                while let Some(q) = chars.next() {
                    match q {
                        '"' => break,
                        '\\' if matches!(chars.peek(), Some('"' | '\\')) => {
                            current.push(chars.next().expect("peeked"));
                        }
                        q => current.push(q),
                    }
                }
            }
            c => {
                in_token = true;
                current.push(c);
            }
        }
    }
    if in_token {
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
        let failed = |root: PathBuf, err: String| {
            let mut config = Config::build(root, None, HashMap::new());
            config.error = Some(err);
            config
        };

        match locate_config(std::slice::from_ref(&ancestor)) {
            Ok(Some((root, source, options))) => return Config::build(root, Some(source), options),
            Ok(None) => {}
            Err(err) => return failed(ancestor, err),
        }
        for dir in ancestor.ancestors() {
            if dir.join("setup.py").is_file() {
                return Config::build(dir.to_path_buf(), None, HashMap::new());
            }
        }
        if dirs.as_slice() != std::slice::from_ref(&ancestor) && !dirs.is_empty() {
            match locate_config(&dirs) {
                Ok(Some((root, source, options))) => {
                    return Config::build(root, Some(source), options)
                }
                Ok(None) => {}
                Err(err) => return failed(ancestor, err),
            }
        }
        let mut root =
            common_ancestor(&invocation_dir, &[invocation_dir.clone(), ancestor.clone()]);
        if root.parent().is_none() {
            root = ancestor;
        }
        Config::build(root, None, HashMap::new())
    }

    fn build(rootdir: PathBuf, source: Option<PathBuf>, options: Options) -> Config {
        let get = |key: &str, fallback: &[&str]| -> Vec<String> {
            options
                .get(key)
                .cloned()
                .unwrap_or_else(|| defaults(fallback))
        };
        // `__pycache__` never holds sources, so skip it unconditionally.
        let mut norecurse = get("norecursedirs", DEFAULT_NORECURSE);
        norecurse.push("__pycache__".to_string());
        let names = |key: &str, fallback: &[&str]| -> Vec<NamePattern> {
            get(key, fallback).into_iter().map(NamePattern).collect()
        };
        Config {
            testpaths: options.get("testpaths").cloned().unwrap_or_default(),
            addopts: options.get("addopts").cloned().unwrap_or_default(),
            error: None,
            python_files: PathPatterns::new(&get("python_files", DEFAULT_FILES)),
            python_classes: names("python_classes", DEFAULT_CLASSES),
            python_functions: names("python_functions", DEFAULT_FUNCTIONS),
            norecursedirs: PathPatterns::new(&norecurse),
            rootdir,
            source,
        }
    }

    /// `rel` is the rootdir-relative path, when the file is under rootdir;
    /// separator-containing patterns match against the absolute path.
    pub fn is_test_file(&self, name: &str, rel: Option<&Path>) -> bool {
        let abs = rel.map(|rel| self.rootdir.join(rel));
        name.ends_with(".py") && self.python_files.matches(name, abs.as_deref())
    }

    pub fn class_matches(&self, name: &str) -> bool {
        self.python_classes.iter().any(|p| p.matches(name))
    }

    pub fn function_matches(&self, name: &str) -> bool {
        self.python_functions.iter().any(|p| p.matches(name))
    }

    /// `dir` is absolute (pytest matches `norecursedirs` via `fnmatch_ex`
    /// against the absolute collection path); `_rel` is kept for API
    /// compatibility.
    pub fn skip_dir(&self, dir: &Path, _rel: Option<&Path>) -> bool {
        let Some(name) = dir.file_name().and_then(|n| n.to_str()) else {
            return false;
        };
        // pytest also refuses to descend into virtualenvs regardless of name.
        self.norecursedirs.matches(name, Some(dir)) || dir.join("pyvenv.cfg").is_file()
    }

    pub fn relative_to_root<'a>(&self, abs: &'a Path) -> Option<&'a Path> {
        abs.strip_prefix(&self.rootdir).ok()
    }

    /// The value of a flag inside addopts; last one wins, mirroring how
    /// pytest prepends addopts to the CLI.
    pub fn addopts_flag(&self, flag: &str) -> Option<String> {
        self.addopts_values(flag).pop()
    }

    /// Every value of `flag` in addopts, in order, parsed the way argparse
    /// does: `-k X`, `-kX`, `-k=X` for short flags; `--flag X`,
    /// `--flag=X` for long ones.
    pub fn addopts_values(&self, flag: &str) -> Vec<String> {
        let short = flag.len() == 2 && flag.starts_with('-') && !flag.starts_with("--");
        let mut found = Vec::new();
        let mut iter = self.addopts.iter();
        while let Some(arg) = iter.next() {
            if arg == flag {
                if let Some(value) = iter.next() {
                    found.push(value.clone());
                }
            } else if let Some(rest) = arg.strip_prefix(flag) {
                if short {
                    // argparse splits `-k=X` on the first `=`; `-kX` is
                    // an attached value.
                    found.push(rest.strip_prefix('=').unwrap_or(rest).to_string());
                } else if let Some(value) = rest.strip_prefix('=') {
                    found.push(value.to_string());
                }
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

type Located = Option<(PathBuf, PathBuf, Options)>;

/// pytest's `locate_config`: walk each arg and its parents, trying the
/// config names in order. A section-less pyproject.toml never wins directly
/// but the first one seen becomes the fallback rootdir anchor. Like pytest,
/// the lower-priority config files next to the winner are loaded too (to
/// warn about them), so their errors are fatal as well.
fn locate_config(args: &[PathBuf]) -> Result<Located, String> {
    let mut bare_pyproject: Option<PathBuf> = None;
    for arg in args {
        let chain = std::iter::once(arg.as_path()).chain(arg.ancestors().skip(1));
        for base in chain {
            for (index, name) in CONFIG_NAMES.iter().enumerate() {
                let candidate = base.join(name);
                if !candidate.is_file() {
                    continue;
                }
                if *name == "pyproject.toml" && bare_pyproject.is_none() {
                    bare_pyproject = Some(candidate.clone());
                }
                if let Some(options) = load_config_file(&candidate)? {
                    for remainder in &CONFIG_NAMES[index + 1..] {
                        let other = base.join(remainder);
                        if other.is_file() {
                            load_config_file(&other)?;
                        }
                    }
                    return Ok(Some((base.to_path_buf(), candidate, options)));
                }
            }
        }
    }
    Ok(bare_pyproject.map(|p| {
        let parent = p.parent().unwrap_or(Path::new("/")).to_path_buf();
        (parent, p, HashMap::new())
    }))
}

/// pytest's `load_config_dict_from_file`: Ok(None) = this file is not a
/// pytest config (a bare pyproject.toml, a tox.ini without [pytest], ...);
/// Err = pytest would abort with this message.
fn load_config_file(path: &Path) -> Result<Option<Options>, String> {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return Ok(None);
    };
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let display = path.display();
    if name.ends_with(".ini") {
        let sections = parse_ini(&text, &display.to_string())?;
        if let Some(values) = sections.get("pytest") {
            return Ok(Some(ini_options(values)));
        }
        let always = matches!(name, "pytest.ini" | ".pytest.ini");
        return Ok(always.then(HashMap::new));
    }
    if name.ends_with(".cfg") {
        let sections = parse_ini(&text, &display.to_string())?;
        if let Some(values) = sections.get("tool:pytest") {
            return Ok(Some(ini_options(values)));
        }
        if sections.contains_key("pytest") {
            return Err(format!(
                "[pytest] section in {name} files is no longer supported, \
                 change to [tool:pytest] instead."
            ));
        }
        return Ok(None);
    }
    if name.ends_with(".toml") {
        let value: toml::Table = text.parse().map_err(|e| format!("{display}: {e}"))?;
        if matches!(name, "pytest.toml" | ".pytest.toml") {
            return match value.get("pytest").and_then(|v| v.as_table()) {
                Some(table) if !table.is_empty() => toml_native_options(table, path).map(Some),
                _ => Ok(Some(HashMap::new())),
            };
        }
        return pyproject_options(&value, path);
    }
    Ok(None)
}

/// `[tool.pytest]` (native TOML mode) or `[tool.pytest.ini_options]` (ini
/// mode). Both at once is a pytest 9 usage error; a `[tool.pytest]` holding
/// only an `ini_options` subtable is plain ini mode.
fn pyproject_options(value: &toml::Table, path: &Path) -> Result<Option<Options>, String> {
    let Some(pytest) = value
        .get("tool")
        .and_then(|t| t.as_table())
        .and_then(|t| t.get("pytest"))
        .and_then(|t| t.as_table())
    else {
        return Ok(None);
    };
    let native: toml::Table = pytest
        .iter()
        .filter(|(k, _)| k.as_str() != "ini_options")
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let ini = pytest.get("ini_options");
    let ini_nonempty = ini.is_some_and(|v| v.as_table().is_none_or(|t| !t.is_empty()));
    if !native.is_empty() && ini_nonempty {
        return Err(format!(
            "{}: Cannot use both [tool.pytest] (native TOML types) and \
             [tool.pytest.ini_options] (string-based INI format) simultaneously. \
             Please use [tool.pytest] with native TOML types (recommended) \
             or [tool.pytest.ini_options] for backwards compatibility.",
            path.display()
        ));
    }
    if !native.is_empty() {
        return toml_native_options(&native, path).map(Some);
    }
    match ini.and_then(|v| v.as_table()) {
        Some(table) => Ok(Some(toml_ini_options(table))),
        None => Ok(None),
    }
}

/// Python's `type(v).__name__` / `repr(v)` for the TOML value, for messages.
fn py_describe(value: &toml::Value) -> (&'static str, String) {
    match value {
        toml::Value::String(s) => ("str", format!("'{s}'")),
        toml::Value::Integer(i) => ("int", i.to_string()),
        toml::Value::Float(f) => ("float", f.to_string()),
        toml::Value::Boolean(b) => ("bool", if *b { "True" } else { "False" }.to_string()),
        toml::Value::Datetime(d) => ("datetime", d.to_string()),
        toml::Value::Array(_) => ("list", value.to_string()),
        toml::Value::Table(_) => ("dict", value.to_string()),
    }
}

/// Native TOML mode: "args" options must be lists of strings — pytest
/// raises a TypeError otherwise, so cito refuses too.
fn toml_native_options(table: &toml::Table, path: &Path) -> Result<Options, String> {
    let mut options = HashMap::new();
    for key in ARGS_OPTIONS {
        let Some(value) = table.get(*key) else {
            continue;
        };
        let Some(items) = value.as_array() else {
            let (ty, repr) = py_describe(value);
            return Err(format!(
                "{}: config option '{key}' expects a list for type 'args', got {ty}: {repr}",
                path.display()
            ));
        };
        let mut values = Vec::new();
        for (i, item) in items.iter().enumerate() {
            match item.as_str() {
                Some(s) => values.push(s.to_string()),
                None => {
                    let (ty, repr) = py_describe(item);
                    return Err(format!(
                        "{}: config option '{key}' expects a list of strings, \
                         but item at index {i} is {ty}: {repr}",
                        path.display()
                    ));
                }
            }
        }
        options.insert(key.to_string(), values);
    }
    Ok(options)
}

/// `[tool.pytest.ini_options]`: lists are used as-is, scalars are
/// stringified (`str(v)`) and then shlex-split like any ini value.
fn toml_ini_options(table: &toml::Table) -> Options {
    let mut options = HashMap::new();
    for key in ARGS_OPTIONS {
        let values = match table.get(*key) {
            None => continue,
            Some(toml::Value::Array(items)) => items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            Some(toml::Value::String(s)) => shlex_split(s),
            Some(other) => shlex_split(&py_describe(other).1),
        };
        options.insert(key.to_string(), values);
    }
    options
}

/// Ini-mode values: every option cito reads is type "args" (shlex-split).
fn ini_options(values: &HashMap<String, String>) -> Options {
    ARGS_OPTIONS
        .iter()
        .filter_map(|key| values.get(*key).map(|v| (key.to_string(), shlex_split(v))))
        .collect()
}

/// A parsed ini line: (lineno, section, Some((name, value))) for a value,
/// None for a section header.
type IniEntry = (usize, String, Option<(String, String)>);

/// A port of iniconfig 2.x as pytest invokes it (`IniConfig(path)`: inline
/// value comments are kept, section headers may carry a trailing `#`/`;`
/// comment, names are not trimmed inside the brackets). `key = value` and
/// `key: value` both work; indented lines continue the previous value
/// (joined with `\n`). Malformed input is an error, as in pytest.
fn parse_ini(text: &str, path: &str) -> Result<HashMap<String, HashMap<String, String>>, String> {
    let error = |lineno: usize, msg: String| format!("{path}:{}: {msg}", lineno + 1);
    // Parsed lines: (lineno, section, Some((name, value))) or a header.
    let mut entries: Vec<IniEntry> = Vec::new();
    let mut section: Option<String> = None;
    for (lineno, raw) in text.lines().enumerate() {
        let first = raw.trim_start().chars().next();
        if matches!(first, Some('#' | ';')) {
            continue;
        }
        let line = raw.trim_end();
        if line.is_empty() {
            continue;
        }
        let mut continuation: Option<String> = None;
        if line.starts_with('[') {
            let header = line.split('#').next().unwrap_or("");
            let header = header.split(';').next().unwrap_or("").trim_end();
            if header.ends_with(']') {
                let name = header[1..header.len() - 1].to_string();
                if name.is_empty() {
                    return Err(error(lineno, "empty section name".to_string()));
                }
                section = Some(name.clone());
                entries.push((lineno, name, None));
                continue;
            }
            continuation = Some(line.trim().to_string());
        } else if !line.starts_with(char::is_whitespace) {
            let (name, value) = match line.split_once('=') {
                Some((name, value)) if !name.contains(':') => (name, value),
                _ => match line.split_once(':') {
                    Some(pair) => pair,
                    None => return Err(error(lineno, format!("unexpected line: '{line}'"))),
                },
            };
            let Some(current) = &section else {
                return Err(error(lineno, "no section header defined".to_string()));
            };
            let pair = (name.trim().to_string(), value.trim().to_string());
            entries.push((lineno, current.clone(), Some(pair)));
            continue;
        }
        let data = continuation.unwrap_or_else(|| line.trim().to_string());
        match entries.last_mut() {
            Some((_, _, Some((_, value)))) => {
                if value.is_empty() {
                    *value = data;
                } else {
                    value.push('\n');
                    value.push_str(&data);
                }
            }
            _ => return Err(error(lineno, "unexpected value continuation".to_string())),
        }
    }
    let mut sections: HashMap<String, HashMap<String, String>> = HashMap::new();
    for (lineno, section, pair) in entries {
        match pair {
            None => {
                if sections.contains_key(&section) {
                    return Err(error(lineno, format!("duplicate section '{section}'")));
                }
                sections.insert(section, HashMap::new());
            }
            Some((name, value)) => {
                let values = sections.entry(section).or_default();
                if values.contains_key(&name) {
                    return Err(error(lineno, format!("duplicate name '{name}'")));
                }
                values.insert(name, value);
            }
        }
    }
    Ok(sections)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ini_section(text: &str, section: &str) -> Option<Options> {
        parse_ini(text, "t.ini")
            .unwrap()
            .get(section)
            .map(ini_options)
    }

    fn pyproject(text: &str) -> Result<Option<Options>, String> {
        pyproject_options(&text.parse().unwrap(), Path::new("pyproject.toml"))
    }

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
    fn iniconfig_header_comments_colons_and_spaces() {
        let text = "[pytest]  # main\npython_files: check_*.py\n";
        let options = ini_section(text, "pytest").unwrap();
        assert_eq!(options["python_files"], vec!["check_*.py"]);
        // Section names are not trimmed.
        assert!(ini_section("[ pytest ]\npython_files = x\n", "pytest").is_none());
        // `=` wins unless the name part contains `:`.
        let sections = parse_ini("[s]\na = b:c\nd:e=f\n", "t.ini").unwrap();
        assert_eq!(sections["s"]["a"], "b:c");
        assert_eq!(sections["s"]["d"], "e=f");
    }

    #[test]
    fn iniconfig_errors() {
        assert!(parse_ini("[pytest]\nfoo\n", "t.ini")
            .unwrap_err()
            .ends_with("t.ini:2: unexpected line: 'foo'"));
        assert!(parse_ini("x = 1\n", "t.ini").is_err());
        assert!(parse_ini("[a]\n  cont\n", "t.ini").is_err());
        assert!(parse_ini("[a]\n[a]\n", "t.ini").is_err());
        assert!(parse_ini("[a]\nx=1\nx=2\n", "t.ini").is_err());
    }

    #[test]
    fn shlex_semantics() {
        assert_eq!(
            shlex_split(r#"-m 'not stress' "a b" c\ d e"f"g '' x"\"y""#),
            vec!["-m", "not stress", "a b", "c d", "efg", "", "x\"y"]
        );
        assert_eq!(shlex_split(r#""a\b""#), vec![r"a\b"]);
        let options = ini_section("[pytest]\npython_files = 'my tests*.py'\n", "pytest").unwrap();
        assert_eq!(options["python_files"], vec!["my tests*.py"]);
    }

    #[test]
    fn pyproject_arrays_and_strings() {
        let text = "[tool.pytest.ini_options]\npython_files = [\"check_*.py\", \"spec_*.py\"]\ntestpaths = \"suite lib\"\n";
        let options = pyproject(text).unwrap().unwrap();
        assert_eq!(options["python_files"], vec!["check_*.py", "spec_*.py"]);
        assert_eq!(options["testpaths"], vec!["suite", "lib"]);
    }

    #[test]
    fn pyproject_tool_pytest_table() {
        let text = "[tool.pytest]\npython_classes = [\"Test\", \"Acceptance\"]\ntestpaths = [\"testing\"]\n";
        let options = pyproject(text).unwrap().unwrap();
        assert_eq!(options["python_classes"], vec!["Test", "Acceptance"]);
        assert_eq!(options["testpaths"], vec!["testing"]);
    }

    #[test]
    fn pyproject_errors() {
        let both = "[tool.pytest]\nx = [1]\n[tool.pytest.ini_options]\ny = 1\n";
        assert!(pyproject(both).unwrap_err().contains("Cannot use both"));
        let string = "[tool.pytest]\ntestpaths = \"tests\"\n";
        assert!(pyproject(string)
            .unwrap_err()
            .contains("expects a list for type 'args', got str: 'tests'"));
        // Only ini_options (even empty) is ini mode; no pytest table is None.
        assert!(pyproject("[tool.pytest.ini_options]\n").unwrap().is_some());
        assert!(pyproject("[tool.black]\n").unwrap().is_none());
    }

    #[test]
    fn fnmatch_semantics() {
        assert!(fnmatch("{arch}", "{arch}"));
        assert!(!fnmatch("arch", "{arch}"));
        assert!(fnmatch("a/b/c.py", "*.py"));
        assert!(fnmatch("x1", "x[0-9]"));
        assert!(!fnmatch("xa", "x[0-9]"));
        assert!(fnmatch("xa", "x[!0-9]"));
        assert!(fnmatch("x]", "x[]]"));
        assert!(fnmatch("x-", "x[a-]"));
        assert!(fnmatch("[ab", "[ab"));
        assert!(fnmatch("aXbXc", "a*b*c"));
        assert!(!fnmatch("abc", "a?"));
        assert!(fnmatch("", "*"));
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
        assert!(config.skip_dir(Path::new("/r/{arch}"), None));
        assert!(!config.skip_dir(Path::new("/r/arch"), None));
    }

    #[test]
    fn addopts_forms() {
        let mut config = Config::build(PathBuf::from("."), None, HashMap::new());
        config.addopts = shlex_split("-kfoo -m=slow --ignore a --ignore=b -k 'x or y'");
        assert_eq!(config.addopts_flag("-k").as_deref(), Some("x or y"));
        assert_eq!(config.addopts_flag("-m").as_deref(), Some("slow"));
        assert_eq!(config.addopts_values("-k"), vec!["foo", "x or y"]);
        assert_eq!(config.addopts_values("--ignore"), vec!["a", "b"]);
        config.addopts = shlex_split("--ignore-glob=x");
        assert!(config.addopts_values("--ignore").is_empty());
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
        // `*/` prefix: the pattern matches at any depth.
        assert!(config.is_test_file("a.py", Some(Path::new("x/testing/python/a.py"))));
        assert!(config.class_matches("AcceptanceThing"));
        assert!(config.skip_dir(
            Path::new("/r/testing/example_scripts"),
            Some(Path::new("testing/example_scripts"))
        ));
        assert!(config.skip_dir(Path::new("/r/deep/testing/example_scripts"), None));
        assert!(!config.skip_dir(
            Path::new("/r/testing/other"),
            Some(Path::new("testing/other"))
        ));
    }
}
