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
    /// `-k` keyword names per test (marks, `pytestmark`), keyed by the
    /// unparametrized test ID (`Class::test`).
    #[serde(skip)]
    pub keywords: HashMap<String, Vec<String>>,
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
    /// `func.__test__ = False` was assigned: pytest skips the function.
    not_test: bool,
}

#[derive(Debug)]
enum ClassItem {
    Method(TestDef),
    Nested(String, Class),
    /// A non-callable class attribute (`test_x = None`, `test_data =
    /// dict(...)`): never a test, but it claims the name, shadowing an
    /// inherited method of the same name.
    Attr(String),
}

impl ClassItem {
    fn name(&self) -> &str {
        match self {
            ClassItem::Method(def) => def.name.as_str(),
            ClassItem::Nested(name, _) | ClassItem::Attr(name) => name.as_str(),
        }
    }
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
    /// Literal `__test__ = ...` in the class body: None = not assigned,
    /// Some(None) = assigned a non-literal, Some(Some(b)) = literal bool.
    dunder_test: Option<Option<bool>>,
}

/// A module-namespace binding pytest may collect, in `__dict__` order.
#[derive(Debug)]
enum TopItem {
    Func(TestDef),
    Class(String),
    /// `from M import name` — collected at the import's position.
    Import(String),
    /// `X = SomeStateMachine.TestCase` synthetic unittest class.
    Synthetic(String),
}

impl TopItem {
    fn name(&self) -> &str {
        match self {
            TopItem::Func(def) => def.name.as_str(),
            TopItem::Class(name) | TopItem::Import(name) | TopItem::Synthetic(name) => {
                name.as_str()
            }
        }
    }
}

/// The module namespace as an insertion-ordered dict: rebinding a name
/// keeps its original slot (Python dict semantics), `del` frees the slot so
/// a later rebinding appends at the end.
#[derive(Debug, Default)]
struct Namespace {
    slots: Vec<Option<TopItem>>,
    index: HashMap<String, usize>,
}

impl Namespace {
    fn bind(&mut self, item: TopItem) {
        match self.index.get(item.name()) {
            Some(&i) => {
                // A local def/class always wins over an import of the same
                // name (conditional-import fallbacks), whatever the order.
                let keep_local = matches!(item, TopItem::Import(_) | TopItem::Synthetic(_))
                    && matches!(self.slots[i], Some(TopItem::Func(_) | TopItem::Class(_)));
                if !keep_local {
                    self.slots[i] = Some(item);
                }
            }
            None => {
                self.index.insert(item.name().to_string(), self.slots.len());
                self.slots.push(Some(item));
            }
        }
    }

    fn remove(&mut self, name: &str) {
        if let Some(i) = self.index.remove(name) {
            self.slots[i] = None;
        }
    }

