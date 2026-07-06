use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use ignore::WalkBuilder;
use rayon::prelude::*;
use ruff_python_ast::{self as ast, Expr, Stmt};
use serde::Serialize;

use crate::config::Config;
use crate::params::{self, Expansion};

#[derive(Debug, Serialize, serde::Deserialize)]
pub struct FileTests {
    /// rootdir-relative, forward-slash path — the node ID prefix.
    pub path: String,
    /// Absolute path, used to build node IDs that pytest can run from any cwd.
    pub abs_path: PathBuf,
    pub tests: Vec<String>,
}

// ---------------------------------------------------------------------------
// Parsed module model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum ModuleRef {
    /// Dotted module path resolved against the rootdir (and `src/` layout).
    Absolute(String),
    /// Filesystem base (no extension) from a relative import.
    Relative(PathBuf),
}

#[derive(Debug, Clone)]
enum Import {
    /// `import a.b [as x]` — binding maps to a dotted module.
    Module(String),
    /// `from M import name [as x]` — name may be a class or a submodule.
    From(ModuleRef, String),
}

/// One test function/method: its parametrize expansion plus the parameter
/// names it requests (fixtures) and the names claimed by parametrize.
#[derive(Debug, Clone)]
struct TestDef {
    name: String,
    expansion: Expansion,
    args: Vec<String>,
    claimed: Vec<String>,
    marks: Vec<String>,
    maybe_marks: Vec<String>,
    /// The function body contains a `pytest.skip(...)` call — calling it at
    /// module level skips the whole module.
    skips_module: bool,
    /// Body is exactly `return <expr>` with a constant-evaluable expr —
    /// usable as a platform predicate (`if is_win32():`).
    returns_const: Option<bool>,
}

#[derive(Debug)]
enum ClassItem {
    Method(TestDef),
    Nested(String, Class),
}

#[derive(Debug)]
struct Class {
    bases: Vec<String>,
    items: Vec<ClassItem>,
    fixtures: HashMap<String, Fixture>,
    usefixtures: Vec<String>,
    marks: Vec<String>,
    expansion: Expansion,
    has_ctor: bool,
}

#[derive(Debug)]
enum TopItem {
    Func(TestDef),
    Class(String),
}

/// A branch guard resolvable only with cross-file or environment knowledge.
#[derive(Debug, Clone)]
enum DeferredGuard {
    /// `if predicate():` where predicate is a (possibly imported) function.
    Call { name: String, negated: bool },
    /// `if binding:` where `binding = import_module("mod")`.
    Binding { module: String, negated: bool },
}

#[derive(Debug, Clone)]
struct Fixture {
    parametrized: bool,
    autouse: bool,
    deps: Vec<String>,
}

/// A parametrized autouse fixture parametrizes every test in its scope.
fn has_autouse_params(fixtures: &HashMap<String, Fixture>) -> bool {
    fixtures.values().any(|f| f.autouse && f.parametrized)
}

#[derive(Debug)]
struct Module {
    path: PathBuf,
    dir: PathBuf,
    imports: HashMap<String, Import>,
    star_imports: Vec<ModuleRef>,
    classes: HashMap<String, Class>,
    functions: HashMap<String, TestDef>,
    fixtures: HashMap<String, Fixture>,
    order: Vec<TopItem>,
    /// Module names demanded via module-level `pytest.importorskip(...)`.
    skip_requires: Vec<String>,
    /// A module-level `pytest.skip(...)` call (possibly behind an `if`):
    /// under a default invocation the module opts out of collection.
    has_module_skip: bool,
    /// Bare helper calls at module level — possibly imported skip wrappers.
    helper_calls: Vec<String>,
    /// `NAME = import_module('mod')` / importorskip bindings.
    import_bindings: HashMap<String, String>,
    /// `X = Machine.TestCase` synthetic unittest classes.
    synthetic_testcases: Vec<String>,
    /// Top-level defs guarded by a condition we can only resolve at emit
    /// time (imported predicates, import-availability bindings).
    cond_blocks: Vec<(DeferredGuard, Vec<String>)>,
    /// Names defined on unconditional top-level paths (never deadened).
    certain_names: HashSet<String>,
    /// Module-level `pytestmark = ...` mark names.
    pytestmark: Vec<String>,
    /// `slow = pytest.mark.slow` style aliases defined in this module.
    mark_aliases: HashMap<String, String>,
    /// `pytest_plugins = [...]` declarations (conftest only, per pytest).
    plugin_modules: Vec<String>,
    /// conftest.py `collect_ignore` / `collect_ignore_glob` (literal lists).
    collect_ignore: Vec<String>,
    collect_ignore_glob: Vec<String>,
    /// A `pytest_generate_tests` hook here parametrizes tests in ways static
    /// analysis cannot see; all expansions in scope must fall back.
    has_generate_tests: bool,
}

/// Does this test transitively request a parametrized fixture visible in any
/// of `contexts` (its module plus the conftest chain)? If so, pytest will
/// append ID pieces we cannot see statically, so the test's expansion must
/// fall back to the bare name.
fn requests_parametrized_fixture(
    contexts: &[Rc<Module>],
    class_fixtures: &[&HashMap<String, Fixture>],
    def: &TestDef,
) -> bool {
    let lookup = |name: &str| {
        class_fixtures
            .iter()
            .find_map(|f| f.get(name))
            .or_else(|| contexts.iter().find_map(|m| m.fixtures.get(name)))
    };
    let mut queue: Vec<&str> = def
        .args
        .iter()
        .map(String::as_str)
        .filter(|a| {
            !matches!(*a, "self" | "cls" | "request") && !def.claimed.iter().any(|c| c == a)
        })
        .collect();
    let mut seen: HashSet<&str> = HashSet::new();
    while let Some(name) = queue.pop() {
        if !seen.insert(name) {
            continue;
        }
        // The anyio plugin's backend fixtures are parametrized by the
        // plugin itself; reaching one (directly or transitively) means the
        // plugin will add ID pieces we cannot see.
        if matches!(
            name,
            "anyio_backend" | "anyio_backend_name" | "anyio_backend_options"
        ) {
            return true;
        }
        if let Some(fixture) = lookup(name) {
            if fixture.parametrized {
                return true;
            }
            queue.extend(
                fixture
                    .deps
                    .iter()
                    .map(String::as_str)
                    .filter(|d| !matches!(*d, "self" | "cls" | "request")),
            );
        }
    }
    false
}

fn parameter_names(parameters: &ast::Parameters) -> Vec<String> {
    parameters
        .posonlyargs
        .iter()
        .chain(parameters.args.iter())
        .chain(parameters.kwonlyargs.iter())
        .map(|p| p.parameter.name.to_string())
        .collect()
}

fn test_def(func: &ast::StmtFunctionDef, aliases: &params::ParamAliases) -> TestDef {
    let info = params::from_decorators(&func.decorator_list, aliases);
    let mut args = parameter_names(&func.parameters);
    args.extend(info.extra_fixture_requests);
    TestDef {
        name: func.name.to_string(),
        expansion: info.expansion,
        args,
        claimed: params::decorator_argnames(&func.decorator_list),
        marks: info.marks,
        maybe_marks: info.unresolved,
        skips_module: body_calls_skip(&func.body),
        returns_const: const_return(&func.body),
    }
}

/// `def f(): return <evaluable>` — the constant truth of the return value.
fn const_return(body: &[Stmt]) -> Option<bool> {
    match body {
        [Stmt::Return(ret)] => ret.value.as_deref().and_then(eval_condition),
        _ => None,
    }
}

