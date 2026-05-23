use anyhow::{Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use crate::config::parse_config;
use crate::envelope::Envelope;
use crate::pipeline::ErrorPolicy;
use crate::transforms::{BasicTransform, MapOne, Transform};

/// Drops envelopes that do not match a predicate.
///
/// The predicate is a small expression language evaluated against each
/// envelope. Supported constructs:
///
/// - Field paths: `payload.userId`, `meta.headers.priority`, `meta.key`
/// - Literals: strings (`"high"`), numbers (`42`, `3.14`), booleans (`true`, `false`), `null`
/// - Comparison: `==`, `!=`, `<`, `<=`, `>`, `>=`
/// - Logical: `&&` (and), `||` (or), `!` (not)
/// - Grouping: `(expr)`
/// - Existence: `exists payload.userId`
///
/// Examples:
/// - `meta.headers.priority == "high"`
/// - `payload.userId == 1`
/// - `payload.score > 0.5 && meta.headers.env == "prod"`
/// - `!exists payload.optionalField || payload.optionalField == null`
pub struct FilterTransform {
    id: String,
    predicate: Expr,
}

impl FilterTransform {
    pub fn new(id: impl Into<String>, predicate: Expr) -> Self {
        Self {
            id: id.into(),
            predicate,
        }
    }
}

#[async_trait]
impl MapOne for FilterTransform {
    fn id(&self) -> &str {
        &self.id
    }

    async fn map(&self, env: Envelope) -> Result<Option<Envelope>> {
        let keep = self.predicate.eval(&env)?;
        Ok(if keep { Some(env) } else { None })
    }
}

// ------------------------------------------------------------------
// Expression AST
// ------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Bool(bool),
    Compare {
        left: Box<Expr>,
        op: CompareOp,
        right: Box<Expr>,
    },
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    Exists(Path),
    Path(Path),
    Literal(Value),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// A dotted path like `payload.user.id`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Path {
    segments: Vec<String>,
}

impl Path {
    pub fn from_dotted(s: &str) -> Self {
        Self {
            segments: s.split('.').map(String::from).collect(),
        }
    }

    pub fn eval(&self, env: &Envelope) -> Result<Value> {
        let mut current: Value = match self.segments.first().map(String::as_str) {
            Some("payload") => env.payload.clone(),
            Some("meta") => serde_json::to_value(&env.meta)?,
            Some(other) => bail!("filter: unknown root '{}' in path", other),
            None => bail!("filter: empty path"),
        };

        for segment in &self.segments[1..] {
            match current {
                Value::Object(map) => {
                    current = map.get(segment).cloned().unwrap_or(Value::Null);
                }
                _ => {
                    return Ok(Value::Null);
                }
            }
        }
        Ok(current)
    }

    pub fn exists(&self, env: &Envelope) -> bool {
        if self.segments.is_empty() {
            return false;
        }
        let mut current: Value = match self.segments.first().map(String::as_str) {
            Some("payload") => env.payload.clone(),
            Some("meta") => serde_json::to_value(&env.meta).unwrap_or(Value::Null),
            Some(_) | None => return false,
        };

        for segment in &self.segments[1..] {
            match current {
                Value::Object(map) => {
                    current = match map.get(segment) {
                        Some(v) => v.clone(),
                        None => return false,
                    };
                }
                _ => return false,
            }
        }
        true
    }
}

impl Expr {
    pub fn eval(&self, env: &Envelope) -> Result<bool> {
        match self {
            Expr::Bool(b) => Ok(*b),
            Expr::Compare { left, op, right } => {
                let lv = left.value(env)?;
                let rv = right.value(env)?;
                Ok(compare_values(&lv, *op, &rv))
            }
            Expr::And(a, b) => Ok(a.eval(env)? && b.eval(env)?),
            Expr::Or(a, b) => Ok(a.eval(env)? || b.eval(env)?),
            Expr::Not(e) => Ok(!e.eval(env)?),
            Expr::Exists(path) => Ok(path.exists(env)),
            Expr::Path(path) => Ok(truthy(&path.eval(env)?)),
            Expr::Literal(v) => Ok(truthy(v)),
        }
    }

