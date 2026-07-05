use ruff_python_ast::{self as ast, Expr};

/// Static expansion of `@pytest.mark.parametrize` into pytest's `[...]` ID
/// suffixes. Only literal scalars we can render exactly like pytest are
/// expanded (str/int/bool/None and tuples of those, plus literal `ids=`);
/// anything else — floats, objects, `indirect=`, computed values — yields
/// `Fallback`, meaning cito emits the bare test name and lets pytest expand
/// at run time. Piece ordering is pinned by tests/fixtures against pytest.
#[derive(Debug, Clone, PartialEq)]
pub enum Expansion {
    /// Not parametrized.
    None,
    /// Parametrized with fully-resolved suffixes, e.g. `["1-x", "2-x"]`.
    Params(Vec<String>),
    /// Parametrized in a way we cannot resolve statically.
    Fallback,
}

impl Expansion {
    /// Apply this expansion to a bare test name.
    pub fn apply(&self, name: &str) -> Vec<String> {
        match self {
            Expansion::None | Expansion::Fallback => vec![name.to_string()],
            Expansion::Params(suffixes) => {
                suffixes.iter().map(|s| format!("{name}[{s}]")).collect()
            }
        }
    }

    /// Combine a class-level expansion with a method-level one. pytest merges
    /// all parameter sets into a single bracket; the ordering is pinned by
    /// the fixture tree. Any Fallback poisons the result.
    pub fn combine(class: &Expansion, method: &Expansion) -> Expansion {
        match (class, method) {
            (Expansion::None, other) | (other, Expansion::None) => other.clone(),
            (Expansion::Fallback, _) | (_, Expansion::Fallback) => Expansion::Fallback,
            (Expansion::Params(outer), Expansion::Params(inner)) => {
                // Method params vary fastest; class params appended last,
                // mirroring stacked decorators (verified against pytest).
                let mut combined = Vec::with_capacity(outer.len() * inner.len());
                for o in outer {
                    for i in inner {
                        combined.push(format!("{i}-{o}"));
                    }
                }
                Expansion::Params(combined)
            }
        }
    }
}

/// Module-level parametrize aliases: `NAME = pytest.mark.parametrize(...)`.
/// Some(pieces) when statically resolvable, None when it must fall back.
pub type ParamAliases = std::collections::HashMap<String, Option<Vec<String>>>;

/// Everything the decorator list tells us about a test.
pub struct DecoratorInfo {
    pub expansion: Expansion,
    /// Fixture names pulled in via `@pytest.mark.usefixtures(...)`.
    pub extra_fixture_requests: Vec<String>,
}

/// Analyze a decorator list. Multiple stacked `parametrize` decorators
/// produce the cartesian product; pytest orders ID pieces
/// bottom-decorator-first and iterates the bottom decorator fastest (pinned
/// by tests/fixtures/basic/test_params.py). A decorator we cannot classify
/// as ID-neutral poisons any expansion to Fallback — wrong bracket IDs are
/// worse than bare names.
pub fn from_decorators(decorators: &[ast::Decorator], aliases: &ParamAliases) -> DecoratorInfo {
    let mut sets: Vec<Vec<String>> = Vec::new();
    let mut extra_fixture_requests = Vec::new();
    let mut poisoned = false;
    let mut parametrized = false;
    for decorator in decorators {
        match &decorator.expression {
            Expr::Call(call) if is_parametrize(&call.func) => {
                parametrized = true;
                match parametrize_pieces(call) {
                    Some(pieces) => sets.push(pieces),
                    None => poisoned = true,
                }
            }
            Expr::Call(call) if is_usefixtures(&call.func) => {
                for arg in call.arguments.args.iter() {
                    match string_value(arg) {
                        Some(name) => extra_fixture_requests.push(name),
                        None => poisoned = true,
                    }
                }
            }
            Expr::Name(name) if aliases.contains_key(name.id.as_str()) => {
                parametrized = true;
                match &aliases[name.id.as_str()] {
                    Some(pieces) => sets.push(pieces.clone()),
                    None => poisoned = true,
                }
            }
            expr => {
                if !is_id_neutral(expr) {
                    poisoned = true;
                }
            }
        }
    }
    // A test that IS parametrized but whose IDs we cannot fully resolve must
    // fall back to the bare name — never expand a partial/wrong set.
    let expansion = if poisoned && parametrized {
        Expansion::Fallback
    } else if sets.is_empty() {
        Expansion::None
    } else {
        // Bottom decorator = last in source order = first piece, fastest loop.
        sets.reverse();
        let mut suffixes: Vec<String> = vec![String::new()];
        for set in &sets {
            let mut next = Vec::with_capacity(suffixes.len() * set.len());
            for piece in set {
                for existing in &suffixes {
                    if existing.is_empty() {
                        next.push(piece.clone());
                    } else {
                        next.push(format!("{existing}-{piece}"));
                    }
                }
            }
            suffixes = next;
        }
        Expansion::Params(suffixes)
    };
    DecoratorInfo {
        expansion,
        extra_fixture_requests,
    }
}