    fn into_order(self) -> Vec<TopItem> {
        self.slots.into_iter().flatten().collect()
    }
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
    /// Full dotted names of `import a.b` statements — the binding in
    /// `imports` keeps only what the namespace sees; impact analysis needs
    /// the module that actually executes.
    imported_modules: Vec<String>,
    classes: HashMap<String, Class>,
    functions: HashMap<String, TestDef>,
    fixtures: HashMap<String, Fixture>,
    order: Vec<TopItem>,
    /// Namespace being built during the scan; drained into `order`.
    namespace: Namespace,
    /// Module-level `__test__ = False`: pytest collects nothing here.
    not_test: bool,
    /// Functions marked `func.__test__ = False` at module level.
    not_test_funcs: HashSet<String>,
    /// Module names demanded via module-level `pytest.importorskip(...)`.
    skip_requires: Vec<String>,
    /// A module-level `pytest.skip(...)` call (possibly behind an `if`):
    /// under a default invocation the module opts out of collection.
    has_module_skip: bool,
    /// Bare helper calls at module level — possibly imported skip wrappers.
    helper_calls: Vec<String>,
    /// `NAME = import_module('mod')` / importorskip bindings.
    import_bindings: HashMap<String, String>,
    /// Top-level defs guarded by a condition we can only resolve at emit
    /// time (imported predicates, import-availability bindings).
    cond_blocks: Vec<(DeferredGuard, Vec<String>)>,
    /// Names defined on unconditional top-level paths (never deadened).
    certain_names: HashSet<String>,
    /// Names removed via module-level `del NAME` and not rebound after.
    deleted_names: HashSet<String>,
    /// Module-level `pytestmark = ...` mark names.
    pytestmark: Vec<String>,
    /// `slow = pytest.mark.slow` style aliases defined in this module.
    mark_aliases: HashMap<String, String>,
    /// `pytest_plugins = [...]` declarations (conftest only, per pytest).
    plugin_modules: Vec<String>,
    /// conftest.py `collect_ignore` / `collect_ignore_glob` (literal
    /// entries). None = the name is not defined here; pytest consults only
    /// the nearest conftest that defines it.
    collect_ignore: Option<Vec<String>>,
    collect_ignore_glob: Option<Vec<String>>,
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
        not_test: false,
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

fn parse_source(path: &Path, source: &str) -> Result<Module, ruff_python_parser::ParseError> {
    let syntax = ruff_python_parser::parse_module(source)?.into_syntax();
    let mut module = Module {
        path: path.to_path_buf(),
        dir: path.parent().unwrap_or(Path::new("")).to_path_buf(),
        imports: HashMap::new(),
        star_imports: Vec::new(),
        imported_modules: Vec::new(),
        classes: HashMap::new(),
        functions: HashMap::new(),
        fixtures: HashMap::new(),
        order: Vec::new(),
        namespace: Namespace::default(),
        not_test: false,
        not_test_funcs: HashSet::new(),
        skip_requires: Vec::new(),
        has_module_skip: false,
        helper_calls: Vec::new(),
        import_bindings: HashMap::new(),
        cond_blocks: Vec::new(),
        certain_names: HashSet::new(),
        deleted_names: HashSet::new(),
        pytestmark: Vec::new(),
        mark_aliases: HashMap::new(),
        plugin_modules: Vec::new(),
        collect_ignore: None,
        collect_ignore_glob: None,
        has_generate_tests: false,
    };
    let mut aliases = params::ParamAliases::new();
    scan(&syntax.body, &mut module, &mut aliases, true, true);
    module.order = std::mem::take(&mut module.namespace).into_order();
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
                module.deleted_names.remove(&def.name);
                module.namespace.bind(TopItem::Func(def));
            }
            Stmt::ClassDef(class) if top => {
                if certain {
                    module.certain_names.insert(class.name.to_string());
                }
                let name = class.name.to_string();
                module
                    .classes
                    .insert(name.clone(), build_class(class, aliases));
                module.deleted_names.remove(&name);
                module.namespace.bind(TopItem::Class(name));
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
                            module.deleted_names.remove(target.id.as_str());
                            module
                                .namespace
                                .bind(TopItem::Synthetic(target.id.to_string()));
                        }
                    }
                    // conftest collect_ignore lists (literal entries only).
                    if matches!(target.id.as_str(), "collect_ignore" | "collect_ignore_glob") {
                        let entries = string_list(&assign.value);
                        if target.id.as_str() == "collect_ignore" {
                            module.collect_ignore = Some(entries);
                        } else {
                            module.collect_ignore_glob = Some(entries);
                        }
                    }
                    // Module-level `__test__ = False` opts the module out.
                    if target.id.as_str() == "__test__" {
                        module.not_test = matches!(
                            &*assign.value,
                            Expr::BooleanLiteral(b) if !b.value
                        );
                    }
                }
                // `test_helper.__test__ = False` opts a function out.
                if let Some(name) = dunder_test_false_target(assign) {
                    module.not_test_funcs.insert(name);
                }
                // `mpl = pytest.importorskip("matplotlib")`.
                if let Some(name) = importorskip_name(&assign.value) {
                    module.skip_requires.push(name);
                }
            }
            // `del TestFoo` at module level removes the binding before
            // pytest ever collects it (scipy's linprog class-factory idiom).
            // Deletion is positional: a later rebinding is collected again.
            Stmt::Delete(delete) if top => {
                for target in &delete.targets {
                    if let Expr::Name(name) = target {
                        module.deleted_names.insert(name.id.to_string());
                        module.certain_names.remove(name.id.as_str());
                        module.namespace.remove(name.id.as_str());
                    }
                }
            }
            // `collect_ignore += [...]` in a conftest.
            Stmt::AugAssign(aug) if top && certain && matches!(aug.op, ast::Operator::Add) => {
                if let Expr::Name(target) = &*aug.target {
                    extend_ignore_list(module, target.id.as_str(), string_list(&aug.value));
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
                    // `collect_ignore.append("x")` / `.extend([...])` on an
                    // unconditional path.
                    if let Expr::Attribute(attr) = &*call.func {
                        if let (true, Expr::Name(list), Some(arg)) =
                            (certain, &*attr.value, call.arguments.args.first())
                        {
                            let entries = match attr.attr.as_str() {
                                "append" => string_value_of(arg).into_iter().collect(),
                                "extend" => string_list(arg),
                                _ => Vec::new(),
                            };
                            extend_ignore_list(module, list.id.as_str(), entries);
                        }
                    }
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
                    // The full dotted module executes regardless of the
                    // binding shape — impact analysis needs it.
                    module.imported_modules.push(dotted.clone());
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
                    module.deleted_names.remove(&local);
                    module.namespace.bind(TopItem::Import(local.clone()));
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
                // An elif/else arm runs only when every earlier arm is false
                // or unknown: once an arm is statically true, the rest are
                // dead. It is certain only when all earlier arms are
                // statically false and its own test is true (or absent).
                let mut earlier_true = cond == Some(true);
                let mut earlier_false = cond == Some(false);
                for clause in &if_stmt.elif_else_clauses {
                    let clause_cond = match &clause.test {
                        Some(test) => eval_condition(test),
                        None => Some(true),
                    };
                    let live = !earlier_true && clause_cond != Some(false);
                    let clause_certain = certain && earlier_false && clause_cond == Some(true);
                    scan(&clause.body, module, aliases, live, clause_certain);
                    earlier_true |= clause_cond == Some(true);
                    earlier_false &= clause_cond == Some(false);
                }
            }
            Stmt::Try(try_stmt) if top => {
                // `try: import x / except ImportError: pytest.skip(...)` is
                // importorskip in disguise: map it onto the probe.
                let handler_skips = try_stmt.handlers.iter().any(|h| {
                    let ast::ExceptHandler::ExceptHandler(h) = h;
                    body_calls_skip(&h.body)
                });
                let mut try_imports: Vec<String> = Vec::new();
                for stmt in &try_stmt.body {
                    match stmt {
                        Stmt::Import(import) => {
                            for alias in &import.names {
                                try_imports.push(alias.name.to_string());
                            }
                        }
                        Stmt::ImportFrom(import) if import.level == 0 => {
                            if let Some(m) = &import.module {
                                try_imports.push(m.to_string());
                            }
                        }
                        _ => {}
                    }
                }
                if handler_skips {
                    module.skip_requires.extend(try_imports.iter().cloned());
                }
                // `try: import x; has_x = True / except: has_x = False`
                // availability flags — same probe machinery as bindings.
                if let Some(first_import) = try_imports.first() {
                    // Either polarity: `has_x = True` inside the try body,
                    // or `has_x = False` inside an except handler (with the
                    // True assignment anywhere earlier).
                    let mut flags = bool_assignments(&try_stmt.body, true);
                    for handler in &try_stmt.handlers {
                        let ast::ExceptHandler::ExceptHandler(h) = handler;
                        flags.extend(bool_assignments(&h.body, false));
                    }
                    for flag in flags {
                        module.import_bindings.insert(flag, first_import.clone());
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
            // A `with` body always runs: its defs are module members.
            Stmt::With(with_stmt) => scan(&with_stmt.body, module, aliases, top, certain),
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
    // The class body is a namespace too: a rebound name keeps its first
    // slot and takes the last value.
    let mut items: Vec<ClassItem> = Vec::new();
    let mut bind = |item: ClassItem| match items.iter().position(|i| i.name() == item.name()) {
        Some(i) => items[i] = item,
        None => items.push(item),
    };
    let mut fixtures = HashMap::new();
    let mut has_ctor = false;
    let mut dunder_test = None;
    let mut not_tests: HashSet<String> = HashSet::new();
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
                bind(ClassItem::Method(def));
            }
            Stmt::ClassDef(nested) => {
                bind(ClassItem::Nested(
                    nested.name.to_string(),
                    build_class(nested, aliases),
                ));
            }
            Stmt::Assign(assign) => {
                if let Some(name) = dunder_test_false_target(assign) {
                    not_tests.insert(name);
                    continue;
                }
                let [Expr::Name(target)] = assign.targets.as_slice() else {
                    continue;
                };
                let name = target.id.to_string();
                if name == "__test__" {
                    dunder_test = Some(match &*assign.value {
                        Expr::BooleanLiteral(b) => Some(b.value),
                        _ => None,
                    });
                    continue;
                }
                match attr_kind(&assign.value) {
                    AttrKind::Callable => bind(ClassItem::Method(factory_def(name))),
                    AttrKind::NotCallable => bind(ClassItem::Attr(name)),
                    AttrKind::Unknown => {}
                }
            }
            Stmt::AnnAssign(assign) => {
                if let (Expr::Name(target), Some(value)) = (&*assign.target, &assign.value) {
                    match attr_kind(value) {
                        AttrKind::Callable => {
                            bind(ClassItem::Method(factory_def(target.id.to_string())))
                        }
                        AttrKind::NotCallable => bind(ClassItem::Attr(target.id.to_string())),
                        AttrKind::Unknown => {}
                    }
                }
            }
            _ => {}
        }
    }
    for item in &mut items {
        if let ClassItem::Method(def) = item {
            def.not_test = not_tests.contains(&def.name);
        }
    }
    Class {
        bases,
        items,
        fixtures,
        usefixtures: class_info.extra_fixture_requests,
        marks: class_info.marks,
        expansion: class_info.expansion,
        has_ctor,
        dunder_test,
    }
}

