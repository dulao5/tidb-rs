//! TiDB SQL parser, mechanically ported from TiDB's `pkg/parser/parser.y`.
//!
//! ```
//! use tidb_parser::{ParseOptions, parse};
//!
//! let out = parse("SELECT a FROM t WHERE b > 1; SELECT 2", &ParseOptions::default()).unwrap();
//! assert_eq!(out.stmts.len(), 2);
//! ```
//!
//! Architecture (see `README.md` for details and porting status):
//!
//! * `grammar/tidb_typed.y` is generated from TiDB's `parser.y` by `tools/parser-port`; every
//!   production has a typed Rust action. Only the SELECT core is hand-ported so far; other
//!   productions build placeholder nodes (`Node`), so their AST is not usable yet.
//! * `build.rs` computes the LR(1) automaton with cfgrammar/lrtable at build time and emits
//!   comb-packed tables plus one reducer function per production.
//! * At runtime a hand-written lexer feeds a small table-driven LR driver; no parser generator
//!   runtime is linked.
//!
//! The AST (`ast`) mirrors the Go `ast` package names and is not stable yet.
#![warn(missing_docs)]

mod actions_rt;
#[allow(missing_docs)]
pub mod ast;
mod ctx;
mod driver;
/// Stub types for Go AST types the grammar references but which are not ported yet.
#[allow(missing_docs)]
#[rustfmt::skip]
pub mod gen_types;
// `ALIASES` (misc.go `aliases`) is generated but not needed by the lexer yet.
#[allow(dead_code)]
#[rustfmt::skip]
mod keywords_gen;
mod lexer;
mod span;

/// Generated value enum, reducers and production metadata (`build/codegen.rs`).
// The action code is machine-generated from grammar/tidb_typed.y (hand-ported actions are
// verbatim Go translations), so style lints are not meaningful here.
#[allow(
    clippy::all,
    unused_parens,
    unused_braces,
    non_camel_case_types,
    missing_docs,
    rust_2018_idioms
)]
mod actions {
    include!(concat!(env!("OUT_DIR"), "/actions.rs"));
}
mod token_map {
    include!(concat!(env!("OUT_DIR"), "/token_map.rs"));
}

use std::{cell::RefCell, fmt, sync::OnceLock};

pub use actions_rt::Node;

/// Statistics of the generated LR tables (states, table sizes, reducer count; informational).
pub const TABLE_STATS: &str = token_map::BUILD_REPORT;
pub use ctx::Diagnostic;
pub use span::Span;

use ast::Stmt;
use lexer::{Lexer, TokenTable};

/// Parser settings (the subset of goyacc `Parser` configuration supported so far).
#[derive(Debug, Clone)]
pub struct ParseOptions {
    /// Treat window function names (`ROW_NUMBER`, `RANK`, ...) as keywords. TiDB's
    /// `parser.New()` enables this; default `true`.
    pub window_functions: bool,
}

impl Default for ParseOptions {
    fn default() -> Self {
        ParseOptions {
            window_functions: true,
        }
    }
}

/// Result of a successful [`parse`].
#[derive(Debug)]
pub struct ParseOutput {
    /// The statements, in source order (empty statements between `;` are skipped).
    pub stmts: Vec<Stmt>,
    /// Warnings reported by semantic actions (goyacc `AppendWarn`).
    pub warnings: Vec<Diagnostic>,
}

/// Why the input was rejected. Positions are byte offsets into the SQL text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// The lexer could not tokenize the input (unterminated string/comment, stray byte, ...).
    Lex {
        /// Byte offset of the error.
        pos: usize,
        /// Description.
        msg: String,
    },
    /// No grammar action for a token.
    Syntax {
        /// Byte offset of the offending token (`sql.len()` at end of input).
        pos: usize,
        /// Text of the offending token (empty at end of input).
        near: String,
    },
    /// The grammar accepted the input but a semantic action rejected it (goyacc
    /// `AppendError`, e.g. `SELECT ALL DISTINCT`). Only the first error is reported.
    Semantic(Diagnostic),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::Lex { pos, msg } => write!(f, "lexer error at byte {pos}: {msg}"),
            ParseError::Syntax { pos, near } if near.is_empty() => {
                write!(f, "syntax error at byte {pos}: unexpected end of input")
            }
            ParseError::Syntax { pos, near } => {
                write!(f, "syntax error at byte {pos} near \"{near}\"")
            }
            ParseError::Semantic(d) => write!(f, "{} (at byte {})", d.msg, d.pos),
        }
    }
}

impl std::error::Error for ParseError {}

fn token_table() -> &'static TokenTable {
    static TT: OnceLock<TokenTable> = OnceLock::new();
    TT.get_or_init(TokenTable::new)
}

/// A parser instance that reuses its stacks across calls. [`parse`] uses a thread-local one;
/// use this type directly to control allocation reuse explicitly.
#[derive(Default)]
pub struct Parser {
    driver: driver::Driver,
}

impl Parser {
    /// Create a parser with empty stacks.
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse `sql` (one or more `;`-separated statements). See [`parse`].
    pub fn parse(&mut self, sql: &str, opts: &ParseOptions) -> Result<ParseOutput, ParseError> {
        let tt = token_table();
        let toks = Lexer::new(tt, sql, opts.window_functions)
            .tokenize()
            .map_err(|e| ParseError::Lex {
                pos: e.pos,
                msg: e.msg,
            })?;
        let mut ctx = ctx::Ctx::new();
        ctx.window_funcs = opts.window_functions;
        match self.driver.parse(sql, &toks, &ctx) {
            driver::Outcome::Ok(stmts) => {
                // goyacc: any AppendError fails the whole parse, even without `return 1`.
                if let Some(e) = ctx.errors.borrow_mut().drain(..).next() {
                    return Err(ParseError::Semantic(e));
                }
                Ok(ParseOutput {
                    stmts,
                    warnings: ctx.warnings.take(),
                })
            }
            driver::Outcome::Aborted => {
                let e = ctx.errors.borrow_mut().drain(..).next();
                Err(ParseError::Semantic(
                    e.expect("aborting action records an error"),
                ))
            }
            driver::Outcome::Syntax { tok_index } => Err(match toks.get(tok_index) {
                Some(t) => ParseError::Syntax {
                    pos: t.start,
                    near: sql[t.start..t.end].to_string(),
                },
                None => ParseError::Syntax {
                    pos: sql.len(),
                    near: String::new(),
                },
            }),
        }
    }
}

/// Parse `sql`, which may hold several `;`-separated statements, into statements and
/// warnings.
///
/// Errors are reported like TiDB's `Parser.Parse`: lexer errors, the first syntax error
/// (no recovery), or the first error appended by a semantic action.
pub fn parse(sql: &str, opts: &ParseOptions) -> Result<ParseOutput, ParseError> {
    thread_local! {
        static PARSER: RefCell<Parser> = RefCell::new(Parser::new());
    }
    PARSER.with(|p| match p.try_borrow_mut() {
        Ok(mut p) => p.parse(sql, opts),
        // Re-entrant call (cannot happen from actions today): use a fresh parser.
        Err(_) => Parser::new().parse(sql, opts),
    })
}