/// Record `NAME = pytest.mark.parametrize(...)` module-level aliases.
pub fn parametrize_alias(value: &Expr) -> Option<Option<Vec<String>>> {
    let Expr::Call(call) = value else {
        return None;
    };
    if !is_parametrize(&call.func) {
        return None;
    }
    Some(parametrize_pieces(call))
}

/// Decorators known not to alter node IDs: any pytest mark (parametrize and
/// usefixtures are special-cased before this), mock.patch and friends, and
/// plain function wrappers.
fn is_id_neutral(expr: &Expr) -> bool {
    let target = match expr {
        Expr::Call(call) => &*call.func,
        other => other,
    };
    match target {
        Expr::Name(name) => matches!(
            name.id.as_str(),
            "staticmethod" | "classmethod" | "property" | "abstractmethod" | "patch"
        ),
        Expr::Attribute(_) => {
            let Some(chain) = dotted(target) else {
                return false;
            };
            chain.split('.').any(|seg| seg == "mark")
                || chain.starts_with("mock.")
                || chain.starts_with("unittest.mock.")
                || chain.split('.').any(|seg| seg == "patch")
        }
        _ => false,
    }
}

fn dotted(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Name(name) => Some(name.id.to_string()),
        Expr::Attribute(attr) => Some(format!("{}.{}", dotted(&attr.value)?, attr.attr)),
        _ => None,
    }
}

fn is_usefixtures(func: &Expr) -> bool {
    match func {
        Expr::Attribute(attr) => attr.attr.as_str() == "usefixtures",
        Expr::Name(name) => name.id.as_str() == "usefixtures",
        _ => false,
    }
}

fn is_parametrize(func: &Expr) -> bool {
    match func {
        Expr::Attribute(attr) => attr.attr.as_str() == "parametrize",
        Expr::Name(name) => name.id.as_str() == "parametrize",
        _ => false,
    }
}

/// All argnames claimed by `parametrize` decorators — these function
/// parameters are params, not fixture requests.
pub fn decorator_argnames(decorators: &[ast::Decorator]) -> Vec<String> {
    let mut names = Vec::new();
    for decorator in decorators {
        let Expr::Call(call) = &decorator.expression else {
            continue;
        };
        if !is_parametrize(&call.func) || call.arguments.args.is_empty() {
            continue;
        }
        match &call.arguments.args[0] {
            Expr::StringLiteral(s) => {
                names.extend(s.value.to_str().split(',').map(|n| n.trim().to_string()));
            }
            Expr::Tuple(t) => names.extend(t.elts.iter().filter_map(string_value)),
            Expr::List(l) => names.extend(l.elts.iter().filter_map(string_value)),
            _ => {}
        }
    }
    names
}

fn string_value(expr: &Expr) -> Option<String> {
    match expr {
        Expr::StringLiteral(s) => Some(s.value.to_str().to_string()),
        _ => None,
    }
}

pub struct FixtureFlags {
    /// `params=` of any shape counts as parametrized (conservative).
    pub parametrized: bool,
    /// `autouse=` fixtures apply to every test in scope; a parametrized
    /// autouse fixture invalidates all exact expansions in that scope.
    pub autouse: bool,
}

/// Is this decorator list a `@pytest.fixture` (optionally with arguments)?
pub fn fixture_info(decorators: &[ast::Decorator]) -> Option<FixtureFlags> {
    for decorator in decorators {
        let (func, keywords): (&Expr, &[ast::Keyword]) = match &decorator.expression {
            Expr::Call(call) => (&call.func, &call.arguments.keywords),
            other => (other, &[]),
        };
        let is_fixture = match func {
            Expr::Attribute(attr) => attr.attr.as_str() == "fixture",
            Expr::Name(name) => name.id.as_str() == "fixture",
            _ => false,
        };
        if is_fixture {
            let has_kwarg = |name: &str| {
                keywords
                    .iter()
                    .any(|k| k.arg.as_ref().is_some_and(|a| a.as_str() == name))
            };
            return Some(FixtureFlags {
                parametrized: has_kwarg("params"),
                autouse: has_kwarg("autouse"),
            });
        }
    }
    None
}