    fn value(&self, env: &Envelope) -> Result<Value> {
        match self {
            Expr::Path(path) => path.eval(env),
            Expr::Literal(v) => Ok(v.clone()),
            Expr::Bool(b) => Ok(Value::Bool(*b)),
            other => bail!("filter: expected path or literal, got {:?}", other),
        }
    }
}

/// Map an escape-sequence character to its decoded form. Unknown escapes
/// pass through verbatim (e.g. `\é` → `é`), matching the previous behavior.
fn unescape(escaped: char) -> char {
    match escaped {
        'n' => '\n',
        'r' => '\r',
        't' => '\t',
        other => other,
    }
}

fn compare_values(left: &Value, op: CompareOp, right: &Value) -> bool {
    match op {
        CompareOp::Eq => left == right,
        CompareOp::Ne => left != right,
        _ => compare_relational(left, op, right),
    }
}

/// Relational comparisons (`<`, `<=`, `>`, `>=`) only match when both sides
/// are the same comparable kind. Null (missing field) or mixed types are
/// treated as non-matches.
fn compare_relational(left: &Value, op: CompareOp, right: &Value) -> bool {
    if let (Some(l), Some(r)) = (as_f64(left), as_f64(right)) {
        return apply_ord(l, op, r);
    }
    if let (Some(l), Some(r)) = (left.as_str(), right.as_str()) {
        return apply_ord(l, op, r);
    }
    false
}

fn apply_ord<T: PartialOrd>(l: T, op: CompareOp, r: T) -> bool {
    match op {
        CompareOp::Lt => l < r,
        CompareOp::Le => l <= r,
        CompareOp::Gt => l > r,
        CompareOp::Ge => l >= r,
        // `compare_values` only routes relational ops here.
        CompareOp::Eq | CompareOp::Ne => unreachable!(),
    }
}

fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

// ------------------------------------------------------------------
// Parser
// ------------------------------------------------------------------

pub fn parse_predicate(input: &str) -> Result<Expr> {
    let mut lexer = Lexer::new(input);
    let expr = parse_expr(&mut lexer);
    if let Some(err) = lexer.error {
        return Err(err);
    }
    let expr = expr?;
    if !matches!(lexer.peek(), Some(Token::Eof) | None) {
        let rest: String = lexer.input[lexer.pos..].trim().into();
        bail!("filter: unexpected trailing tokens: '{}'", rest);
    }
    Ok(expr)
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Ident(String),
    Str(String),
    Number(serde_json::Number),
    Bool(bool),
    Null,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    Not,
    LParen,
    RParen,
    Dot,
    Exists,
    Eof,
}

struct Lexer<'a> {
    input: &'a str,
    pos: usize,
    current: Option<Token>,
    error: Option<anyhow::Error>,
}

impl<'a> Lexer<'a> {
    fn new(input: &'a str) -> Self {
        let mut l = Self {
            input,
            pos: 0,
            current: None,
            error: None,
        };
        l.advance();
        l
    }

    fn advance(&mut self) {
        if self.error.is_some() {
            self.current = Some(Token::Eof);
            return;
        }
        self.skip_whitespace();
        if self.pos >= self.input.len() {
            self.current = Some(Token::Eof);
            return;
        }
        let ch = self.input[self.pos..].chars().next().unwrap();
        self.current = Some(self.lex_token(ch));
    }

    fn lex_token(&mut self, ch: char) -> Token {
        match ch {
            '(' => self.consume(1, Token::LParen),
            ')' => self.consume(1, Token::RParen),
            '.' => self.consume(1, Token::Dot),
            '!' => self.pair_or_single('=', Token::Ne, Token::Not),
            '<' => self.pair_or_single('=', Token::Le, Token::Lt),
            '>' => self.pair_or_single('=', Token::Ge, Token::Gt),
            '=' => self.pair_required('=', Token::Eq),
            '&' => self.pair_required('&', Token::And),
            '|' => self.pair_required('|', Token::Or),
            '"' | '\'' => self.lex_string(ch),
            c if c.is_ascii_digit() || c == '-' => self.lex_number(c),
            c if c.is_alphabetic() || c == '_' => self.lex_ident(),
            _ => self.set_error(format!(
                "filter: unexpected character '{ch}' at position {}",
                self.pos
            )),
        }
    }