/// `test_kat = generate_encrypt_test(...)`: factory-made test methods bound
/// as class attributes (cryptography's idiom). Parametrization is
/// invisible, so they emit as bare names.
fn factory_def(name: String) -> TestDef {
    TestDef {
        name,
        expansion: Expansion::Fallback,
        args: Vec::new(),
        claimed: Vec::new(),
        marks: Vec::new(),
        maybe_marks: Vec::new(),
        skips_module: false,
        returns_const: None,
        not_test: false,
    }
}

enum AttrKind {
    Callable,
    NotCallable,
    Unknown,
}

/// Is a class attribute's value callable (pytest collects callable
/// test-named attributes)? Literals, displays and calls to builtin
/// constructors of plain data are not; lambdas and other calls (test
/// factories, decorators applied by hand) are assumed to be. Bare names and
/// attribute references are undecidable and bind nothing.
fn attr_kind(value: &Expr) -> AttrKind {
    match value {
        Expr::Lambda(_) => AttrKind::Callable,
        Expr::Call(call) => {
            let data_ctor = match &*call.func {
                Expr::Name(name) => matches!(
                    name.id.as_str(),
                    "dict"
                        | "list"
                        | "set"
                        | "frozenset"
                        | "tuple"
                        | "str"
                        | "bytes"
                        | "bytearray"
                        | "int"
                        | "float"
                        | "complex"
                        | "bool"
                        | "range"
                        | "object"
                        | "OrderedDict"
                        | "defaultdict"
                        | "Counter"
                        | "deque"
                ),
                _ => false,
            };
            if data_ctor {
                AttrKind::NotCallable
            } else {
                AttrKind::Callable
            }
        }
        Expr::NoneLiteral(_)
        | Expr::BooleanLiteral(_)
        | Expr::NumberLiteral(_)
        | Expr::StringLiteral(_)
        | Expr::BytesLiteral(_)
        | Expr::FString(_)
        | Expr::EllipsisLiteral(_)
        | Expr::List(_)
        | Expr::Tuple(_)
        | Expr::Dict(_)
        | Expr::Set(_)
        | Expr::ListComp(_)
        | Expr::SetComp(_)
        | Expr::DictComp(_)
        | Expr::Generator(_) => AttrKind::NotCallable,
        _ => AttrKind::Unknown,
    }
}