/// One decorator's worth of ID pieces, or None if not statically resolvable.
fn parametrize_pieces(call: &ast::ExprCall) -> Option<Vec<String>> {
    let args = &call.arguments.args;
    if args.len() < 2 {
        return None;
    }
    for keyword in call.arguments.keywords.iter() {
        match keyword.arg.as_ref().map(|a| a.as_str()) {
            // `indirect=` routes values through fixtures; IDs may differ.
            Some("indirect") => return None,
            Some("ids") => {}
            _ => return None,
        }
    }

    let n_args = argnames_count(&args[0])?;
    let values = elements(&args[1])?;
    if values.is_empty() {
        return None;
    }

    if let Some(ids) = explicit_ids(call) {
        let mut seen = std::collections::HashSet::new();
        let unique = ids.iter().all(|p| seen.insert(p.clone()));
        return (unique && ids.len() == values.len()).then_some(ids);
    }

    let mut pieces = Vec::with_capacity(values.len());
    for value in values {
        pieces.push(piece_for_element(value, n_args)?);
    }
    // pytest disambiguates duplicate values with per-occurrence suffixes
    // (True0/True1) computed per parameter set; rather than replicate that
    // exactly, fall back when a set repeats a rendered piece.
    let mut seen = std::collections::HashSet::new();
    if !pieces.iter().all(|p| seen.insert(p.clone())) {
        return None;
    }
    Some(pieces)
}

/// Render one argvalues element, including `pytest.param(...)` wrappers:
/// `id=` wins, `marks=` is ID-neutral, any other keyword is unknown.
fn piece_for_element(expr: &Expr, n_args: usize) -> Option<String> {
    if let Expr::Call(call) = expr {
        if is_param_call(&call.func) {
            let mut explicit_id = None;
            for keyword in call.arguments.keywords.iter() {
                match keyword.arg.as_ref().map(|a| a.as_str()) {
                    Some("id") => explicit_id = Some(&keyword.value),
                    Some("marks") => {}
                    _ => return None,
                }
            }
            if let Some(value) = explicit_id {
                return match value {
                    Expr::StringLiteral(s) => safe_string(s.value.to_str()),
                    _ => None,
                };
            }
            let args = &call.arguments.args;
            if args.len() != n_args {
                return None;
            }
            let rendered: Option<Vec<String>> = args.iter().map(render_scalar).collect();
            return Some(rendered?.join("-"));
        }
    }
    if n_args == 1 {
        render_scalar(expr)
    } else {
        let parts = elements(expr)?;
        if parts.len() != n_args {
            return None;
        }
        let rendered: Option<Vec<String>> = parts.iter().map(render_scalar).collect();
        Some(rendered?.join("-"))
    }
}

fn is_param_call(func: &Expr) -> bool {
    match func {
        Expr::Attribute(attr) => attr.attr.as_str() == "param",
        Expr::Name(name) => name.id.as_str() == "param",
        _ => false,
    }
}

fn argnames_count(expr: &Expr) -> Option<usize> {
    match expr {
        Expr::StringLiteral(s) => Some(s.value.to_str().split(',').count()),
        Expr::Tuple(t) => Some(t.elts.len()),
        Expr::List(l) => Some(l.elts.len()),
        _ => None,
    }
}

fn elements(expr: &Expr) -> Option<&[Expr]> {
    match expr {
        Expr::List(l) => Some(&l.elts),
        Expr::Tuple(t) => Some(&t.elts),
        _ => None,
    }
}

fn explicit_ids(call: &ast::ExprCall) -> Option<Vec<String>> {
    let ids = call
        .arguments
        .keywords
        .iter()
        .find(|k| k.arg.as_ref().is_some_and(|a| a.as_str() == "ids"))?;
    let elts = elements(&ids.value)?;
    elts.iter()
        .map(|e| match e {
            Expr::StringLiteral(s) => safe_string(s.value.to_str()),
            _ => None,
        })
        .collect()
}

/// Render one literal the way pytest's idmaker does, or None when unsure.
/// Floats are deliberately excluded: Rust and Python disagree on shortest
/// repr in edge cases (1e-07), so we fall back rather than risk a wrong ID.
fn render_scalar(expr: &Expr) -> Option<String> {
    match expr {
        Expr::StringLiteral(s) => safe_string(s.value.to_str()),
        Expr::NumberLiteral(n) => match &n.value {
            ast::Number::Int(i) => Some(i.to_string()),
            _ => None,
        },
        Expr::BooleanLiteral(b) => Some(if b.value { "True" } else { "False" }.to_string()),
        Expr::NoneLiteral(_) => Some("None".to_string()),
        Expr::UnaryOp(u) if matches!(u.op, ast::UnaryOp::USub) => match &*u.operand {
            Expr::NumberLiteral(n) => match &n.value {
                ast::Number::Int(i) => Some(format!("-{i}")),
                _ => None,
            },
            _ => None,
        },
        _ => None,
    }
}

/// pytest escapes non-ascii and unprintable characters in string IDs; rather
/// than replicate that, only accept strings that pass through unchanged.
fn safe_string(s: &str) -> Option<String> {
    (!s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '+')))
    .then(|| s.to_string())
}