    /// Advance `n` ASCII bytes and return the matching token.
    fn consume(&mut self, n: usize, tok: Token) -> Token {
        self.pos += n;
        tok
    }

    /// Two-character operator (`pair`) with single-character fallback (`single`).
    fn pair_or_single(&mut self, second: char, pair: Token, single: Token) -> Token {
        if self.peek_char() == Some(second) {
            self.consume(2, pair)
        } else {
            self.consume(1, single)
        }
    }

    /// Two-character operator where the single form is invalid — for `==`,
    /// `&&`, `||`, all of which are the same character repeated.
    fn pair_required(&mut self, ch: char, pair: Token) -> Token {
        if self.peek_char() == Some(ch) {
            return self.consume(2, pair);
        }
        self.set_error(format!(
            "filter: unexpected character '{ch}' at position {} (did you mean '{ch}{ch}'?)",
            self.pos
        ))
    }

    fn set_error(&mut self, msg: String) -> Token {
        self.error = Some(anyhow::anyhow!(msg));
        Token::Eof
    }

    fn lex_string(&mut self, quote: char) -> Token {
        self.pos += quote.len_utf8();
        let mut s = String::new();
        while self.pos < self.input.len() {
            let c = self.input[self.pos..].chars().next().unwrap();
            if c == quote {
                self.pos += c.len_utf8();
                return Token::Str(s);
            }
            if c == '\\' {
                self.pos += 1;
                if self.pos >= self.input.len() {
                    break;
                }
                let escaped = self.input[self.pos..].chars().next().unwrap();
                s.push(unescape(escaped));
                self.pos += escaped.len_utf8();
            } else {
                s.push(c);
                self.pos += c.len_utf8();
            }
        }
        self.set_error("filter: unterminated string literal".into())
    }

    fn lex_number(&mut self, first: char) -> Token {
        let start = self.pos;
        if first == '-' {
            self.pos += 1;
        }
        while self.pos < self.input.len() {
            let c = self.input[self.pos..].chars().next().unwrap();
            if c.is_ascii_digit() || c == '.' {
                self.pos += 1;
            } else {
                break;
            }
        }
        let num_str = &self.input[start..self.pos];
        match num_str.parse::<serde_json::Number>() {
            Ok(num) => Token::Number(num),
            Err(_) => self.set_error(format!("filter: invalid number '{num_str}'")),
        }
    }

    fn lex_ident(&mut self) -> Token {
        let start = self.pos;
        while self.pos < self.input.len() {
            let c = self.input[self.pos..].chars().next().unwrap();
            if c.is_alphanumeric() || c == '_' {
                self.pos += c.len_utf8();
            } else {
                break;
            }
        }
        match &self.input[start..self.pos] {
            "true" => Token::Bool(true),
            "false" => Token::Bool(false),
            "null" => Token::Null,
            "exists" => Token::Exists,
            ident => Token::Ident(ident.to_string()),
        }
    }

    fn peek(&self) -> Option<&Token> {
        self.current.as_ref()
    }

    fn next_token(&mut self) -> Option<Token> {
        let t = self.current.take();
        self.advance();
        t
    }

    fn skip_whitespace(&mut self) {
        while self.pos < self.input.len() {
            let c = self.input[self.pos..].chars().next().unwrap();
            if c.is_whitespace() {
                self.pos += c.len_utf8();
            } else {
                break;
            }
        }
    }

    fn peek_char(&self) -> Option<char> {
        self.input[self.pos + 1..].chars().next()
    }
}

fn parse_expr(lexer: &mut Lexer) -> Result<Expr> {
    parse_or(lexer)
}

fn parse_or(lexer: &mut Lexer) -> Result<Expr> {
    let mut left = parse_and(lexer)?;
    while let Some(Token::Or) = lexer.peek() {
        lexer.next_token();
        let right = parse_and(lexer)?;
        left = Expr::Or(Box::new(left), Box::new(right));
    }
    Ok(left)
}

fn parse_and(lexer: &mut Lexer) -> Result<Expr> {
    let mut left = parse_unary(lexer)?;
    while let Some(Token::And) = lexer.peek() {
        lexer.next_token();
        let right = parse_unary(lexer)?;
        left = Expr::And(Box::new(left), Box::new(right));
    }
    Ok(left)
}