/// `NAME.__test__ = False` — the name whose collection is switched off.
fn dunder_test_false_target(assign: &ast::StmtAssign) -> Option<String> {
    let [Expr::Attribute(attr)] = assign.targets.as_slice() else {
        return None;
    };
    let Expr::Name(owner) = &*attr.value else {
        return None;
    };
    let is_false = matches!(&*assign.value, Expr::BooleanLiteral(b) if !b.value);
    (attr.attr.as_str() == "__test__" && is_false).then(|| owner.id.to_string())
}

/// `collect_ignore.append(...)` / `+= [...]` onto a conftest ignore list.
fn extend_ignore_list(module: &mut Module, list: &str, entries: Vec<String>) {
    let target = match list {
        "collect_ignore" => &mut module.collect_ignore,
        "collect_ignore_glob" => &mut module.collect_ignore_glob,
        _ => return,
    };
    target.get_or_insert_with(Vec::new).extend(entries);
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

/// Names assigned the boolean literal `value` in a statement list.
fn bool_assignments(stmts: &[Stmt], value: bool) -> Vec<String> {
    stmts
        .iter()
        .filter_map(|stmt| match stmt {
            Stmt::Assign(assign) => match (assign.targets.as_slice(), &*assign.value) {
                ([Expr::Name(target)], Expr::BooleanLiteral(b)) if b.value == value => {
                    Some(target.id.to_string())
                }
                _ => None,
            },
            _ => None,
        })
        .collect()
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
    mro_cache: HashMap<ClassKey, (Vec<MroClass>, bool)>,
    /// Keyword names of the tests emitted for the current module.
    keywords: HashMap<String, Vec<String>>,
}

impl<'a> Resolver<'a> {
    fn new(config: &'a Config, probe_python: Option<String>) -> Self {
        Resolver {
            config,
            cache: HashMap::new(),
            probe_python,
            probe_cache: HashMap::new(),
            sys_paths: None,
            mro_cache: HashMap::new(),
            keywords: HashMap::new(),
        }
    }

    /// Record a test's `-k` names: its marks, plus the `pytestmark`
    /// attribute that a directly-marked function carries in its __dict__.
    fn note_keywords(
        &mut self,
        module: &Rc<Module>,
        key: String,
        names: &HashSet<String>,
        def: &TestDef,
    ) {
        let mut words: Vec<String> = names.iter().cloned().collect();
        let direct = !def.marks.is_empty()
            || def
                .maybe_marks
                .iter()
                .any(|c| self.resolve_mark_alias(module, c).is_some());
        if direct {
            words.push("pytestmark".to_string());
        }
        self.keywords.insert(key, words);
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
                    // `orig` may be a submodule file, or a module RE-EXPORTED
                    // by the package (`from tests import unittest` where
                    // tests/__init__.py does `import unittest`). Prefer the
                    // re-export when the parent package resolves and binds
                    // the name as a module import.
                    let reexported = self.resolve_ref(&mref, &module.dir).and_then(|parent| {
                        match parent.imports.get(&orig).cloned() {
                            Some(Import::Module(dotted)) => Some(ModuleRef::Absolute(dotted)),
                            Some(Import::From(m2, o2)) => Some(self.resolve_child(&m2, &o2)),
                            None => None,
                        }
                    });
                    let mut full = match reexported {
                        Some(re) => re,
                        None => self.resolve_child(&mref, &orig),
                    };
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

    /// C3 linearization of the resolvable bases of a class (the class
    /// itself excluded), plus whether any base reaches unittest.TestCase.
    /// Unresolvable bases contribute nothing. `stack` guards cycles.
    fn mro_bases(
        &mut self,
        module: &Rc<Module>,
        bases: &[String],
        stack: &mut Vec<ClassKey>,
    ) -> (Vec<MroClass>, bool) {
        let mut unittest = false;
        let mut seqs: Vec<Vec<MroClass>> = Vec::new();
        let mut direct: Vec<MroClass> = Vec::new();
        for base in bases {
            match self.resolve_base(module, base) {
                BaseTarget::Unittest => unittest = true,
                BaseTarget::Local(target_mod, target_name) => {
                    // Chasing landed inside the stdlib unittest package.
                    if is_unittest_module_class(&target_mod, &target_name) {
                        unittest = true;
                        continue;
                    }
                    if let Some((lin, base_ut)) = self.mro_top(&target_mod, &target_name, stack) {
                        unittest |= base_ut;
                        direct.push(lin[0].clone());
                        seqs.push(lin);
                    }
                }
                BaseTarget::Unknown => {}
            }
        }
        seqs.push(direct);
        (c3_merge(seqs), unittest)
    }

    /// Linearization of a top-level class, itself first (memoized).
    fn mro_top(
        &mut self,
        module: &Rc<Module>,
        name: &str,
        stack: &mut Vec<ClassKey>,
    ) -> Option<(Vec<MroClass>, bool)> {
        let key = (module.path.clone(), name.to_string());
        if let Some(hit) = self.mro_cache.get(&key) {
            return Some(hit.clone());
        }
        if stack.contains(&key) {
            return None;
        }
        let class = module.classes.get(name)?;
        stack.push(key.clone());
        let (tail, unittest) = self.mro_bases(module, &class.bases, stack);
        stack.pop();
        let mut lin = vec![(module.clone(), name.to_string())];
        lin.extend(tail);
        self.mro_cache.insert(key, (lin.clone(), unittest));
        Some((lin, unittest))
    }

    /// A class's effective collectable members, in pytest's order: walk the
    /// MRO (C3) claiming each name for the first class defining it, then
    /// emit class by class in REVERSE MRO order (base members first), each
    /// class in definition order. Methods requesting a parametrized fixture
    /// from their *defining* module are downgraded to Fallback here; the
    /// leaf module's fixtures are re-checked at emission.
    fn resolve_class(&mut self, module: &Rc<Module>, class: &Class, key: ClassKey) -> Resolved {
        let mut stack = vec![key];
        let (bases, unittest) = self.mro_bases(module, &class.bases, &mut stack);
        let mut resolved = Resolved {
            items: Vec::new(),
            unittest,
            base_params: false,
            fixtures: HashMap::new(),
            marks: Vec::new(),
            has_ctor: false,
            dunder_test: None,
        };
        let entries = std::iter::once((module.clone(), None))
            .chain(bases.into_iter().map(|(m, n)| (m, Some(n))));
        let mut dunder_test: Option<Option<bool>> = None;
        let mut seen: HashSet<String> = HashSet::new();
        let mut groups: Vec<Vec<ResolvedItem>> = Vec::new();
        for (entry_mod, owner) in entries {
            let entry_class = match &owner {
                None => class,
                Some(name) => match entry_mod.classes.get(name) {
                    Some(c) => c,
                    None => continue,
                },
            };
            for (name, fixture) in &entry_class.fixtures {
                resolved
                    .fixtures
                    .entry(name.clone())
                    .or_insert_with(|| fixture.clone());
            }
            resolved.marks.extend(entry_class.marks.iter().cloned());
            resolved.has_ctor |= entry_class.has_ctor;
            if dunder_test.is_none() {
                dunder_test = entry_class.dunder_test;
            }
            if owner.is_some() {
                resolved.base_params |= entry_class.expansion != Expansion::None
                    || has_autouse_params(&entry_class.fixtures);
            }
            let mut group = Vec::new();
            for item in &entry_class.items {
                if !seen.insert(item.name().to_string()) {
                    continue;
                }
                match item {
                    ClassItem::Method(def) => {
                        let mut def = def.clone();
                        if def.expansion != Expansion::None
                            && (entry_mod.has_generate_tests
                                || requests_parametrized_fixture(
                                    std::slice::from_ref(&entry_mod),
                                    &[&entry_class.fixtures],
                                    &def,
                                ))
                        {
                            def.expansion = Expansion::Fallback;
                        }
                        group.push(ResolvedItem::Method(def));
                    }
                    ClassItem::Nested(name, _) => group.push(ResolvedItem::Nested {
                        module: entry_mod.clone(),
                        owner: owner.clone(),
                        name: name.clone(),
                    }),
                    ClassItem::Attr(_) => {}
                }
            }
            groups.push(group);
        }
        resolved.dunder_test = dunder_test.flatten();
        resolved.items = groups.into_iter().rev().flatten().collect();
        resolved
    }
}

type ClassKey = (PathBuf, String);
/// A resolvable top-level class: its module and name.
type MroClass = (Rc<Module>, String);

/// C3 merge of linearizations. An inconsistent hierarchy (which Python
/// would reject) falls back to first-occurrence order.
fn c3_merge(mut seqs: Vec<Vec<MroClass>>) -> Vec<MroClass> {
    let same = |a: &MroClass, b: &MroClass| a.1 == b.1 && a.0.path == b.0.path;
    let mut out: Vec<MroClass> = Vec::new();
    loop {
        seqs.retain(|s| !s.is_empty());
        if seqs.is_empty() {
            return out;
        }
        let candidate = seqs
            .iter()
            .map(|s| &s[0])
            .find(|head| !seqs.iter().any(|s| s[1..].iter().any(|c| same(c, head))))
            .cloned();
        let Some(candidate) = candidate else {
            for c in seqs.into_iter().flatten() {
                if !out.iter().any(|o| same(o, &c)) {
                    out.push(c);
                }
            }
            return out;
        };
        for s in &mut seqs {
            if same(&s[0], &candidate) {
                s.remove(0);
            }
        }
        out.push(candidate);
    }
}

/// A class resolved against its MRO.
struct Resolved {
    items: Vec<ResolvedItem>,
    unittest: bool,
    /// Some base class is parametrized (or has parametrized autouse
    /// fixtures).
    base_params: bool,
    fixtures: HashMap<String, Fixture>,
    marks: Vec<String>,
    /// Some class in the MRO defines `__init__` / `__new__`.
    has_ctor: bool,
    /// Effective literal `__test__` (first MRO class assigning it).
    dunder_test: Option<bool>,
}

enum ResolvedItem {
    Method(TestDef),
    /// Nested class `name` defined in `module`, inside the leaf class
    /// (`owner` None) or inside the top-level base class `owner`.
    Nested {
        module: Rc<Module>,
        owner: Option<String>,
        name: String,
    },
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
                keywords: std::mem::take(&mut resolver.keywords),
            }
        })
        .collect()
}

