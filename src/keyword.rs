//! pytest-style `-k` keyword expressions: fragments combined with `and`,
//! `or`, `not`, and parentheses. A fragment matches if it is a
//! case-insensitive substring of the candidate (file basename + node ID —
//! an approximation of pytest's keyword set that covers names, classes,
//! files, and parametrize IDs).

#[derive(Debug, PartialEq)]
pub enum KExpr {
    Or(Box<KExpr>, Box<KExpr>),
    And(Box<KExpr>, Box<KExpr>),
    Not(Box<KExpr>),
    Frag(String),
}

impl KExpr {
    /// `candidate` must already be lowercased.
    pub fn matches(&self, candidate: &str) -> bool {
        match self {
            KExpr::Or(a, b) => a.matches(candidate) || b.matches(candidate),
            KExpr::And(a, b) => a.matches(candidate) && b.matches(candidate),
            KExpr::Not(inner) => !inner.matches(candidate),
            KExpr::Frag(frag) => candidate.contains(frag.as_str()),
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
                Ok(KExpr::Frag(token.to_lowercase()))
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
    let expr = parser.expr()?;
    if parser.pos != parser.tokens.len() {
        return Err(format!(
            "unexpected trailing token {:?}",
            parser.tokens[parser.pos]
        ));
    }
    Ok(expr)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(expr: &str, candidate: &str) -> bool {
        parse(expr).unwrap().matches(&candidate.to_lowercase())
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
    }

    #[test]
    fn errors() {
        assert!(parse("and http").is_err());
        assert!(parse("(http").is_err());
        assert!(parse("http)").is_err());
        assert!(parse("").is_err());
    }
}