fn parse_unary(lexer: &mut Lexer) -> Result<Expr> {
    match lexer.peek() {
        Some(Token::Not) => {
            lexer.next_token();
            let inner = parse_unary(lexer)?;
            Ok(Expr::Not(Box::new(inner)))
        }
        Some(Token::Exists) => {
            lexer.next_token();
            let path = parse_path(lexer)?;
            Ok(Expr::Exists(path))
        }
        _ => parse_comparison(lexer),
    }
}

fn parse_comparison(lexer: &mut Lexer) -> Result<Expr> {
    let left = parse_primary(lexer)?;
    let op = match lexer.peek() {
        Some(Token::Eq) => CompareOp::Eq,
        Some(Token::Ne) => CompareOp::Ne,
        Some(Token::Lt) => CompareOp::Lt,
        Some(Token::Le) => CompareOp::Le,
        Some(Token::Gt) => CompareOp::Gt,
        Some(Token::Ge) => CompareOp::Ge,
        _ => return Ok(left),
    };
    lexer.next_token();
    let right = parse_primary(lexer)?;
    Ok(Expr::Compare {
        left: Box::new(left),
        op,
        right: Box::new(right),
    })
}

fn parse_primary(lexer: &mut Lexer) -> Result<Expr> {
    match lexer.peek().cloned() {
        Some(Token::Bool(b)) => {
            lexer.next_token();
            Ok(Expr::Bool(b))
        }
        Some(Token::Null) => {
            lexer.next_token();
            Ok(Expr::Literal(Value::Null))
        }
        Some(Token::Str(s)) => {
            lexer.next_token();
            Ok(Expr::Literal(Value::String(s)))
        }
        Some(Token::Number(n)) => {
            lexer.next_token();
            Ok(Expr::Literal(Value::Number(n)))
        }
        Some(Token::Ident(_)) => {
            let path = parse_path(lexer)?;
            Ok(Expr::Path(path))
        }
        Some(Token::LParen) => {
            lexer.next_token();
            let inner = parse_expr(lexer)?;
            match lexer.peek() {
                Some(Token::RParen) => {
                    lexer.next_token();
                    Ok(inner)
                }
                _ => bail!("filter: expected ')'"),
            }
        }
        other => bail!("filter: unexpected token {:?}", other),
    }
}

fn parse_path(lexer: &mut Lexer) -> Result<Path> {
    let mut segments = Vec::new();
    while let Some(Token::Ident(name)) = lexer.peek().cloned() {
        lexer.next_token();
        segments.push(name);
        match lexer.peek() {
            Some(Token::Dot) => {
                lexer.next_token();
            }
            _ => break,
        }
    }
    if segments.is_empty() {
        bail!("filter: expected identifier in path");
    }
    Ok(Path { segments })
}

// ------------------------------------------------------------------
// Config + Factory
// ------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct FilterTransformConfig {
    predicate: String,
}

