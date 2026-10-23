//! Parser context passed to every action (`%parse-param p: &'input Ctx`).
//!
//! Replaces goyacc's `parser.*` fields (charset/collation/mode flags) and the
//! `yylex.AppendError` / `AppendWarn` side channels.
use std::cell::{Cell, RefCell};

/// An error or warning reported by a semantic action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// Message, worded like TiDB's.
    pub msg: String,
    /// Byte offset into the SQL text.
    pub pos: usize,
}

// Fields and methods not used by the hand-ported actions yet are kept: they are the targets of
// goyacc's `parser.charset`/`parser.collation`, `SetMariaDB` and `AppendWarn` in the actions
// still to be ported.
#[derive(Default)]
#[allow(dead_code)]
pub struct Ctx {
    pub charset: &'static str,
    pub collation: &'static str,
    /// Parser-mode flags (goyacc: `parser.lexer.GetSQLMode()`, `EnableWindowFunc`, `SetMariaDB`).
    pub mariadb: bool,
    pub window_funcs: bool,
    /// `yylex.AppendError(e)` lands here.
    pub errors: RefCell<Vec<Diagnostic>>,
    /// `parser.lastErrorAsWarn()` / `AppendWarn` lands here.
    pub warnings: RefCell<Vec<Diagnostic>>,
    /// goyacc `parser.explicitCharset`: set by `CastType` (CHAR ... CHARSET x), consumed and
    /// reset by the enclosing CAST/CONVERT action.
    pub explicit_charset: Cell<bool>,
    /// Set by an action that did `return 1` in goyacc. The driver checks it after every
    /// reduction and stops immediately, like goyacc.
    pub abort: Cell<bool>,
}

impl Ctx {
    pub fn new() -> Self {
        Ctx {
            charset: "utf8mb4",
            collation: "utf8mb4_bin",
            ..Default::default()
        }
    }
    /// goyacc `yylex.AppendError(err); return 1`
    pub fn fail(&self, msg: impl Into<String>, pos: usize) {
        self.errors.borrow_mut().push(Diagnostic {
            msg: msg.into(),
            pos,
        });
        self.abort.set(true);
    }
    /// goyacc `yylex.AppendError(err)` without `return 1`: the parse continues but fails.
    pub fn error(&self, msg: impl Into<String>, pos: usize) {
        self.errors.borrow_mut().push(Diagnostic {
            msg: msg.into(),
            pos,
        });
    }
    #[allow(dead_code)]
    pub fn warn(&self, msg: impl Into<String>, pos: usize) {
        self.warnings.borrow_mut().push(Diagnostic {
            msg: msg.into(),
            pos,
        });
    }
}
