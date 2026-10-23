//! Runtime support used by the generated action code (`grammar/tidb_typed.y`).
use std::any::Any;

use crate::span::Span;

/// Catch-all value type for nonterminals whose Go type could not be inferred.
pub type Item = Box<dyn Any>;
/// The value of a terminal in an action (`$n` for a token): its id and source range.
pub type Tok = crate::lexer::Tok;

/// Generic placeholder node built by every not-yet-ported action.
pub struct Node {
    /// `"<Rule>#<alternative index>"` of the production in `grammar/tidb_typed.y`.
    pub kind: &'static str,
    /// Source range of the production.
    pub span: Span,
    /// Values of the production's nonterminal children (type-erased).
    pub children: Vec<Box<dyn Any>>,
}

impl std::fmt::Debug for Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Node({} @{}..{}, {} children)",
            self.kind,
            self.span.start(),
            self.span.end(),
            self.children.len()
        )
    }
}

/// Every nonterminal value type can be built from a placeholder node.
pub trait FromNode {
    fn from_node(n: Node) -> Self;
}

#[inline]
pub fn ph<T: FromNode>(kind: &'static str, span: Span, children: Vec<Box<dyn Any>>) -> T {
    T::from_node(Node {
        kind,
        span,
        children,
    })
}

impl<T: FromNode> FromNode for Box<T> {
    fn from_node(n: Node) -> Self {
        Box::new(T::from_node(n))
    }
}
impl<T: FromNode> FromNode for Option<T> {
    fn from_node(n: Node) -> Self {
        Some(T::from_node(n))
    }
}
/// A not-yet-ported list action yields a one-element list holding the placeholder, so the
/// information is never silently dropped (an empty Vec would look like "no items").
impl<T: FromNode> FromNode for Vec<T> {
    fn from_node(n: Node) -> Self {
        vec![T::from_node(n)]
    }
}
impl FromNode for Item {
    fn from_node(n: Node) -> Self {
        Box::new(n)
    }
}
macro_rules! default_from_node {
    ($($t:ty),*) => { $(impl FromNode for $t { fn from_node(_: Node) -> Self { Default::default() } })* };
}
default_from_node!(bool, i64, i32, u64, u32, u16, u8, f64, char, String, ());

/// Source-text access used by actions (`$lexer.src_str(span)`).
pub trait SrcText {
    fn src_str(&self, span: Span) -> &str;
}

/// Token text as goyacc's `yylval.ident` would hold it: quotes stripped / unescaped.
pub fn tok_text<L: SrcText + ?Sized>(l: &L, t: &Tok) -> String {
    unquote_text(l.src_str(tok_span(t)))
}

/// The lexer value of an identifier / string token text: back-quoted names and quoted strings
/// are unquoted (and unescaped), anything else is returned as is.
pub fn unquote_text(s: &str) -> String {
    let b = s.as_bytes();
    if b.len() >= 2 && b[0] == b'`' {
        return s[1..s.len() - 1].replace("``", "`");
    }
    if b.len() >= 2 && (b[0] == b'\'' || b[0] == b'"') {
        return unquote_string(s);
    }
    s.to_string()
}

/// MySQL string literal unescaping (the subset needed by the ported actions so far).
pub fn unquote_string(s: &str) -> String {
    let q = s.as_bytes()[0] as char;
    let body = &s[1..s.len() - 1];
    let mut out = String::with_capacity(body.len());
    let mut it = body.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('0') => out.push('\0'),
                Some('b') => out.push('\u{8}'),
                Some('Z') => out.push('\u{1a}'),
                Some('%') => out.push_str("\\%"),
                Some('_') => out.push_str("\\_"),
                Some(o) => out.push(o),
                None => {}
            }
        } else if c == q && it.peek() == Some(&q) {
            it.next();
            out.push(q);
        } else {
            out.push(c);
        }
    }
    out
}

pub fn tok_span(t: &Tok) -> Span {
    Span::new(t.start, t.end)
}