/// AST-level impact analysis: for each test file, the set of project files
/// whose change can affect it — the file itself, the conftest chain above
/// it (plus conftest-declared pytest_plugins modules), the config file, and
/// the transitive closure of imports that resolve inside the rootdir.
/// Third-party modules are treated as stable, so resolution never leaves
/// the project. Keys and members are rootdir-relative forward-slash paths
/// (the `FileTests::path` convention).
pub fn impact_closures(config: &Config, files: &[FileTests]) -> HashMap<String, HashSet<String>> {
    let mut resolver = Resolver::new(config, None);
    let config_key = config
        .source
        .as_ref()
        .map(|src| display_path(src, &config.rootdir));
    let mut direct: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    let mut closures = HashMap::new();
    for file in files {
        let mut seen: HashSet<PathBuf> = HashSet::new();
        let mut stack: Vec<PathBuf> = vec![file.abs_path.clone()];
        if let Some(dir) = file.abs_path.parent() {
            for conftest in resolver.conftest_chain(dir) {
                stack.push(conftest.path.clone());
            }
        }
        while let Some(path) = stack.pop() {
            if !seen.insert(path.clone()) {
                continue;
            }
            if !direct.contains_key(&path) {
                let deps = direct_deps(&mut resolver, &path);
                direct.insert(path.clone(), deps);
            }
            for dep in &direct[&path] {
                if !seen.contains(dep) {
                    stack.push(dep.clone());
                }
            }
        }
        let mut keys: HashSet<String> = seen
            .iter()
            .map(|p| display_path(p, &config.rootdir))
            .collect();
        if let Some(key) = &config_key {
            keys.insert(key.clone());
        }
        closures.insert(file.path.clone(), keys);
    }
    closures
}

