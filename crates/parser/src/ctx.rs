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
    /// The MySQL error code of the TiDB error the Go action appends (for example
    /// `ErrUnknownCharacterSet`, 1115), which Go's `util.SyntaxError` passes to the client
    /// unchanged. `0` for a plain error, which `util.SyntaxError` wraps as `ErrParse` (1064).
    pub code: u16,
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
    pub sql_mode: tidb_base::mysql::SQLMode,
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
    /// How many parameter markers (`?`) the actions built, over the whole input. No Go
    /// counterpart: PREPARE compares it with the markers its AST walk finds (#356).
    pub param_markers: Cell<usize>,
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
        self.fail_code(0, msg, pos);
    }
    /// goyacc `yylex.AppendError(terr.GenWithStackByArgs(..)); return 1` for a TiDB error of
    /// MySQL code `code` (see [`Diagnostic::code`]).
    pub fn fail_code(&self, code: u16, msg: impl Into<String>, pos: usize) {
        self.errors.borrow_mut().push(Diagnostic {
            msg: msg.into(),
            pos,
            code,
        });
        self.abort.set(true);
    }
    /// goyacc `yylex.AppendError(err)` without `return 1`: the parse continues but fails.
    pub fn error(&self, msg: impl Into<String>, pos: usize) {
        self.errors.borrow_mut().push(Diagnostic {
            msg: msg.into(),
            pos,
            code: 0,
        });
    }
    #[allow(dead_code)]
    pub fn warn(&self, msg: impl Into<String>, pos: usize) {
        self.warnings.borrow_mut().push(Diagnostic {
            msg: msg.into(),
            pos,
            code: 0,
        });
    }
}

/// The message of the TiDB error `code` with `args`, as `GenWithStackByArgs` formats it: the
/// format of `MySQLErrName` (`Unknown character set: '%-.64s'`).
// glue: the message part of terror ErrClass.NewStd(code).GenWithStackByArgs(args) in a grammar action
pub fn tidb_message(code: u16, args: Vec<tidb_base::gofmt::Arg>) -> String {
    String::from_utf8_lossy(&tidb_base::mysql::new_err(code, args).message).into_owned()
}

/// The message of `GenWithStack(format, args)`: `fmt.Sprintf`.
// glue: the message part of terror Error.GenWithStack(format, args) in a grammar action
pub fn go_sprintf(format: &str, args: Vec<tidb_base::gofmt::Arg>) -> String {
    String::from_utf8_lossy(&tidb_base::gofmt::sprintf(format, &args)).into_owned()
}