/// Registry factory for [`FilterTransform`]. Registered by
/// `courier::registry::register_builtin` under kind `"filter"`.
pub fn filter_transform_factory(
    id: &str,
    config: Value,
    on_error: ErrorPolicy,
) -> Result<Box<dyn Transform>> {
    let config: FilterTransformConfig = parse_config("filter", config)?;
    let predicate = parse_predicate(&config.predicate)?;
    Ok(Box::new(
        BasicTransform::new(FilterTransform::new(id, predicate)).with_error_policy(on_error),
    ))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::Registry;
    use crate::config::{ErrorPolicyConfig, TransformSpec};
    use crate::envelope::Envelope;

    #[tokio::test]
    async fn keeps_matching_envelope() {
        let predicate = parse_predicate("payload.status == \"ok\"").unwrap();
        let t = FilterTransform::new("t", predicate);
        let env = Envelope::new("src", json!({ "status": "ok" }));
        assert!(t.map(env).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn drops_non_matching_envelope() {
        let predicate = parse_predicate("payload.status == \"ok\"").unwrap();
        let t = FilterTransform::new("t", predicate);
        let env = Envelope::new("src", json!({ "status": "error" }));
        assert!(t.map(env).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn filters_by_numeric_comparison() {
        let predicate = parse_predicate("payload.score >= 0.5").unwrap();
        let t = FilterTransform::new("t", predicate);
        let env = Envelope::new("src", json!({ "score": 0.7 }));
        assert!(t.map(env).await.unwrap().is_some());

        let env = Envelope::new("src", json!({ "score": 0.3 }));
        assert!(t.map(env).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn filters_by_meta_header() {
        let predicate = parse_predicate("meta.headers.priority == \"high\"").unwrap();
        let t = FilterTransform::new("t", predicate);
        let mut env = Envelope::new("src", json!({}));
        env.meta.headers.insert("priority".into(), "high".into());
        assert!(t.map(env).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn logical_and_or() {
        let predicate = parse_predicate("payload.a == 1 && payload.b == 2").unwrap();
        let t = FilterTransform::new("t", predicate);
        let env = Envelope::new("src", json!({ "a": 1, "b": 2 }));
        assert!(t.map(env).await.unwrap().is_some());

        let env = Envelope::new("src", json!({ "a": 1, "b": 3 }));
        assert!(t.map(env).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn logical_not() {
        let predicate = parse_predicate("!exists payload.skip").unwrap();
        let t = FilterTransform::new("t", predicate);
        let env = Envelope::new("src", json!({}));
        assert!(t.map(env).await.unwrap().is_some());

        let env = Envelope::new("src", json!({ "skip": true }));
        assert!(t.map(env).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn null_equality() {
        let predicate = parse_predicate("payload.missing == null").unwrap();
        let t = FilterTransform::new("t", predicate);
        let env = Envelope::new("src", json!({}));
        assert!(t.map(env).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn grouping_parentheses() {
        let predicate =
            parse_predicate("(payload.a == 1 || payload.a == 2) && payload.b == 3").unwrap();
        let t = FilterTransform::new("t", predicate);
        let env = Envelope::new("src", json!({ "a": 2, "b": 3 }));
        assert!(t.map(env).await.unwrap().is_some());

        let env = Envelope::new("src", json!({ "a": 3, "b": 3 }));
        assert!(t.map(env).await.unwrap().is_none());
    }

    #[test]
    fn rejects_invalid_predicate_at_parse_time() {
        let err = parse_predicate("payload.status == ").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("unexpected token") || msg.contains("EOF"),
            "{msg}"
        );
    }

    #[test]
    fn rejects_unknown_root() {
        let predicate = parse_predicate("unknown.field == 1").unwrap();
        let env = Envelope::new("src", json!({}));
        let result = predicate.eval(&env);
        assert!(result.is_err());
    }

    #[test]
    fn factory_resolves_through_registry() {
        let registry = Registry::with_builtins();
        registry
            .build_transform(
                "p/t0",
                TransformSpec {
                    kind: "filter".into(),
                    config: json!({ "predicate": "payload.active == true" }),
                    on_error: Some(ErrorPolicyConfig::Drop),
                },
            )
            .unwrap();
    }

    #[test]
    fn factory_reports_invalid_config() {
        let registry = Registry::with_builtins();
        let err = registry
            .build_transform(
                "p/t0",
                TransformSpec {
                    kind: "filter".into(),
                    config: json!({ "wrong_field": "x" }),
                    on_error: None,
                },
            )
            .err()
            .expect("expected invalid-config error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("invalid config for component type 'filter'"),
            "{msg}",
        );
    }

    #[test]
    fn factory_rejects_malformed_predicate() {
        let registry = Registry::with_builtins();
        let err = registry
            .build_transform(
                "p/t0",
                TransformSpec {
                    kind: "filter".into(),
                    config: json!({ "predicate": "payload.status == " }),
                    on_error: None,
                },
            )
            .err()
            .expect("expected predicate parse error");
        let msg = format!("{err:#}");
        assert!(msg.contains("filter"), "{msg}");
    }

    #[test]
    fn rejects_single_equals() {
        let err = parse_predicate("payload.status = \"ok\"").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("'='") && msg.contains("'=='"), "{msg}");
    }

    #[test]
    fn rejects_single_ampersand() {
        let err = parse_predicate("payload.a & payload.b").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("'&'") && msg.contains("'&&'"), "{msg}");
    }

    #[test]
    fn rejects_single_pipe() {
        let err = parse_predicate("payload.a | payload.b").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("'|'") && msg.contains("'||'"), "{msg}");
    }

    #[tokio::test]
    async fn relational_compare_with_missing_field_returns_false() {
        let predicate = parse_predicate("payload.score >= 0.5").unwrap();
        let t = FilterTransform::new("t", predicate);
        // Missing field → Null → should NOT match (was previously "null" >= "0.5" = true)
        let env = Envelope::new("src", json!({}));
        assert!(t.map(env).await.unwrap().is_none());

        // Present but wrong type (string vs number) → should NOT match
        let env = Envelope::new("src", json!({ "score": "high" }));
        assert!(t.map(env).await.unwrap().is_none());
    }

    #[test]
    fn rejects_unterminated_string_literal() {
        let err = parse_predicate("payload.status == \"ok").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("unterminated string"), "{msg}");
    }

    #[tokio::test]
    async fn parses_escaped_utf8_string_literal() {
        let predicate = parse_predicate("payload.status == \"\\é\"").unwrap();
        let t = FilterTransform::new("t", predicate);
        let env = Envelope::new("src", json!({ "status": "é" }));
        assert!(t.map(env).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn parses_utf8_identifier_segments() {
        let predicate = parse_predicate("payload.café == 1").unwrap();
        let t = FilterTransform::new("t", predicate);
        let env = Envelope::new("src", json!({ "café": 1 }));
        assert!(t.map(env).await.unwrap().is_some());
    }

    #[test]
    fn skips_utf8_whitespace() {
        parse_predicate("payload.status\u{00a0}==\u{00a0}\"ok\"").unwrap();
    }

    #[test]
    fn compare_eq_ne_cover_equal_and_unequal_values() {
        assert!(compare_values(&json!(1), CompareOp::Eq, &json!(1)));
        assert!(!compare_values(&json!(1), CompareOp::Eq, &json!(2)));
        assert!(compare_values(&json!("a"), CompareOp::Ne, &json!("b")));
        assert!(!compare_values(&json!("a"), CompareOp::Ne, &json!("a")));
    }

    #[test]
    fn compare_relational_orders_numbers() {
        assert!(compare_values(&json!(1), CompareOp::Lt, &json!(2)));
        assert!(compare_values(&json!(2), CompareOp::Le, &json!(2)));
        assert!(compare_values(&json!(3), CompareOp::Gt, &json!(2)));
        assert!(compare_values(&json!(2), CompareOp::Ge, &json!(2)));
        assert!(!compare_values(&json!(2), CompareOp::Lt, &json!(1)));
    }

    #[test]
    fn compare_relational_orders_strings_lexicographically() {
        assert!(compare_values(&json!("a"), CompareOp::Lt, &json!("b")));
        assert!(compare_values(&json!("a"), CompareOp::Le, &json!("a")));
        assert!(compare_values(&json!("b"), CompareOp::Gt, &json!("a")));
        assert!(compare_values(&json!("a"), CompareOp::Ge, &json!("a")));
    }

    #[test]
    fn compare_relational_returns_false_for_mixed_or_null() {
        // Number vs string: not comparable.
        assert!(!compare_values(&json!(1), CompareOp::Lt, &json!("1")));
        // Null on either side: not comparable.
        assert!(!compare_values(&Value::Null, CompareOp::Gt, &json!(1)));
        assert!(!compare_values(&json!(1), CompareOp::Ge, &Value::Null));
        // Two nulls / objects: not comparable.
        assert!(!compare_values(
            &json!({"k": 1}),
            CompareOp::Lt,
            &json!({"k": 2})
        ));
    }

    #[test]
    fn truthy_covers_every_value_kind() {
        assert!(!truthy(&Value::Null));
        assert!(truthy(&json!(true)));
        assert!(!truthy(&json!(false)));
        assert!(truthy(&json!(1)));
        assert!(!truthy(&json!(0)));
        assert!(truthy(&json!("x")));
        assert!(!truthy(&json!("")));
        assert!(truthy(&json!([1])));
        assert!(!truthy(&json!([])));
        assert!(truthy(&json!({"k": 1})));
        assert!(!truthy(&json!({})));
    }
}
