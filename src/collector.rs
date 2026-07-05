use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use ignore::WalkBuilder;
use rayon::prelude::*;
use ruff_python_ast::{self as ast, Expr, Stmt};
use serde::Serialize;

use crate::config::Config;
use crate::params::{self, Expansion};

#[derive(Debug, Serialize)]
pub struct FileTests {
    /// rootdir-relative, forward-slash path — the node ID prefix.
    pub path: String,
    /// Absolute path, used to build node IDs that pytest can run from any cwd.
    #[serde(skip)]
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
    expansion: Expansion,
    has_ctor: bool,
}

#[derive(Debug)]
enum TopItem {
    Func(TestDef),
    Class(String),
}

#[derive(Debug)]
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
    fixtures: HashMap<String, Fixture>,
    order: Vec<TopItem>,
    /// Module names demanded via module-level `pytest.importorskip(...)`.
    skip_requires: Vec<String>,
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
    }
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
        classes: HashMap::new(),
        fixtures: HashMap::new(),
        order: Vec::new(),
        skip_requires: Vec::new(),
        has_generate_tests: false,
    };
    let mut aliases = params::ParamAliases::new();
    scan(&syntax.body, &mut module, &mut aliases, true);
    Ok(module)
}

/// Walk statements. Definitions only count at true top level; imports are
/// also harvested from inside `if`/`try`/`with` blocks (the common
/// conditional-import patterns), since over-approximating imports is safe.
fn scan(stmts: &[Stmt], module: &mut Module, aliases: &mut params::ParamAliases, top: bool) {
    for stmt in stmts {
        match stmt {
            Stmt::FunctionDef(func) if top => {
                if func.name.as_str() == "pytest_generate_tests" {
                    module.has_generate_tests = true;
                    continue;
                }
                // Fixtures are never collected as tests, even test-named ones.
                if let Some(flags) = params::fixture_info(&func.decorator_list) {
                    module.fixtures.insert(
                        func.name.to_string(),
                        Fixture {
                            parametrized: flags.parametrized,
                            autouse: flags.autouse,
                            deps: parameter_names(&func.parameters),
                        },
                    );
                    continue;
                }
                module.order.push(TopItem::Func(test_def(func, aliases)));
            }
            Stmt::ClassDef(class) if top => {
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
                }
                // `mpl = pytest.importorskip("matplotlib")`.
                if let Some(name) = importorskip_name(&assign.value) {
                    module.skip_requires.push(name);
                }
            }
            Stmt::Expr(expr_stmt) => {
                // Bare `pytest.importorskip("numba")` at module level.
                if let Some(name) = importorskip_name(&expr_stmt.value) {
                    module.skip_requires.push(name);
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
            Stmt::If(if_stmt) => {
                scan(&if_stmt.body, module, aliases, false);
                for clause in &if_stmt.elif_else_clauses {
                    scan(&clause.body, module, aliases, false);
                }
            }
            Stmt::Try(try_stmt) => {
                scan(&try_stmt.body, module, aliases, false);
                for handler in &try_stmt.handlers {
                    let ast::ExceptHandler::ExceptHandler(h) = handler;
                    scan(&h.body, module, aliases, false);
                }
                scan(&try_stmt.orelse, module, aliases, false);
                scan(&try_stmt.finalbody, module, aliases, false);
            }
            Stmt::With(with_stmt) => scan(&with_stmt.body, module, aliases, false),
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
                    fixtures.insert(
                        name.to_string(),
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
    Class {
        bases,
        items,
        fixtures,
        expansion: class_info.expansion,
        has_ctor,
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
}

impl<'a> Resolver<'a> {
    fn new(config: &'a Config, probe_python: Option<String>) -> Self {
        Resolver {
            config,
            cache: HashMap::new(),
            probe_python,
            probe_cache: HashMap::new(),
        }
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
                "import importlib.util, sys; sys.exit(0 if importlib.util.find_spec({name:?}) else 1)"
            ))
            .output()
            .map(|out| out.status.success())
            .unwrap_or(true);
        self.probe_cache.insert(name.to_string(), ok);
        ok
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

    fn resolve_base(&mut self, module: &Rc<Module>, text: &str) -> BaseTarget {
        let segments: Vec<&str> = text.split('.').collect();
        if segments.len() == 1 {
            let name = segments[0];
            match module.imports.get(name).cloned() {
                Some(Import::From(mref, orig)) => {
                    if is_unittest_ref(&mref, &orig) {
                        return BaseTarget::Unittest;
                    }
                    match self.resolve_ref(&mref, &module.dir) {
                        Some(target) => BaseTarget::Local(target, orig),
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
            match self.resolve_ref(&mref, &module.dir) {
                Some(target) => BaseTarget::Local(target, last.to_string()),
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
    ) -> (Vec<TestDef>, bool, bool) {
        visited.insert(key);
        let mut methods: Vec<TestDef> = Vec::new();
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
                    let key = (target_mod.path.clone(), target_name.clone());
                    if visited.contains(&key) {
                        continue;
                    }
                    let Some(target_class) = target_mod.classes.get(&target_name) else {
                        continue;
                    };
                    base_params |= target_class.expansion != Expansion::None
                        || has_autouse_params(&target_class.fixtures);
                    let (inherited, base_ut, base_bp) =
                        self.resolve_class(&target_mod, target_class, key, visited);
                    unittest |= base_ut;
                    base_params |= base_bp;
                    for def in inherited {
                        if seen.insert(def.name.clone()) {
                            methods.push(def);
                        }
                    }
                }
                BaseTarget::Unknown => {}
            }
        }
        (methods, unittest, base_params)
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

fn is_unittest_ref(mref: &ModuleRef, name: &str) -> bool {
    matches!(
        mref,
        ModuleRef::Absolute(dotted)
            if (dotted == "unittest" || dotted == "unittest.case") && name == "TestCase"
    )
}

// ---------------------------------------------------------------------------
// Emission
// ---------------------------------------------------------------------------

/// Collect tests from all roots, honoring `config`. Test files are parsed in
/// parallel; base-class modules are parsed lazily during resolution.
pub fn collect(roots: &[PathBuf], config: &Config, probe_python: Option<&str>) -> Vec<FileTests> {
    let files = discover(roots, config);
    let parsed: Vec<Option<Module>> = files.par_iter().map(|p| parse_file(p)).collect();

    let mut resolver = Resolver::new(config, probe_python.map(str::to_string));
    for (path, module) in files.iter().zip(parsed) {
        resolver.preload(path.clone(), module);
    }

    files
        .iter()
        .map(|abs| {
            let tests = resolver
                .module(abs)
                .map(|module| emit_module(&mut resolver, &module))
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
            emit_module(&mut resolver, &module)
        }
        Err(_) => Vec::new(),
    }
}

fn emit_module(resolver: &mut Resolver, module: &Rc<Module>) -> Vec<String> {
    // Fixture visibility for tests in this module: the module itself plus
    // its conftest chain.
    let mut contexts = vec![module.clone()];
    contexts.extend(resolver.conftest_chain(&module.dir));

    // With a probe python, module-level `importorskip` in the file or its
    // conftest chain drops the whole module when the dependency is absent,
    // matching pytest's behavior in that environment.
    if resolver.probe_python.is_some() {
        let requires: Vec<String> = contexts
            .iter()
            .flat_map(|m| m.skip_requires.iter().cloned())
            .collect();
        if requires.iter().any(|name| !resolver.probe_ok(name)) {
            return Vec::new();
        }
    }

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
                if resolver.config.function_matches(&def.name) {
                    let mut expansion = if def.expansion != Expansion::None
                        && requests_parametrized_fixture(&contexts, &[], def)
                    {
                        Expansion::Fallback
                    } else {
                        def.expansion.clone()
                    };
                    if poisoned && matches!(expansion, Expansion::Params(_)) {
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
                    poisoned,
                    &mut Vec::new(),
                    &mut tests,
                );
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
    poisoned: bool,
    stack: &mut Vec<String>,
    out: &mut Vec<String>,
) {
    let mut visited = HashSet::new();
    let key = (module.path.clone(), name.to_string());
    let (methods, unittest, base_params) = resolver.resolve_class(module, class, key, &mut visited);
    // The class's own (or inherited) parametrized autouse fixtures poison
    // exact expansion for all of its methods.
    let poisoned = poisoned || base_params || has_autouse_params(&class.fixtures);

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
        // The leaf module's fixture visibility applies to inherited methods.
        let expansion = if def.expansion != Expansion::None
            && requests_parametrized_fixture(contexts, &[&class.fixtures], def)
        {
            Expansion::Fallback
        } else {
            def.expansion.clone()
        };
        let mut combined = Expansion::combine(&class_expansion, &expansion);
        if poisoned && matches!(combined, Expansion::Params(_)) {
            combined = Expansion::Fallback;
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
                poisoned,
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
