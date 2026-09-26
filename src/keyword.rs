//! pytest-style `-k` / `-m` expressions: fragments combined with `and`,
//! `or`, `not`, and parentheses. For `-k` a fragment matches if it is a
//! case-insensitive substring of any name in the item's keyword set
//! (pytest's `KeywordMatcher`: node names up the chain, marker names, ...);
//! for `-m` it must be one of the item's marker names.

#[derive(Debug, PartialEq)]
pub enum KExpr {
    Or(Box<KExpr>, Box<KExpr>),
    And(Box<KExpr>, Box<KExpr>),
    Not(Box<KExpr>),
    Frag(String),
    /// The empty expression, which pytest evaluates to False.
    Empty,
}

impl KExpr {
    /// `-k` semantics: a fragment is a case-insensitive substring of any
    /// one name (so `a::b` never spans two names). `names` must already be
    /// lowercased.
    pub fn matches(&self, names: &[String]) -> bool {
        match self {
            KExpr::Or(a, b) => a.matches(names) || b.matches(names),
            KExpr::And(a, b) => a.matches(names) && b.matches(names),
            KExpr::Not(inner) => !inner.matches(names),
            KExpr::Frag(frag) => {
                let frag = frag.to_lowercase();
                names.iter().any(|name| name.contains(frag.as_str()))
            }
            KExpr::Empty => false,
        }
    }

    /// `-m` semantics: exact, case-sensitive mark-name membership.
    pub fn matches_names(&self, names: &std::collections::HashSet<String>) -> bool {
        match self {
            KExpr::Or(a, b) => a.matches_names(names) || b.matches_names(names),
            KExpr::And(a, b) => a.matches_names(names) && b.matches_names(names),
            KExpr::Not(inner) => !inner.matches_names(names),
            KExpr::Frag(frag) => names.contains(frag.as_str()),
            KExpr::Empty => false,
        }
    }
}

fn tokenize(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for c in input.chars() {
        match c {
            '(' | ')' => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
                tokens.push(c.to_string());
            }
            c if c.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

struct Parser {
    tokens: Vec<String>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&str> {
        self.tokens.get(self.pos).map(String::as_str)
    }

    fn next(&mut self) -> Option<String> {
        let token = self.tokens.get(self.pos).cloned();
        self.pos += 1;
        token
    }

    fn expr(&mut self) -> Result<KExpr, String> {
        let mut left = self.and_expr()?;
        while self.peek() == Some("or") {
            self.next();
            let right = self.and_expr()?;
            left = KExpr::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn and_expr(&mut self) -> Result<KExpr, String> {
        let mut left = self.not_expr()?;
        while self.peek() == Some("and") {
            self.next();
            let right = self.not_expr()?;
            left = KExpr::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn not_expr(&mut self) -> Result<KExpr, String> {
        match self.peek() {
            Some("not") => {
                self.next();
                Ok(KExpr::Not(Box::new(self.not_expr()?)))
            }
            Some("(") => {
                self.next();
                let inner = self.expr()?;
                if self.next().as_deref() != Some(")") {
                    return Err("expected ')'".to_string());
                }
                Ok(inner)
            }
            Some(")") => Err("unexpected ')'".to_string()),
            Some(_) => {
                let token = self.next().expect("peeked");
                if token == "and" || token == "or" {
                    return Err(format!("unexpected keyword {token:?}"));
                }
                Ok(KExpr::Frag(token))
            }
            None => Err("unexpected end of expression".to_string()),
        }
    }
}

pub fn parse(input: &str) -> Result<KExpr, String> {
    let mut parser = Parser {
        tokens: tokenize(input),
        pos: 0,
    };
    if parser.tokens.is_empty() {
        return Ok(KExpr::Empty);
    }
    let expr = parser.expr()?;
    if parser.pos != parser.tokens.len() {
        return Err(format!(
            "unexpected trailing token {:?}",
            parser.tokens[parser.pos]
        ));
    }
    Ok(expr)
}

/// A `-k` option value: pytest strips leading whitespace and treats an
/// empty result as "no keyword filter".
pub fn parse_keyword_option(value: &str) -> Result<Option<KExpr>, String> {
    let value = value.trim_start();
    if value.is_empty() {
        return Ok(None);
    }
    parse(value).map(Some)
}

/// A `-m` option value: only the empty string disables the filter; a
/// whitespace-only expression is the empty expression, which is False and
/// deselects everything (pytest 9 behavior).
pub fn parse_marker_option(value: &str) -> Result<Option<KExpr>, String> {
    if value.is_empty() {
        return Ok(None);
    }
    parse(value).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(expr: &str, candidate: &str) -> bool {
        let names: Vec<String> = candidate.split("::").map(|n| n.to_lowercase()).collect();
        parse(expr).unwrap().matches(&names)
    }

    #[test]
    fn fragments_and_booleans() {
        assert!(m("http", "test_api.py::TestHttp::test_get"));
        assert!(!m("grpc", "test_api.py::TestHttp::test_get"));
        assert!(m("http and get", "test_api.py::TestHttp::test_get"));
        assert!(!m("http and not get", "test_api.py::TestHttp::test_get"));
        assert!(m("grpc or http", "test_api.py::TestHttp::test_get"));
        assert!(m("not (grpc or ftp)", "test_api.py::TestHttp::test_get"));
        assert!(m("TESTHTTP", "test_api.py::testhttp::test_get"));
        // A fragment never spans two names.
        assert!(!m("py::TestHttp", "test_api.py::TestHttp::test_get"));
    }

    #[test]
    fn empty_options() {
        assert_eq!(parse_keyword_option("").unwrap(), None);
        assert_eq!(parse_keyword_option("  ").unwrap(), None);
        assert_eq!(parse_marker_option("").unwrap(), None);
        let blank = parse_marker_option(" ").unwrap().unwrap();
        assert!(!blank.matches_names(&["slow".to_string()].into()));
    }

    #[test]
    fn mark_name_matching() {
        let names: std::collections::HashSet<String> =
            ["slow".to_string(), "network".to_string()].into();
        assert!(parse("slow").unwrap().matches_names(&names));
        assert!(!parse("not slow").unwrap().matches_names(&names));
        assert!(parse("slow and network").unwrap().matches_names(&names));
        assert!(!parse("SLOW").unwrap().matches_names(&names)); // case-sensitive
    }

    #[test]
    fn errors() {
        assert!(parse("and http").is_err());
        assert!(parse("(http").is_err());
        assert!(parse("http)").is_err());
        assert_eq!(parse("").unwrap(), KExpr::Empty);
    }
}