/// Does this statement list (recursively) contain a `pytest.skip(...)` call?
fn body_calls_skip(body: &[Stmt]) -> bool {
    body.iter().any(|stmt| match stmt {
        Stmt::Expr(expr) => is_module_skip_call(&expr.value),
        Stmt::If(if_stmt) => {
            body_calls_skip(&if_stmt.body)
                || if_stmt
                    .elif_else_clauses
                    .iter()
                    .any(|c| body_calls_skip(&c.body))
        }
        Stmt::Try(try_stmt) => {
            body_calls_skip(&try_stmt.body)
                || try_stmt.handlers.iter().any(|h| {
                    let ast::ExceptHandler::ExceptHandler(h) = h;
                    body_calls_skip(&h.body)
                })
                || body_calls_skip(&try_stmt.orelse)
                || body_calls_skip(&try_stmt.finalbody)
        }
        Stmt::With(with_stmt) => body_calls_skip(&with_stmt.body),
        _ => false,
    })
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// Discover test files under `roots` (canonicalized, so results are absolute
/// and rootdir-relative matching works). Explicitly-passed files are always
/// collected, matching pytest.
fn discover(roots: &[PathBuf], config: &Config) -> Vec<PathBuf> {
    let cwd = std::env::current_dir().unwrap_or_default();
    let mut files = Vec::new();
    for root in roots {
        let abs = if root.is_absolute() {
            root.clone()
        } else {
            cwd.join(root)
        };
        let abs = abs.canonicalize().unwrap_or(abs);
        if abs.is_file() {
            files.push(abs);
            continue;
        }
        let walker = WalkBuilder::new(&abs)
            .standard_filters(false)
            .hidden(false)
            // pytest follows symlinked directories during collection
            // (pydantic vendors pydantic-core's tests as a symlink).
            .follow_links(true)
            .filter_entry({
                let config = config.clone();
                move |entry| {
                    entry.depth() == 0
                        || !(entry.file_type().is_some_and(|t| t.is_dir())
                            && config.skip_dir(entry.path(), config.relative_to_root(entry.path())))
                }
            })
            .build();
        for entry in walker.flatten() {
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let Some(name) = entry.path().file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if config.is_test_file(name, config.relative_to_root(entry.path())) {
                files.push(entry.into_path());
            }
        }
    }
    files.sort();
    files.dedup();
    files
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

fn parse_file(path: &Path) -> Option<Module> {
    let source = match std::fs::read_to_string(path) {
        Ok(source) => source,
        Err(err) => {
            eprintln!("cito: warning: skipping {} ({err})", path.display());
            return None;
        }
    };
    match parse_source(path, &source) {
        Ok(module) => Some(module),
        Err(err) => {
            eprintln!(
                "cito: warning: skipping {} (parse error: {err})",
                path.display()
            );
            None
        }
    }
}

/// Python shadowing: a later `def`/`class` with the same name replaces the
/// earlier one; pytest collects only the surviving object, located at its
/// last definition site. Keep the LAST occurrence of each name.
fn dedupe_keep_last<T>(items: Vec<T>, name_of: impl Fn(&T) -> &str) -> Vec<T> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut kept: Vec<T> = Vec::new();
    for item in items.into_iter().rev() {
        if seen.insert(name_of(&item).to_string()) {
            kept.push(item);
        }
    }
    kept.reverse();
    kept
}

fn parse_source(path: &Path, source: &str) -> Result<Module, ruff_python_parser::ParseError> {
    let syntax = ruff_python_parser::parse_module(source)?.into_syntax();
    let mut module = Module {
        path: path.to_path_buf(),
        dir: path.parent().unwrap_or(Path::new("")).to_path_buf(),
        imports: HashMap::new(),
        star_imports: Vec::new(),
        classes: HashMap::new(),
        functions: HashMap::new(),
        fixtures: HashMap::new(),
        order: Vec::new(),
        skip_requires: Vec::new(),
        has_module_skip: false,
        helper_calls: Vec::new(),
        import_bindings: HashMap::new(),
        synthetic_testcases: Vec::new(),
        cond_blocks: Vec::new(),
        certain_names: HashSet::new(),
        pytestmark: Vec::new(),
        mark_aliases: HashMap::new(),
        plugin_modules: Vec::new(),
        collect_ignore: Vec::new(),
        collect_ignore_glob: Vec::new(),
        has_generate_tests: false,
    };
    let mut aliases = params::ParamAliases::new();
    scan(&syntax.body, &mut module, &mut aliases, true, true);
    module.order = dedupe_keep_last(std::mem::take(&mut module.order), |item| match item {
        TopItem::Func(def) => def.name.as_str(),
        TopItem::Class(name) => name.as_str(),
    });
    Ok(module)
}

/// Walk statements. Definitions only count at true top level; imports are
/// also harvested from inside `if`/`try`/`with` blocks (the common
/// conditional-import patterns), since over-approximating imports is safe.
fn scan(
    stmts: &[Stmt],
    module: &mut Module,
    aliases: &mut params::ParamAliases,
    top: bool,
    certain: bool,
) {
    for stmt in stmts {
        match stmt {
            Stmt::FunctionDef(func) if top => {
                if certain {
                    module.certain_names.insert(func.name.to_string());
                }
                if func.name.as_str() == "pytest_generate_tests" {
                    module.has_generate_tests = true;
                    continue;
                }
                // Fixtures are never collected as tests, even test-named ones.
                if let Some(flags) = params::fixture_info(&func.decorator_list) {
                    let key = flags.name.unwrap_or_else(|| func.name.to_string());
                    module.fixtures.insert(
                        key,
                        Fixture {
                            parametrized: flags.parametrized,
                            autouse: flags.autouse,
                            deps: parameter_names(&func.parameters),
                        },
                    );
                    continue;
                }
                let def = test_def(func, aliases);
                module.functions.insert(def.name.clone(), def.clone());
                module.order.push(TopItem::Func(def));
            }
            Stmt::ClassDef(class) if top => {
                if certain {
                    module.certain_names.insert(class.name.to_string());
                }
                let name = class.name.to_string();
                module
                    .classes
                    .insert(name.clone(), build_class(class, aliases));
                module.order.push(TopItem::Class(name));
            }
            Stmt::Assign(assign) if top => {
                // `NAME = pytest.mark.parametrize(...)` decorator aliases.
                if let [Expr::Name(target)] = assign.targets.as_slice() {
                    if let Some(alias) = params::parametrize_alias(&assign.value) {
                        aliases.insert(target.id.to_string(), alias);
                    }
                    if target.id.as_str() == "pytest_plugins" {
                        match &*assign.value {
                            Expr::List(l) => module
                                .plugin_modules
                                .extend(l.elts.iter().filter_map(string_value_of)),
                            Expr::Tuple(t) => module
                                .plugin_modules
                                .extend(t.elts.iter().filter_map(string_value_of)),
                            Expr::StringLiteral(v) => {
                                module.plugin_modules.push(v.value.to_str().to_string())
                            }
                            _ => {}
                        }
                    }
                    if target.id.as_str() == "pytestmark" {
                        match &*assign.value {
                            Expr::List(l) => module
                                .pytestmark
                                .extend(l.elts.iter().filter_map(params::mark_name)),
                            other => module.pytestmark.extend(params::mark_name(other)),
                        }
                    }
                    // `slow = pytest.mark.slow` mark aliases.
                    if let Some(mark) = params::mark_name(&assign.value) {
                        module.mark_aliases.insert(target.id.to_string(), mark);
                    }
                    // `cin = import_module('clang.cindex')` availability
                    // binding (sympy idiom) — resolved via the probe.
                    if let Some(name) = import_module_binding(&assign.value) {
                        module.import_bindings.insert(target.id.to_string(), name);
                    }
                    // `TestFoo = SomeStateMachine.TestCase` (hypothesis
                    // stateful idiom): a synthetic unittest class.
                    if let Expr::Attribute(attr) = &*assign.value {
                        if attr.attr.as_str() == "TestCase" {
                            module.synthetic_testcases.push(target.id.to_string());
                        }
                    }
                    // conftest collect_ignore lists (literal entries only).
                    if matches!(target.id.as_str(), "collect_ignore" | "collect_ignore_glob") {
                        let entries = string_list(&assign.value);
                        if target.id.as_str() == "collect_ignore" {
                            module.collect_ignore.extend(entries);
                        } else {
                            module.collect_ignore_glob.extend(entries);
                        }
                    }
                }
                // `mpl = pytest.importorskip("matplotlib")`.
                if let Some(name) = importorskip_name(&assign.value) {
                    module.skip_requires.push(name);
                }
            }
            // Only live (non-dead-branch) statements can skip the module
            // or demand dependencies.
            Stmt::Expr(expr_stmt) if top => {
                // Bare `pytest.importorskip("numba")` at module level.
                if let Some(name) = importorskip_name(&expr_stmt.value) {
                    module.skip_requires.push(name);
                }
                if is_module_skip_call(&expr_stmt.value) {
                    module.has_module_skip = true;
                } else if let Expr::Call(call) = &*expr_stmt.value {
                    let name = match &*call.func {
                        Expr::Name(name) => Some(name.id.to_string()),
                        Expr::Attribute(attr) => Some(attr.attr.to_string()),
                        _ => None,
                    };
                    module.helper_calls.extend(name);
                }
            }
            Stmt::Import(import) => {
                for alias in &import.names {
                    let dotted = alias.name.to_string();
                    match &alias.asname {
                        Some(asname) => {
                            module
                                .imports
                                .insert(asname.to_string(), Import::Module(dotted));
                        }
                        None => {
                            // `import a.b` binds `a`.
                            let first = dotted.split('.').next().unwrap_or("").to_string();
                            module.imports.insert(first.clone(), Import::Module(first));
                        }
                    }
                }
            }
            Stmt::ImportFrom(import) => {
                let base = match import.level {
                    0 => None,
                    level => {
                        let mut dir = module.dir.clone();
                        for _ in 1..level {
                            dir = dir.parent().unwrap_or(Path::new("")).to_path_buf();
                        }
                        Some(dir)
                    }
                };
                let mref = match (&base, &import.module) {
                    (None, Some(m)) => ModuleRef::Absolute(m.to_string()),
                    (None, None) => continue,
                    (Some(dir), Some(m)) => {
                        let mut p = dir.clone();
                        for seg in m.as_str().split('.') {
                            p = p.join(seg);
                        }
                        ModuleRef::Relative(p)
                    }
                    (Some(dir), None) => ModuleRef::Relative(dir.clone()),
                };
                for alias in &import.names {
                    if alias.name.as_str() == "*" {
                        module.star_imports.push(mref.clone());
                        continue;
                    }
                    let local = alias
                        .asname
                        .as_ref()
                        .map(|a| a.to_string())
                        .unwrap_or_else(|| alias.name.to_string());
                    module
                        .imports
                        .insert(local, Import::From(mref.clone(), alias.name.to_string()));
                }
            }
            // Definitions inside top-level if/else and try/except are real
            // module members for whichever branch runs. Guards over
            // sys.platform / os.name / sys.argv are evaluated; branches that
            // are decidably dead contribute imports only. Undecidable
            // branches are all collected, and the keep-last name dedupe
            // resolves the common "same name in both branches" pattern.
            Stmt::If(if_stmt) if top => {
                let cond = eval_condition(&if_stmt.test);
                if cond.is_none() {
                    if let Some(guard) = classify_guard(&if_stmt.test, &module.import_bindings) {
                        let names = defined_names(&if_stmt.body);
                        if !names.is_empty() {
                            module.cond_blocks.push((guard.clone(), names));
                        }
                        // Plain else-branch names carry the inverted guard.
                        for clause in &if_stmt.elif_else_clauses {
                            if clause.test.is_none() {
                                let names = defined_names(&clause.body);
                                if !names.is_empty() {
                                    let inverted = match guard.clone() {
                                        DeferredGuard::Call { name, negated } => {
                                            DeferredGuard::Call {
                                                name,
                                                negated: !negated,
                                            }
                                        }
                                        DeferredGuard::Binding { module, negated } => {
                                            DeferredGuard::Binding {
                                                module,
                                                negated: !negated,
                                            }
                                        }
                                    };
                                    module.cond_blocks.push((inverted, names));
                                }
                            }
                        }
                    }
                }
                scan(
                    &if_stmt.body,
                    module,
                    aliases,
                    cond != Some(false),
                    certain && cond == Some(true),
                );
                // else/elif run when the if-condition is false or unknown;
                // elif conditions are rarely used in these guards, so treat
                // them like the else arm.
                let else_live = cond != Some(true);
                for clause in &if_stmt.elif_else_clauses {
                    let clause_cond = clause.test.as_ref().and_then(eval_condition);
                    let live = else_live && clause_cond != Some(false);
                    let clause_certain = certain
                        && cond == Some(false)
                        && clause
                            .test
                            .as_ref()
                            .map(|t| eval_condition(t) == Some(true))
                            .unwrap_or(true);
                    scan(&clause.body, module, aliases, live, clause_certain);
                }
            }
            Stmt::Try(try_stmt) if top => {
                // `try: import x / except ImportError: pytest.skip(...)` is
                // importorskip in disguise: map it onto the probe.
                let handler_skips = try_stmt.handlers.iter().any(|h| {
                    let ast::ExceptHandler::ExceptHandler(h) = h;
                    body_calls_skip(&h.body)
                });
                if handler_skips {
                    for stmt in &try_stmt.body {
                        match stmt {
                            Stmt::Import(import) => {
                                for alias in &import.names {
                                    module.skip_requires.push(alias.name.to_string());
                                }
                            }
                            Stmt::ImportFrom(import) if import.level == 0 => {
                                if let Some(m) = &import.module {
                                    module.skip_requires.push(m.to_string());
                                }
                            }
                            _ => {}
                        }
                    }
                }
                scan(&try_stmt.body, module, aliases, true, certain);
                for handler in &try_stmt.handlers {
                    let ast::ExceptHandler::ExceptHandler(h) = handler;
                    scan(&h.body, module, aliases, false, false);
                }
                scan(&try_stmt.orelse, module, aliases, true, false);
                scan(&try_stmt.finalbody, module, aliases, true, certain);
            }
            Stmt::If(if_stmt) => {
                scan(&if_stmt.body, module, aliases, false, false);
                for clause in &if_stmt.elif_else_clauses {
                    scan(&clause.body, module, aliases, false, false);
                }
            }
            Stmt::Try(try_stmt) => {
                scan(&try_stmt.body, module, aliases, false, false);
                for handler in &try_stmt.handlers {
                    let ast::ExceptHandler::ExceptHandler(h) = handler;
                    scan(&h.body, module, aliases, false, false);
                }
                scan(&try_stmt.orelse, module, aliases, false, false);
                scan(&try_stmt.finalbody, module, aliases, false, false);
            }
            Stmt::With(with_stmt) => scan(&with_stmt.body, module, aliases, false, certain),
            _ => {}
        }
    }
}

fn build_class(class: &ast::StmtClassDef, aliases: &params::ParamAliases) -> Class {
    let bases = class
        .arguments
        .as_ref()
        .map(|args| args.args.iter().filter_map(base_text).collect())
        .unwrap_or_default();
    let class_info = params::from_decorators(&class.decorator_list, aliases);
    let mut items = Vec::new();
    let mut fixtures = HashMap::new();
    let mut has_ctor = false;
    for stmt in &class.body {
        match stmt {
            Stmt::FunctionDef(func) => {
                let name = func.name.as_str();
                if name == "__init__" || name == "__new__" {
                    has_ctor = true;
                }
                if let Some(flags) = params::fixture_info(&func.decorator_list) {
                    let key = flags.name.unwrap_or_else(|| name.to_string());
                    fixtures.insert(
                        key,
                        Fixture {
                            parametrized: flags.parametrized,
                            autouse: flags.autouse,
                            deps: parameter_names(&func.parameters),
                        },
                    );
                    continue;
                }
                let mut def = test_def(func, aliases);
                // Class-level usefixtures apply to every method.
                def.args
                    .extend(class_info.extra_fixture_requests.iter().cloned());
                items.push(ClassItem::Method(def));
            }
            Stmt::ClassDef(nested) => {
                items.push(ClassItem::Nested(
                    nested.name.to_string(),
                    build_class(nested, aliases),
                ));
            }
            _ => {}
        }
    }
    let items = dedupe_keep_last(items, |item| match item {
        ClassItem::Method(def) => def.name.as_str(),
        ClassItem::Nested(name, _) => name.as_str(),
    });
    Class {
        bases,
        items,
        fixtures,
        usefixtures: class_info.extra_fixture_requests,
        marks: class_info.marks,
        expansion: class_info.expansion,
        has_ctor,
    }
}

fn string_value_of(expr: &Expr) -> Option<String> {
    match expr {
        Expr::StringLiteral(s) => Some(s.value.to_str().to_string()),
        _ => None,
    }
}

/// Literal strings from a list/tuple expression.
fn string_list(expr: &Expr) -> Vec<String> {
    let elements = match expr {
        Expr::List(l) => &l.elts,
        Expr::Tuple(t) => &t.elts,
        _ => return Vec::new(),
    };
    elements
        .iter()
        .filter_map(|e| match e {
            Expr::StringLiteral(s) => Some(s.value.to_str().to_string()),
            _ => None,
        })
        .collect()
}

/// Best-effort constant evaluation of module-level guard conditions over
/// `sys.platform`, `os.name`, and `sys.argv` (cito never passes plugin
/// flags, and neither does a default pytest invocation).
fn eval_condition(expr: &Expr) -> Option<bool> {
    match expr {
        Expr::BoolOp(op) => {
            let values: Option<Vec<bool>> = op.values.iter().map(eval_condition).collect();
            let values = values?;
            Some(match op.op {
                ast::BoolOp::And => values.iter().all(|v| *v),
                ast::BoolOp::Or => values.iter().any(|v| *v),
            })
        }
        Expr::UnaryOp(u) if matches!(u.op, ast::UnaryOp::Not) => {
            eval_condition(&u.operand).map(|v| !v)
        }
        Expr::Compare(cmp) if cmp.ops.len() == 1 && cmp.comparators.len() == 1 => {
            let left = &cmp.left;
            let right = &cmp.comparators[0];
            match cmp.ops[0] {
                ast::CmpOp::Eq => Some(const_str(left)? == const_str(right)?),
                ast::CmpOp::NotEq => Some(const_str(left)? != const_str(right)?),
                ast::CmpOp::In | ast::CmpOp::NotIn => {
                    // `"--flag" in sys.argv`: never true for default runs.
                    if dotted(right).as_deref() == Some("sys.argv") {
                        let contains = false;
                        Some(match cmp.ops[0] {
                            ast::CmpOp::In => contains,
                            _ => !contains,
                        })
                    } else {
                        None
                    }
                }
                _ => None,
            }
        }
        // sys.platform.startswith("...")
        Expr::Call(call) => {
            let Expr::Attribute(attr) = &*call.func else {
                return None;
            };
            if attr.attr.as_str() != "startswith" {
                return None;
            }
            let base = const_str(&attr.value)?;
            match call.arguments.args.first() {
                Some(Expr::StringLiteral(s)) => Some(base.starts_with(s.value.to_str())),
                _ => None,
            }
        }
        _ => None,
    }
}

/// String value of an expression when it is a literal or a known runtime
/// constant (`sys.platform`, `os.name`) for the machine cito runs on.
fn const_str(expr: &Expr) -> Option<String> {
    match expr {
        Expr::StringLiteral(s) => Some(s.value.to_str().to_string()),
        _ => match dotted(expr)?.as_str() {
            "sys.platform" => Some(
                match std::env::consts::OS {
                    "macos" => "darwin",
                    "windows" => "win32",
                    other => other,
                }
                .to_string(),
            ),
            "os.name" => Some(
                match std::env::consts::OS {
                    "windows" => "nt",
                    _ => "posix",
                }
                .to_string(),
            ),
            _ => None,
        },
    }
}

fn dotted(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Name(name) => Some(name.id.to_string()),
        Expr::Attribute(attr) => Some(format!("{}.{}", dotted(&attr.value)?, attr.attr)),
        _ => None,
    }
}