/// Project-local files a module imports directly: absolute imports resolved
/// against the rootdir (and `src/` or the importer's package root), relative
/// imports against the importer, plus conftest `pytest_plugins` modules.
/// Only paths under the rootdir count.
fn direct_deps(resolver: &mut Resolver, path: &Path) -> Vec<PathBuf> {
    let Some(module) = resolver.module(path) else {
        return Vec::new();
    };
    let mut refs: Vec<ModuleRef> = Vec::new();
    for dotted in &module.imported_modules {
        refs.push(ModuleRef::Absolute(dotted.clone()));
    }
    for import in module.imports.values() {
        match import {
            Import::Module(dotted) => refs.push(ModuleRef::Absolute(dotted.clone())),
            Import::From(mref, name) => {
                refs.push(mref.clone());
                refs.push(resolver.resolve_child(mref, name));
            }
        }
    }
    refs.extend(module.star_imports.iter().cloned());
    for plugin in &module.plugin_modules {
        refs.push(ModuleRef::Absolute(plugin.clone()));
    }
    let rootdir = resolver.config.rootdir.clone();
    let mut deps: HashSet<PathBuf> = HashSet::new();
    for mref in refs {
        for candidate in local_candidates(resolver.config, &mref, &module.dir) {
            if !candidate.starts_with(&rootdir) {
                continue;
            }
            if let Some(dep) = resolver.module(&candidate) {
                deps.insert(dep.path.clone());
                break;
            }
        }
    }
    deps.into_iter().collect()
}

/// Candidate files for a module reference, restricted to project roots —
/// `resolve_ref`'s search order minus the interpreter's sys.path.
fn local_candidates(config: &Config, mref: &ModuleRef, importer_dir: &Path) -> Vec<PathBuf> {
    match mref {
        ModuleRef::Relative(base) => {
            vec![base.with_extension("py"), base.join("__init__.py")]
        }
        ModuleRef::Absolute(dotted) => {
            let mut rel = PathBuf::new();
            for seg in dotted.split('.') {
                rel = rel.join(seg);
            }
            let mut roots = vec![config.rootdir.clone(), config.rootdir.join("src")];
            if let Some(pkg_root) = package_root_above(importer_dir) {
                roots.push(pkg_root);
            }
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
    }
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

    if module.not_test || is_collect_ignored(resolver, &module.path) {
        return Vec::new();
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
    dead.extend(module.deleted_names.iter().cloned());

    // A pytest_generate_tests hook or a parametrized autouse fixture
    // anywhere in scope can add parameters we cannot see; exact expansions
    // are no longer trustworthy.
    let poisoned = contexts
        .iter()
        .any(|m| m.has_generate_tests || has_autouse_params(&m.fixtures));

    let mut tests = Vec::new();
    for item in &module.order {
        match item {
            TopItem::Func(def) => {
                if dead.contains(&def.name) || module.not_test_funcs.contains(&def.name) {
                    continue;
                }
                if resolver.config.function_matches(&def.name) {
                    emit_function(
                        resolver, module, def, &def.name, &contexts, poisoned, marker, &mut tests,
                    );
                }
            }
            TopItem::Class(name) => {
                if dead.contains(name) {
                    continue;
                }
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
            // `X = SomeStateMachine.TestCase` bindings (hypothesis
            // stateful): a unittest TestCase whose single test method is
            // runTest.
            TopItem::Synthetic(name) => {
                if dead.contains(name) || module.classes.contains_key(name) {
                    continue;
                }
                if let Some(expr) = marker {
                    let names: HashSet<String> = module.pytestmark.iter().cloned().collect();
                    if !expr.matches_names(&names) {
                        continue;
                    }
                }
                let key = format!("{name}::runTest");
                resolver
                    .keywords
                    .insert(key.clone(), module.pytestmark.clone());
                tests.push(key);
            }
            // pytest collects over the module NAMESPACE: test classes and
            // functions *imported* into a test module are collected here
            // too, at the import's position (the classic urllib3 contrib
            // pattern: `from ..test_https import TestHTTPS`).
            TopItem::Import(local) => {
                emit_imported(
                    resolver, module, local, &contexts, poisoned, marker, &mut tests,
                );
            }
        }
    }
    tests
}

/// Emit an imported test class/function bound as `local` in `module`.
fn emit_imported(
    resolver: &mut Resolver,
    module: &Rc<Module>,
    local: &str,
    contexts: &[Rc<Module>],
    poisoned: bool,
    marker: Option<&crate::keyword::KExpr>,
    tests: &mut Vec<String>,
) {
    let Some(Import::From(mref, orig)) = module.imports.get(local) else {
        return;
    };
    let looks_like_class = resolver.config.class_matches(local);
    let looks_like_func = resolver.config.function_matches(local);
    if !looks_like_class && !looks_like_func {
        return;
    }
    if module.classes.contains_key(local) || module.functions.contains_key(local) {
        return; // a local definition shadows the import
    }
    if is_unittest_ref(mref, orig) {
        return;
    }
    let Some(target) = resolver.resolve_ref(mref, &module.dir) else {
        return;
    };
    let Some((target, orig)) = resolver.resolve_symbol_or_function(target, orig.clone()) else {
        return;
    };
    if looks_like_class {
        if let Some(class) = target.classes.get(&orig) {
            emit_class(
                resolver,
                &target.clone(),
                class,
                local,
                contexts,
                &module.pytestmark,
                poisoned,
                marker,
                &mut Vec::new(),
                tests,
            );
            return;
        }
    }
    if looks_like_func {
        if let Some(def) = target.functions.get(&orig) {
            if target.not_test_funcs.contains(&orig) {
                return;
            }
            let pytestmark = &module.pytestmark;
            emit_function_with(
                resolver, &target, pytestmark, def, local, contexts, poisoned, marker, tests,
            );
        }
    }
}

/// Emit a module-level test function of `module` under `id_name`.
#[allow(clippy::too_many_arguments)]
fn emit_function(
    resolver: &mut Resolver,
    module: &Rc<Module>,
    def: &TestDef,
    id_name: &str,
    contexts: &[Rc<Module>],
    poisoned: bool,
    marker: Option<&crate::keyword::KExpr>,
    tests: &mut Vec<String>,
) {
    emit_function_with(
        resolver,
        module,
        &module.pytestmark,
        def,
        id_name,
        contexts,
        poisoned,
        marker,
        tests,
    );
}

/// `def_module` resolves the def's mark aliases; `pytestmark` is the
/// collecting module's.
#[allow(clippy::too_many_arguments)]
fn emit_function_with(
    resolver: &mut Resolver,
    def_module: &Rc<Module>,
    pytestmark: &[String],
    def: &TestDef,
    id_name: &str,
    contexts: &[Rc<Module>],
    poisoned: bool,
    marker: Option<&crate::keyword::KExpr>,
    tests: &mut Vec<String>,
) {
    let mut names: HashSet<String> = pytestmark.iter().chain(def.marks.iter()).cloned().collect();
    for candidate in &def.maybe_marks {
        if let Some(mark) = resolver.resolve_mark_alias(def_module, candidate) {
            names.insert(mark);
        }
    }
    if let Some(expr) = marker {
        if !expr.matches_names(&names) {
            return;
        }
    }
    resolver.note_keywords(def_module, id_name.to_string(), &names, def);
    let mut expansion =
        if def.expansion != Expansion::None && requests_parametrized_fixture(contexts, &[], def) {
            Expansion::Fallback
        } else {
            def.expansion.clone()
        };
    // The anyio plugin parametrizes marked tests with the backend fixture,
    // adding ID pieces we cannot see.
    if (poisoned || names.contains("anyio")) && matches!(expansion, Expansion::Params(_)) {
        expansion = Expansion::Fallback;
    }
    tests.extend(expansion.apply(id_name));
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
    let key = (module.path.clone(), name.to_string());
    let resolved = resolver.resolve_class(module, class, key);
    // `__test__ = False` (own or inherited) empties the class, unittest or
    // not; `__test__ = True` collects a class whatever its name.
    if resolved.dunder_test == Some(false) {
        return;
    }
    let unittest = resolved.unittest;
    let collectable = unittest
        || ((resolver.config.class_matches(name) || resolved.dunder_test == Some(true))
            && !resolved.has_ctor);
    if !collectable {
        return;
    }
    // The class chain's parametrized autouse fixtures poison exact
    // expansion for all of its methods.
    let poisoned = poisoned || resolved.base_params || has_autouse_params(&resolved.fixtures);

    let mut items: Vec<&ResolvedItem> = resolved.items.iter().collect();
    if unittest {
        // unittest's TestLoader.getTestCaseNames: every callable attribute
        // starting with "test", sorted by name. Nested classes are callable
        // attributes too, never collectors of their own.
        items.retain(|item| item_name(item).starts_with("test"));
        items.sort_by(|a, b| item_name(a).cmp(item_name(b)));
    }

    stack.push(name.to_string());
    let class_expansion = if resolved.base_params {
        Expansion::Fallback
    } else {
        class.expansion.clone()
    };
    for item in items {
        let def = match item {
            ResolvedItem::Method(def) => def,
            ResolvedItem::Nested {
                module: nested_mod,
                owner,
                name: nested_name,
            } => {
                if unittest {
                    if class_marks_match(pytestmark, &resolved.marks, marker) {
                        out.push(format!("{}::{}", stack.join("::"), nested_name));
                    }
                    continue;
                }
                let owner_class = match owner {
                    None => Some(class),
                    Some(owner) => nested_mod.classes.get(owner),
                };
                let nested = owner_class.and_then(|c| {
                    c.items.iter().find_map(|i| match i {
                        ClassItem::Nested(n, nested) if n == nested_name => Some(nested),
                        _ => None,
                    })
                });
                if let Some(nested) = nested {
                    emit_class(
                        resolver,
                        nested_mod,
                        nested,
                        nested_name,
                        contexts,
                        pytestmark,
                        poisoned,
                        marker,
                        stack,
                        out,
                    );
                }
                continue;
            }
        };
        if def.not_test || (!unittest && !resolver.config.function_matches(&def.name)) {
            continue;
        }
        let mut names: HashSet<String> = pytestmark
            .iter()
            .chain(resolved.marks.iter())
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
        let key = format!("{}::{}", stack.join("::"), def.name);
        resolver.note_keywords(module, key, &names, def);
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
                || requests_parametrized_fixture(contexts, &[&resolved.fixtures], &request)
            {
                combined = Expansion::Fallback;
            }
        }
        for id in combined.apply(&def.name) {
            out.push(format!("{}::{}", stack.join("::"), id));
        }
    }
    stack.pop();
}

fn item_name(item: &ResolvedItem) -> &str {
    match item {
        ResolvedItem::Method(def) => def.name.as_str(),
        ResolvedItem::Nested { name, .. } => name.as_str(),
    }
}

fn class_marks_match(
    pytestmark: &[String],
    class_marks: &[String],
    marker: Option<&crate::keyword::KExpr>,
) -> bool {
    let Some(expr) = marker else {
        return true;
    };
    let names: HashSet<String> = pytestmark.iter().chain(class_marks).cloned().collect();
    expr.matches_names(&names)
}

/// pytest's `pytest_ignore_collect` for conftest `collect_ignore` /
/// `collect_ignore_glob`, applied to the file and every directory between
/// it and the rootdir (pytest checks each path as its walk reaches it). For
/// a path, only the NEAREST conftest at or above its parent that defines
/// the list counts; entries are relative to that conftest's directory.
/// Plain entries match a path exactly (so a directory entry prunes the
/// whole subtree); glob entries use fnmatch, where `*` crosses `/`.
fn is_collect_ignored(resolver: &mut Resolver, file: &Path) -> bool {
    let rootdir = resolver.config.rootdir.clone();
    let mut current = Some(file);
    while let Some(path) = current {
        if path == rootdir || !path.starts_with(&rootdir) {
            break;
        }
        let Some(parent) = path.parent() else {
            break;
        };
        let chain = resolver.conftest_chain(parent);
        if let Some(conftest) = chain.iter().find(|c| c.collect_ignore.is_some()) {
            let entries = conftest.collect_ignore.as_deref().unwrap_or_default();
            if entries
                .iter()
                .any(|e| normalize_path(&conftest.dir.join(e)) == path)
            {
                return true;
            }
        }
        if let Some(conftest) = chain.iter().find(|c| c.collect_ignore_glob.is_some()) {
            let patterns = conftest.collect_ignore_glob.as_deref().unwrap_or_default();
            for pattern in patterns {
                let full = normalize_path(&conftest.dir.join(pattern));
                let matched = globset::GlobBuilder::new(&full.to_string_lossy())
                    .literal_separator(false)
                    .backslash_escape(false)
                    .build()
                    .map(|g| g.compile_matcher().is_match(path))
                    .unwrap_or(false);
                if matched {
                    return true;
                }
            }
        }
        current = Some(parent);
    }
    false
}

/// Lexical `os.path.abspath`-style normalization of `.` and `..`.
fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
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
            // pytest yields inherited members first (reverse MRO order).
            vec![
                "TestChild::test_from_base",
                "TestChild::test_own",
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