/// `import_module('name')` / `importorskip('name')` call → the name.
fn import_module_binding(expr: &Expr) -> Option<String> {
    let Expr::Call(call) = expr else {
        return None;
    };
    let callee = match &*call.func {
        Expr::Attribute(attr) => attr.attr.as_str(),
        Expr::Name(name) => name.id.as_str(),
        _ => return None,
    };
    if !matches!(callee, "import_module" | "importorskip") {
        return None;
    }
    match call.arguments.args.first() {
        Some(Expr::StringLiteral(s)) => Some(s.value.to_str().to_string()),
        _ => None,
    }
}

/// Classify an if-guard we could not constant-fold into a deferred form.
fn classify_guard(expr: &Expr, bindings: &HashMap<String, String>) -> Option<DeferredGuard> {
    match expr {
        Expr::UnaryOp(u) if matches!(u.op, ast::UnaryOp::Not) => {
            classify_guard(&u.operand, bindings).map(|g| match g {
                DeferredGuard::Call { name, negated } => DeferredGuard::Call {
                    name,
                    negated: !negated,
                },
                DeferredGuard::Binding { module, negated } => DeferredGuard::Binding {
                    module,
                    negated: !negated,
                },
            })
        }
        Expr::Call(call) if call.arguments.args.is_empty() => match &*call.func {
            Expr::Name(name) => Some(DeferredGuard::Call {
                name: name.id.to_string(),
                negated: false,
            }),
            _ => None,
        },
        Expr::Name(name) => bindings
            .get(name.id.as_str())
            .map(|m| DeferredGuard::Binding {
                module: m.clone(),
                negated: false,
            }),
        // `X is None` / `X is not None`
        Expr::Compare(cmp) if cmp.ops.len() == 1 && cmp.comparators.len() == 1 => {
            let Expr::Name(name) = &*cmp.left else {
                return None;
            };
            if !matches!(cmp.comparators[0], Expr::NoneLiteral(_)) {
                return None;
            }
            let module = bindings.get(name.id.as_str())?.clone();
            match cmp.ops[0] {
                ast::CmpOp::Is => Some(DeferredGuard::Binding {
                    module,
                    negated: true,
                }),
                ast::CmpOp::IsNot => Some(DeferredGuard::Binding {
                    module,
                    negated: false,
                }),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Names of top-level test-relevant definitions in a statement list
/// (shallow: exactly the names the branch would bind).
fn defined_names(stmts: &[Stmt]) -> Vec<String> {
    stmts
        .iter()
        .filter_map(|stmt| match stmt {
            Stmt::FunctionDef(f) => Some(f.name.to_string()),
            Stmt::ClassDef(c) => Some(c.name.to_string()),
            _ => None,
        })
        .collect()
}

/// `pytest.skip(...)` at module level (any arguments).
fn is_module_skip_call(expr: &Expr) -> bool {
    let Expr::Call(call) = expr else {
        return false;
    };
    match &*call.func {
        Expr::Attribute(attr) => attr.attr.as_str() == "skip",
        Expr::Name(name) => name.id.as_str() == "skip",
        _ => false,
    }
}

/// `pytest.importorskip("name")` (or bare `importorskip("name")`).
fn importorskip_name(expr: &Expr) -> Option<String> {
    let Expr::Call(call) = expr else {
        return None;
    };
    let is_importorskip = match &*call.func {
        Expr::Attribute(attr) => attr.attr.as_str() == "importorskip",
        Expr::Name(name) => name.id.as_str() == "importorskip",
        _ => false,
    };
    if !is_importorskip {
        return None;
    }
    match call.arguments.args.first() {
        Some(Expr::StringLiteral(s)) => Some(s.value.to_str().to_string()),
        _ => None,
    }
}

/// Textual dotted form of a base-class expression; subscripts (generics) are
/// unwrapped, anything else is ignored.
fn base_text(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Name(name) => Some(name.id.to_string()),
        Expr::Attribute(attr) => Some(format!("{}.{}", base_text(&attr.value)?, attr.attr)),
        Expr::Subscript(sub) => base_text(&sub.value),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Cross-module resolution
// ---------------------------------------------------------------------------

enum BaseTarget {
    Unittest,
    Local(Rc<Module>, String),
    Unknown,
}

struct Resolver<'a> {
    config: &'a Config,
    cache: HashMap<PathBuf, Option<Rc<Module>>>,
    /// Python used to probe `importorskip` availability; None = collect
    /// statically (keep environment-conditional modules).
    probe_python: Option<String>,
    probe_cache: HashMap<String, bool>,
    /// The probe python's sys.path entries — lets absolute imports resolve
    /// into site-packages (e.g. external TestCase base classes). Lazy.
    sys_paths: Option<Vec<PathBuf>>,
}

impl<'a> Resolver<'a> {
    fn new(config: &'a Config, probe_python: Option<String>) -> Self {
        Resolver {
            config,
            cache: HashMap::new(),
            probe_python,
            probe_cache: HashMap::new(),
            sys_paths: None,
        }
    }

    fn sys_paths(&mut self) -> Vec<PathBuf> {
        if let Some(paths) = &self.sys_paths {
            return paths.clone();
        }
        let mut paths = Vec::new();
        if let Some(python) = &self.probe_python {
            if let Ok(out) = std::process::Command::new(python)
                .arg("-c")
                .arg("import json, sys; print(json.dumps([p for p in sys.path if p]))")
                .output()
            {
                if let Ok(list) = serde_json::from_slice::<Vec<String>>(&out.stdout) {
                    paths = list.into_iter().map(PathBuf::from).collect();
                }
            }
        }
        self.sys_paths = Some(paths.clone());
        paths
    }

    /// Is `name` importable in the probe interpreter? Only called when a
    /// probe python was supplied; results are cached per name.
    fn probe_ok(&mut self, name: &str) -> bool {
        let Some(python) = self.probe_python.clone() else {
            return true;
        };
        if let Some(&ok) = self.probe_cache.get(name) {
            return ok;
        }
        let ok = std::process::Command::new(&python)
            .arg("-c")
            .arg(format!(
                "import importlib, sys\ntry:\n    importlib.import_module({name:?})\nexcept BaseException:\n    sys.exit(1)"
            ))
            .output()
            .map(|out| out.status.success())
            .unwrap_or(true);
        self.probe_cache.insert(name.to_string(), ok);
        ok
    }

    /// Resolve many importorskip names with a single interpreter launch.
    fn probe_batch(&mut self, names: &std::collections::BTreeSet<String>) {
        let Some(python) = self.probe_python.clone() else {
            return;
        };
        let pending: Vec<&String> = names
            .iter()
            .filter(|n| !self.probe_cache.contains_key(n.as_str()))
            .collect();
        if pending.is_empty() {
            return;
        }
        // Real imports, not find_spec: pytest.importorskip imports, and a
        // module can exist yet fail to import (PIL.FpxImagePlugin without
        // olefile installed).
        const PROBE: &str = r#"
import importlib, json, sys
result = {}
for name in json.loads(sys.argv[1]):
    try:
        importlib.import_module(name)
        result[name] = True
    except BaseException:
        result[name] = False
print(json.dumps(result))
"#;
        let payload = serde_json::to_string(&pending).expect("names serialize");
        let output = std::process::Command::new(&python)
            .arg("-c")
            .arg(PROBE)
            .arg(&payload)
            .output();
        if let Ok(out) = output {
            if let Ok(map) = serde_json::from_slice::<HashMap<String, bool>>(&out.stdout) {
                self.probe_cache.extend(map);
            }
        }
    }

    fn preload(&mut self, path: PathBuf, module: Option<Module>) {
        self.cache.insert(path, module.map(Rc::new));
    }

    fn module(&mut self, path: &Path) -> Option<Rc<Module>> {
        if let Some(cached) = self.cache.get(path) {
            return cached.clone();
        }
        let parsed = path.is_file().then(|| parse_file(path)).flatten();
        let entry = parsed.map(Rc::new);
        self.cache.insert(path.to_path_buf(), entry.clone());
        entry
    }

    /// The conftest.py modules governing `dir`, nearest first, up to (and
    /// including) the rootdir — pytest's fixture lookup chain minus plugins.
    fn conftest_chain(&mut self, dir: &Path) -> Vec<Rc<Module>> {
        let mut chain = Vec::new();
        let mut current = Some(dir);
        while let Some(cur) = current {
            if let Some(module) = self.module(&cur.join("conftest.py")) {
                chain.push(module);
            }
            if cur == self.config.rootdir {
                break;
            }
            current = cur.parent();
        }
        chain
    }

    /// `importer_dir` supplies Python's sys.path semantics: absolute imports
    /// also resolve against the directory above the importer's topmost
    /// package (site-packages, or a repo's package parent).
    fn resolve_ref(&mut self, mref: &ModuleRef, importer_dir: &Path) -> Option<Rc<Module>> {
        let candidates: Vec<PathBuf> = match mref {
            ModuleRef::Relative(base) => {
                vec![base.with_extension("py"), base.join("__init__.py")]
            }
            ModuleRef::Absolute(dotted) => {
                let mut rel = PathBuf::new();
                for seg in dotted.split('.') {
                    rel = rel.join(seg);
                }
                let mut roots = vec![self.config.rootdir.clone(), self.config.rootdir.join("src")];
                if let Some(pkg_root) = package_root_above(importer_dir) {
                    roots.push(pkg_root);
                }
                roots.extend(self.sys_paths());
                roots
                    .iter()
                    .flat_map(|root| {
                        [
                            root.join(&rel).with_extension("py"),
                            root.join(&rel).join("__init__.py"),
                        ]
                    })
                    .collect()
            }
        };
        candidates.iter().find_map(|c| self.module(c))
    }

    /// Resolve a submodule reference: `mref` + one more dotted segment.
    fn resolve_child(&mut self, mref: &ModuleRef, child: &str) -> ModuleRef {
        match mref {
            ModuleRef::Absolute(dotted) => ModuleRef::Absolute(format!("{dotted}.{child}")),
            ModuleRef::Relative(base) => ModuleRef::Relative(base.join(child)),
        }
    }

    /// Chase `name` through re-exports (`from .impl import X` in an
    /// __init__.py, star re-exports) until the defining module is found.
    fn resolve_symbol(
        &mut self,
        mut module: Rc<Module>,
        mut name: String,
    ) -> Option<(Rc<Module>, String)> {
        for _ in 0..8 {
            if module.classes.contains_key(&name) {
                return Some((module, name));
            }
            match module.imports.get(&name).cloned() {
                Some(Import::From(mref, orig)) => {
                    if is_unittest_ref(&mref, &orig) {
                        return None;
                    }
                    let next = self.resolve_ref(&mref, &module.dir.clone())?;
                    module = next;
                    name = orig;
                }
                Some(Import::Module(_)) => return None,
                None => {
                    for star in module.star_imports.clone() {
                        if let Some(target) = self.resolve_ref(&star, &module.dir) {
                            if target.classes.contains_key(&name) {
                                return Some((target, name));
                            }
                        }
                    }
                    return None;
                }
            }
        }
        None
    }

    /// Like resolve_symbol, but a name defined as a top-level function in
    /// the target module also terminates the chase.
    fn resolve_symbol_or_function(
        &mut self,
        mut module: Rc<Module>,
        mut name: String,
    ) -> Option<(Rc<Module>, String)> {
        for _ in 0..8 {
            if module.classes.contains_key(&name) || module.functions.contains_key(&name) {
                return Some((module, name));
            }
            match module.imports.get(&name).cloned() {
                Some(Import::From(mref, orig)) => {
                    if is_unittest_ref(&mref, &orig) {
                        return None;
                    }
                    let next = self.resolve_ref(&mref, &module.dir.clone())?;
                    module = next;
                    name = orig;
                }
                _ => return None,
            }
        }
        None
    }

    /// Resolve a bare decorator name to a mark name, chasing imports
    /// (`from sympy.testing.pytest import slow` -> `slow = pytest.mark.slow`).
    fn resolve_mark_alias(&mut self, module: &Rc<Module>, name: &str) -> Option<String> {
        let mut module = module.clone();
        let mut name = name.to_string();
        for _ in 0..8 {
            if let Some(mark) = module.mark_aliases.get(&name) {
                return Some(mark.clone());
            }
            match module.imports.get(&name).cloned() {
                Some(Import::From(mref, orig)) => {
                    let next = self.resolve_ref(&mref, &module.dir.clone())?;
                    module = next;
                    name = orig;
                }
                _ => return None,
            }
        }
        None
    }

    fn resolve_base(&mut self, module: &Rc<Module>, text: &str) -> BaseTarget {
        let segments: Vec<&str> = text.split('.').collect();
        if segments.len() == 1 {
            let name = segments[0];
            match module.imports.get(name).cloned() {
                Some(Import::From(mref, orig)) => {
                    if is_unittest_ref(&mref, &orig) {
                        return BaseTarget::Unittest;
                    }
                    match self
                        .resolve_ref(&mref, &module.dir)
                        .and_then(|target| self.resolve_symbol(target, orig.clone()))
                    {
                        Some((target, name)) => BaseTarget::Local(target, name),
                        None => BaseTarget::Unknown,
                    }
                }
                Some(Import::Module(_)) => BaseTarget::Unknown,
                None => {
                    if module.classes.contains_key(name) {
                        return BaseTarget::Local(module.clone(), name.to_string());
                    }
                    // Fall back to star imports.
                    for star in module.star_imports.clone() {
                        if let Some(target) = self.resolve_ref(&star, &module.dir) {
                            if target.classes.contains_key(name) {
                                return BaseTarget::Local(target, name.to_string());
                            }
                        }
                    }
                    BaseTarget::Unknown
                }
            }
        } else {
            let first = segments[0];
            let last = *segments.last().unwrap();
            let middle = &segments[1..segments.len() - 1];
            let mref = match module.imports.get(first).cloned() {
                Some(Import::Module(dotted)) => {
                    let mut full = dotted;
                    for seg in middle {
                        full = format!("{full}.{seg}");
                    }
                    ModuleRef::Absolute(full)
                }
                Some(Import::From(mref, orig)) => {
                    let mut full = self.resolve_child(&mref, &orig);
                    for seg in middle {
                        full = self.resolve_child(&full, seg);
                    }
                    full
                }
                None => return BaseTarget::Unknown,
            };
            if is_unittest_ref(&mref, last) {
                return BaseTarget::Unittest;
            }
            match self
                .resolve_ref(&mref, &module.dir)
                .and_then(|target| self.resolve_symbol(target, last.to_string()))
            {
                Some((target, name)) => BaseTarget::Local(target, name),
                None => BaseTarget::Unknown,
            }
        }
    }

    /// Effective test-method list for a class: own methods first, then
    /// inherited ones (base order, depth-first), overrides deduped. Methods
    /// requesting a parametrized fixture from their *defining* module are
    /// downgraded to Fallback here; the leaf module's fixtures are re-checked
    /// at emission. Returns (methods, reaches_unittest, any_base_class_params).
    fn resolve_class(
        &mut self,
        module: &Rc<Module>,
        class: &Class,
        key: (PathBuf, String),
        visited: &mut HashSet<(PathBuf, String)>,
    ) -> (
        Vec<TestDef>,
        bool,
        bool,
        HashMap<String, Fixture>,
        Vec<String>,
    ) {
        visited.insert(key);
        let mut methods: Vec<TestDef> = Vec::new();
        let mut chain_fixtures: HashMap<String, Fixture> = class.fixtures.clone();
        let mut chain_marks: Vec<String> = class.marks.clone();
        let mut seen: HashSet<String> = HashSet::new();
        for item in &class.items {
            if let ClassItem::Method(def) = item {
                if seen.insert(def.name.clone()) {
                    let mut def = def.clone();
                    if def.expansion != Expansion::None
                        && (module.has_generate_tests
                            || requests_parametrized_fixture(
                                std::slice::from_ref(module),
                                &[&class.fixtures],
                                &def,
                            ))
                    {
                        def.expansion = Expansion::Fallback;
                    }
                    methods.push(def);
                }
            }
        }
        let mut unittest = false;
        let mut base_params = false;
        for base in &class.bases {
            match self.resolve_base(module, base) {
                BaseTarget::Unittest => unittest = true,
                BaseTarget::Local(target_mod, target_name) => {
                    // Chasing landed inside the stdlib unittest package.
                    if is_unittest_module_class(&target_mod, &target_name) {
                        unittest = true;
                        continue;
                    }
                    let key = (target_mod.path.clone(), target_name.clone());
                    if visited.contains(&key) {
                        continue;
                    }
                    let Some(target_class) = target_mod.classes.get(&target_name) else {
                        continue;
                    };
                    base_params |= target_class.expansion != Expansion::None
                        || has_autouse_params(&target_class.fixtures);
                    let (inherited, base_ut, base_bp, base_fixtures, base_marks) =
                        self.resolve_class(&target_mod, target_class, key, visited);
                    unittest |= base_ut;
                    base_params |= base_bp;
                    for (name, fixture) in base_fixtures {
                        chain_fixtures.entry(name).or_insert(fixture);
                    }
                    chain_marks.extend(base_marks);
                    for def in inherited {
                        if seen.insert(def.name.clone()) {
                            methods.push(def);
                        }
                    }
                }
                BaseTarget::Unknown => {}
            }
        }
        (methods, unittest, base_params, chain_fixtures, chain_marks)
    }
}

/// The directory above the topmost package containing `dir` — a sys.path
/// entry from Python's perspective.
fn package_root_above(dir: &Path) -> Option<PathBuf> {
    let mut current = dir;
    let mut topmost = None;
    while current.join("__init__.py").is_file() {
        topmost = Some(current);
        current = current.parent()?;
    }
    topmost.and_then(|t| t.parent()).map(Path::to_path_buf)
}

const UNITTEST_CLASSES: &[&str] = &["TestCase", "IsolatedAsyncioTestCase", "FunctionTestCase"];

fn is_unittest_ref(mref: &ModuleRef, name: &str) -> bool {
    matches!(
        mref,
        ModuleRef::Absolute(dotted)
            if matches!(
                dotted.as_str(),
                "unittest" | "unittest.case" | "unittest.async_case"
            ) && UNITTEST_CLASSES.contains(&name)
    )
}

/// A base that resolved into the stdlib `unittest` package itself.
fn is_unittest_module_class(module: &Module, name: &str) -> bool {
    UNITTEST_CLASSES.contains(&name)
        && module
            .path
            .components()
            .any(|c| c.as_os_str() == "unittest")
}

// ---------------------------------------------------------------------------
// Emission
// ---------------------------------------------------------------------------

/// Collect tests from all roots, honoring `config`. Test files are parsed in
/// parallel; base-class modules are parsed lazily during resolution.
pub fn collect(
    roots: &[PathBuf],
    config: &Config,
    probe_python: Option<&str>,
    marker: Option<&crate::keyword::KExpr>,
) -> Vec<FileTests> {
    let files = discover(roots, config);
    let parsed: Vec<Option<Module>> = files.par_iter().map(|p| parse_file(p)).collect();

    let mut resolver = Resolver::new(config, probe_python.map(str::to_string));
    for (path, module) in files.iter().zip(parsed) {
        resolver.preload(path.clone(), module);
    }

    // Batch all importorskip probes into one interpreter launch up front.
    if resolver.probe_python.is_some() {
        let mut names = std::collections::BTreeSet::new();
        for path in &files {
            if let Some(module) = resolver.module(path) {
                names.extend(module.skip_requires.iter().cloned());
                for conftest in resolver.conftest_chain(&module.dir) {
                    names.extend(conftest.skip_requires.iter().cloned());
                }
            }
        }
        resolver.probe_batch(&names);
    }

    files
        .iter()
        .map(|abs| {
            let tests = resolver
                .module(abs)
                .map(|module| emit_module(&mut resolver, &module, marker))
                .unwrap_or_default();
            FileTests {
                path: display_path(abs, &config.rootdir),
                abs_path: abs.clone(),
                tests,
            }
        })
        .collect()
}

/// Collect from a single source string (used by unit tests); same-module
/// inheritance works, cross-module bases resolve against `config.rootdir`.
pub fn collect_source(source: &str, config: &Config) -> Vec<String> {
    let path = config.rootdir.join("__cito_inline__.py");
    match parse_source(&path, source) {
        Ok(module) => {
            let mut resolver = Resolver::new(config, None);
            let module = Rc::new(module);
            emit_module(&mut resolver, &module, None)
        }
        Err(_) => Vec::new(),
    }
}

fn emit_module(
    resolver: &mut Resolver,
    module: &Rc<Module>,
    marker: Option<&crate::keyword::KExpr>,
) -> Vec<String> {
    // Fixture visibility for tests in this module: the module itself plus
    // its conftest chain, plus any pytest_plugins modules those conftests
    // declare (their fixtures and pytest_generate_tests hooks apply too —
    // e.g. aiohttp.pytest_plugin parametrizes the loop fixture).
    let mut contexts = vec![module.clone()];
    contexts.extend(resolver.conftest_chain(&module.dir));
    let declared: Vec<String> = contexts
        .iter()
        .flat_map(|m| m.plugin_modules.iter().cloned())
        .collect();
    for plugin in declared {
        if let Some(target) =
            resolver.resolve_ref(&ModuleRef::Absolute(plugin), &module.dir.clone())
        {
            contexts.push(target);
        }
    }

    // conftest `collect_ignore` / `collect_ignore_glob` drop matching files
    // (literal entries only; computed appends are invisible to us).
    for conftest in contexts.iter().skip(1) {
        for entry in &conftest.collect_ignore {
            if conftest.dir.join(entry) == module.path {
                return Vec::new();
            }
        }
        if let Ok(rel) = module.path.strip_prefix(&conftest.dir) {
            for pattern in &conftest.collect_ignore_glob {
                if globset::Glob::new(pattern)
                    .map(|g| g.compile_matcher().is_match(rel))
                    .unwrap_or(false)
                {
                    return Vec::new();
                }
            }
        }
    }

    // With a probe python, module-level `importorskip` in the file or its
    // conftest chain drops the whole module when the dependency is absent,
    // matching pytest's behavior in that environment.
    if resolver.probe_python.is_some() {
        if contexts.iter().any(|m| m.has_module_skip) {
            return Vec::new();
        }
        for helper in &module.helper_calls {
            if module
                .functions
                .get(helper)
                .map(|def| def.skips_module)
                .unwrap_or(false)
            {
                return Vec::new();
            }
            if let Some((target, name)) =
                resolver.resolve_symbol_or_function(module.clone(), helper.clone())
            {
                if target
                    .functions
                    .get(&name)
                    .map(|def| def.skips_module)
                    .unwrap_or(false)
                {
                    return Vec::new();
                }
            }
        }
        let requires: Vec<String> = contexts
            .iter()
            .flat_map(|m| m.skip_requires.iter().cloned())
            .collect();
        if requires.iter().any(|name| !resolver.probe_ok(name)) {
            return Vec::new();
        }
    }

    // Resolve deferred branch guards (imported predicates, import-module
    // availability bindings) into a set of dead definition names.
    let mut dead: HashSet<String> = HashSet::new();
    let mut alive: HashSet<String> = HashSet::new();
    for (guard, names) in &module.cond_blocks {
        let truth = match guard {
            DeferredGuard::Call { name, negated } => resolver
                .resolve_symbol_or_function(module.clone(), name.clone())
                .and_then(|(target, resolved)| {
                    target
                        .functions
                        .get(&resolved)
                        .and_then(|d| d.returns_const)
                })
                .map(|v| v != *negated),
            DeferredGuard::Binding {
                module: dep,
                negated,
            } => {
                if resolver.probe_python.is_some() {
                    Some(resolver.probe_ok(dep) != *negated)
                } else {
                    None
                }
            }
        };
        match truth {
            Some(false) => dead.extend(names.iter().cloned()),
            Some(true) => alive.extend(names.iter().cloned()),
            None => alive.extend(names.iter().cloned()),
        }
    }
    for name in alive.iter().chain(module.certain_names.iter()) {
        dead.remove(name);
    }

    // A pytest_generate_tests hook or a parametrized autouse fixture
    // anywhere in scope can add parameters we cannot see; exact expansions
    // are no longer trustworthy.
    let poisoned = contexts
        .iter()
        .any(|m| m.has_generate_tests || has_autouse_params(&m.fixtures));

    let mut tests = Vec::new();
    for item in &module.order {
        let item_name = match item {
            TopItem::Func(def) => def.name.as_str(),
            TopItem::Class(name) => name.as_str(),
        };
        if dead.contains(item_name) {
            continue;
        }
        match item {
            TopItem::Func(def) => {
                if resolver.config.function_matches(&def.name) {
                    let mut names: HashSet<String> = module
                        .pytestmark
                        .iter()
                        .chain(def.marks.iter())
                        .cloned()
                        .collect();
                    for candidate in &def.maybe_marks {
                        if let Some(mark) = resolver.resolve_mark_alias(module, candidate) {
                            names.insert(mark);
                        }
                    }
                    if let Some(expr) = marker {
                        if !expr.matches_names(&names) {
                            continue;
                        }
                    }
                    let mut expansion = if def.expansion != Expansion::None
                        && requests_parametrized_fixture(&contexts, &[], def)
                    {
                        Expansion::Fallback
                    } else {
                        def.expansion.clone()
                    };
                    // The anyio plugin parametrizes marked tests with the
                    // backend fixture, adding ID pieces we cannot see.
                    if (poisoned || names.contains("anyio"))
                        && matches!(expansion, Expansion::Params(_))
                    {
                        expansion = Expansion::Fallback;
                    }
                    tests.extend(expansion.apply(&def.name));
                }
            }
            TopItem::Class(name) => {
                let Some(class) = module.classes.get(name) else {
                    continue;
                };
                emit_class(
                    resolver,
                    module,
                    class,
                    name,
                    &contexts,
                    &module.pytestmark,
                    poisoned,
                    marker,
                    &mut Vec::new(),
                    &mut tests,
                );
            }
        }
    }

    // `X = SomeStateMachine.TestCase` bindings (hypothesis stateful): a
    // unittest TestCase whose single test method is runTest.
    for name in &module.synthetic_testcases {
        if dead.contains(name) || module.classes.contains_key(name) {
            continue;
        }
        if let Some(expr) = marker {
            let names: HashSet<String> = module.pytestmark.iter().cloned().collect();
            if !expr.matches_names(&names) {
                continue;
            }
        }
        tests.push(format!("{name}::runTest"));
    }

    // pytest collects over the module NAMESPACE: test classes/functions
    // *imported* into a test module are collected here too (the classic
    // urllib3 contrib pattern: `from ..test_https import TestHTTPS`).
    let mut imported: Vec<(String, Rc<Module>, String)> = Vec::new(); // (local, module, original)
    for (local, import) in &module.imports {
        let Import::From(mref, orig) = import else {
            continue;
        };
        let looks_like_class = resolver.config.class_matches(local);
        let looks_like_func = resolver.config.function_matches(local);
        if !looks_like_class && !looks_like_func {
            continue;
        }
        if module.classes.contains_key(local) || module.functions.contains_key(local) {
            continue; // a local definition shadows the import
        }
        if is_unittest_ref(mref, orig) {
            continue;
        }
        if let Some(target) = resolver.resolve_ref(mref, &module.dir) {
            imported.push((local.clone(), target, orig.clone()));
        }
    }
    imported.sort_by(|a, b| a.0.cmp(&b.0));
    for (local, target, orig) in imported {
        let Some((target, orig)) = resolver.resolve_symbol_or_function(target, orig) else {
            continue;
        };
        if resolver.config.class_matches(&local) {
            if let Some(class) = target.classes.get(&orig) {
                emit_class(
                    resolver,
                    &target.clone(),
                    class,
                    &local,
                    &contexts,
                    &module.pytestmark,
                    poisoned,
                    marker,
                    &mut Vec::new(),
                    &mut tests,
                );
                continue;
            }
        }
        if resolver.config.function_matches(&local) {
            if let Some(def) = target.functions.get(&orig) {
                let mut names: HashSet<String> = module
                    .pytestmark
                    .iter()
                    .chain(def.marks.iter())
                    .cloned()
                    .collect();
                for candidate in &def.maybe_marks {
                    if let Some(mark) = resolver.resolve_mark_alias(&target, candidate) {
                        names.insert(mark);
                    }
                }
                if let Some(expr) = marker {
                    if !expr.matches_names(&names) {
                        continue;
                    }
                }
                let mut expansion = if def.expansion != Expansion::None
                    && requests_parametrized_fixture(&contexts, &[], def)
                {
                    Expansion::Fallback
                } else {
                    def.expansion.clone()
                };
                if (poisoned || names.contains("anyio"))
                    && matches!(expansion, Expansion::Params(_))
                {
                    expansion = Expansion::Fallback;
                }
                for id in expansion.apply(&local) {
                    tests.push(id);
                }
            }
        }
    }
    tests
}

#[allow(clippy::too_many_arguments)]
fn emit_class(
    resolver: &mut Resolver,
    module: &Rc<Module>,
    class: &Class,
    name: &str,
    contexts: &[Rc<Module>],
    pytestmark: &[String],
    poisoned: bool,
    marker: Option<&crate::keyword::KExpr>,
    stack: &mut Vec<String>,
    out: &mut Vec<String>,
) {
    let mut visited = HashSet::new();
    let key = (module.path.clone(), name.to_string());
    let (methods, unittest, base_params, chain_fixtures, chain_marks) =
        resolver.resolve_class(module, class, key, &mut visited);
    // The class chain's parametrized autouse fixtures poison exact
    // expansion for all of its methods.
    let poisoned = poisoned || base_params || has_autouse_params(&chain_fixtures);

    let collectable = unittest || (resolver.config.class_matches(name) && !class.has_ctor);
    if !collectable {
        return;
    }

    stack.push(name.to_string());
    let class_expansion = if base_params {
        Expansion::Fallback
    } else {
        class.expansion.clone()
    };
    for def in &methods {
        let matches = if unittest {
            def.name.starts_with("test")
        } else {
            resolver.config.function_matches(&def.name)
        };
        if !matches {
            continue;
        }
        let mut names: HashSet<String> = pytestmark
            .iter()
            .chain(chain_marks.iter())
            .chain(def.marks.iter())
            .cloned()
            .collect();
        for candidate in &def.maybe_marks {
            if let Some(mark) = resolver.resolve_mark_alias(module, candidate) {
                names.insert(mark);
            }
        }
        if let Some(expr) = marker {
            if !expr.matches_names(&names) {
                continue;
            }
        }
        // Any exact expansion — the method's own or one applied by the
        // class — is invalid if the test requests a parametrized fixture
        // (leaf-module visibility applies to inherited methods too), or if
        // the anyio plugin will parametrize it via its backend fixture.
        let mut combined = Expansion::combine(&class_expansion, &def.expansion);
        if matches!(combined, Expansion::Params(_)) {
            let mut request = def.clone();
            request.args.extend(class.usefixtures.iter().cloned());
            if poisoned
                || names.contains("anyio")
                || requests_parametrized_fixture(contexts, &[&chain_fixtures], &request)
            {
                combined = Expansion::Fallback;
            }
        }
        for id in combined.apply(&def.name) {
            out.push(format!("{}::{}", stack.join("::"), id));
        }
    }
    for item in &class.items {
        if let ClassItem::Nested(nested_name, nested_class) = item {
            emit_class(
                resolver,
                module,
                nested_class,
                nested_name,
                contexts,
                pytestmark,
                poisoned,
                marker,
                stack,
                out,
            );
        }
    }
    stack.pop();
}

/// rootdir-relative, forward-slash display path (pytest's node ID prefix).
fn display_path(abs: &Path, rootdir: &Path) -> String {
    let rel = abs.strip_prefix(rootdir).unwrap_or(abs);
    let s = rel.to_string_lossy();
    if std::path::MAIN_SEPARATOR == '/' {
        s.into_owned()
    } else {
        s.replace(std::path::MAIN_SEPARATOR, "/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Config {
        Config::discover(Path::new("/nonexistent-cito-root"))
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
        let tests = collect_source(source, &test_config());
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

    #[test]
    fn same_module_inheritance_and_unittest() {
        let source = r#"
import unittest

class Base:
    def test_from_base(self):
        pass

    def helper(self):
        pass

class TestChild(Base):
    def test_own(self):
        pass

class LegacySuite(unittest.TestCase):
    def test_unittest_style(self):
        pass

    def not_a_test(self):
        pass
"#;
        let tests = collect_source(source, &test_config());
        assert_eq!(
            tests,
            vec![
                "TestChild::test_own",
                "TestChild::test_from_base",
                "LegacySuite::test_unittest_style",
            ]
        );
    }

    #[test]
    fn parametrize_literals_expand() {
        let source = r#"
import pytest

@pytest.mark.parametrize("x", [1, 2])
def test_ints(x):
    pass

@pytest.mark.parametrize("f", [1.5])
def test_floats_fall_back(f):
    pass
"#;
        let tests = collect_source(source, &test_config());
        assert_eq!(
            tests,
            vec!["test_ints[1]", "test_ints[2]", "test_floats_fall_back"]
        );
    }
}
