//! The error type of tidb-rs: `github.com/pingcap/errors.Error` (`normalize.go`) and the
//! parts of `pingcap/errors` that TiDB relies on (mapping rule `M-ERR`).
//!
//! go-file: github.com/pingcap/errors@v0.11.5-0.20250523034308-74f78ae071ee/normalize.go
//! go-file: github.com/pingcap/errors@v0.11.5-0.20250523034308-74f78ae071ee/errors.go
//!
//! Go TiDB returns the `error` interface everywhere; almost all values are `*errors.Error`
//! (`terror.Error` is an alias), created from a package-level prototype with
//! `GenWithStackByArgs`, and wrapped on the way up with `errors.Trace` / `errors.Annotate`
//! (which add a stack and are transparent to `Error()`, `Cause` and `Equal`).
//!
//! `M-ERR` (block D00.2 spec): the Rust carrier is this one concrete type, [`Error`]:
//! the error code, RFC code, message template and arguments, an optional cause, and the
//! creation site (file/line of `#[track_caller]`, plus a `Backtrace` that is only captured
//! when `RUST_BACKTRACE` is set). `errors.Trace`, `Annotate`, `AddStack`, `WithStack` are
//! not ported: the error is returned as is, and `Cause(err)` is `err`. A plain Go error
//! (`errors.New`, `errors.Errorf`, `fmt.Errorf`, errors of the standard library) is an
//! `Error` without code ([`Error::plain`]); its `Error()` is the message alone.
//!
//! Go strings are byte strings: the message template, `Error()` and `GetMsg()` are bytes
//! (`Vec<u8>`, as `M-NAME`'s `to_string() -> Vec<u8>`); arguments are [`Arg`]s over bytes.
//! `Display` (for logs) converts lossily.

#![allow(non_upper_case_globals)]

use std::backtrace::Backtrace;
use std::borrow::Cow;
use std::fmt;
use std::panic::Location;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use crate::gofmt::{self, Arg};

/// `ErrCode` represents a specific error type in a error class.
/// Same error code can be used in different error classes.
pub type ErrCode = isize;

/// Go converts untyped error-code constants to `ErrCode` implicitly; the port's codes are
/// `u16` constants (`mysql::ErrDupEntry`), so constructors take `impl IntoErrCode`.
pub trait IntoErrCode {
    /// `ErrCode(x)`.
    // glue: Go conversion ErrCode(x) (M-INT)
    fn into_err_code(self) -> ErrCode;
}
macro_rules! into_err_code {
    ($($t:ty),*) => {$(
        impl IntoErrCode for $t {
            // glue: Go conversion ErrCode(x) (M-INT)
            fn into_err_code(self) -> ErrCode {
                self as ErrCode
            }
        }
    )*};
}
into_err_code!(u8, u16, u32, i16, i32, i64, isize);

/// `RedactLogEnabled` values.
pub const RedactLogEnable: &str = "ON";
/// `RedactLogEnabled` values.
pub const RedactLogDisable: &str = "OFF";
/// `RedactLogEnabled` values.
pub const RedactLogMarker: &str = "MARKER";

/// `RedactLogEnabled` defines whether the arguments of Error need to be redacted
/// (Go: an `atomic.String`; only "ON" and "MARKER" have an effect, anything else is "OFF").
static REDACT_LOG_ENABLED: AtomicU8 = AtomicU8::new(0);

/// `RedactLogEnabled.Store(v)`.
// glue: Go atomic.String RedactLogEnabled.Store (values other than ON/MARKER behave as OFF)
pub fn redact_log_enabled_store(v: &str) {
    let n = match v {
        RedactLogEnable => 1,
        RedactLogMarker => 2,
        _ => 0,
    };
    REDACT_LOG_ENABLED.store(n, Ordering::Relaxed);
}

/// `RedactLogEnabled.Load()`.
// glue: Go atomic.String RedactLogEnabled.Load
pub fn redact_log_enabled_load() -> &'static str {
    match REDACT_LOG_ENABLED.load(Ordering::Relaxed) {
        1 => RedactLogEnable,
        2 => RedactLogMarker,
        _ => RedactLogDisable,
    }
}

/// Whether the error came from `Normalize` (a `*errors.Error`) or is a plain Go error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// `*errors.Error`.
    Normalized,
    /// Any other Go `error` (`errors.New`, `fmt.Errorf`, standard library errors).
    Plain,
}

/// `Error` is the 'prototype' of a type of errors (pingcap/errors `Error`).
///
/// Prototypes are package-level `LazyLock<Error>` statics (Go: package-level vars
/// initialised by `Normalize` / `ErrClass.NewStd`); `gen_with_stack_by_args` & co. return a
/// copy with arguments and creation site.
#[derive(Clone)]
pub struct Error(Box<Inner>);

/// The fields of [`Error`] (boxed: `Result<T, Error>` stays small). Not part of the API:
/// the fields are private, use the methods of [`Error`].
#[doc(hidden)]
#[derive(Clone)]
pub struct Inner {
    kind: Kind,
    code: ErrCode,
    /// codeText is the textual describe of the error code
    code_text: Cow<'static, str>,
    /// message is a template of the description of this error.
    /// printf-style formatting is enabled.
    message: Cow<'static, [u8]>,
    /// redactArgsPos defines the positions of arguments in message that need to be redacted.
    /// And it is controlled by the global var RedactLogEnabled.
    redact_args_pos: Cow<'static, [usize]>,
    /// Cause is used to warp some third party error.
    cause: Option<Arc<Error>>,
    args: Vec<Arg>,
    file: &'static str,
    line: i32,
    /// `AddStack`: captured only if `RUST_BACKTRACE` is set (`M-ERR`).
    stack: Option<Arc<Backtrace>>,
}

impl std::ops::Deref for Error {
    type Target = Inner;
    // glue: field access through the box
    fn deref(&self) -> &Inner {
        &self.0
    }
}

impl std::ops::DerefMut for Error {
    // glue: field access through the box
    fn deref_mut(&mut self) -> &mut Inner {
        &mut self.0
    }
}

impl fmt::Debug for Error {
    // glue: Debug for tests and logs: the Go Error() text
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&String::from_utf8_lossy(&self.error()))
    }
}

impl fmt::Display for Error {
    // glue: Display delegates to Error() (M-NAME), lossy for non-UTF-8 bytes (logs only)
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&String::from_utf8_lossy(&self.error()))
    }
}

impl std::error::Error for Error {
    // glue: the cause (Unwrap) for Rust error chains
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.cause
            .as_deref()
            .map(|e| e as &(dyn std::error::Error + 'static))
    }
}

/// A Go `string` value for the error API: string literals, `String`, bytes.
pub trait IntoGoString {
    /// The bytes of the Go string.
    // glue: Go string conversion
    fn into_go_string(self) -> Cow<'static, [u8]>;
}
impl IntoGoString for &'static str {
    // glue: Go string from a literal
    fn into_go_string(self) -> Cow<'static, [u8]> {
        Cow::Borrowed(self.as_bytes())
    }
}
impl IntoGoString for String {
    // glue: Go string from a Rust string
    fn into_go_string(self) -> Cow<'static, [u8]> {
        Cow::Owned(self.into_bytes())
    }
}
impl IntoGoString for &'static [u8] {
    // glue: Go string from bytes
    fn into_go_string(self) -> Cow<'static, [u8]> {
        Cow::Borrowed(self)
    }
}
impl IntoGoString for Vec<u8> {
    // glue: Go string from bytes
    fn into_go_string(self) -> Cow<'static, [u8]> {
        Cow::Owned(self)
    }
}
impl IntoGoString for Cow<'static, [u8]> {
    // glue: Go string from bytes
    fn into_go_string(self) -> Cow<'static, [u8]> {
        self
    }
}

/// `NormalizeOption`.
pub enum NormalizeOption {
    /// `RedactArgs(pos)`.
    RedactArgs(Cow<'static, [usize]>),
    /// `RFCCodeText(codeText)`.
    RFCCodeText(Cow<'static, str>),
    /// `MySQLErrorCode(code)`.
    MySQLErrorCode(ErrCode),
}

/// `RedactArgs` returns a NormalizeOption to set the arguments to redact.
// go-dep: github.com/pingcap/errors@v0.11.5-0.20250523034308-74f78ae071ee.RedactArgs L1
pub fn redact_args(pos: impl Into<Cow<'static, [usize]>>) -> NormalizeOption {
    NormalizeOption::RedactArgs(pos.into())
}

/// `RFCCodeText` returns a NormalizeOption to set RFC error code.
// go-dep: github.com/pingcap/errors@v0.11.5-0.20250523034308-74f78ae071ee.RFCCodeText L1
pub fn rfc_code_text(code_text: impl Into<Cow<'static, str>>) -> NormalizeOption {
    NormalizeOption::RFCCodeText(code_text.into())
}

/// `MySQLErrorCode` returns a NormalizeOption to set error code.
// go-dep: github.com/pingcap/errors@v0.11.5-0.20250523034308-74f78ae071ee.MySQLErrorCode L1
pub fn mysql_error_code(code: ErrCode) -> NormalizeOption {
    NormalizeOption::MySQLErrorCode(code)
}

/// `Normalize` creates a new Error object.
// go-dep: github.com/pingcap/errors@v0.11.5-0.20250523034308-74f78ae071ee.Normalize L1
pub fn normalize(
    message: impl IntoGoString,
    opts: impl IntoIterator<Item = NormalizeOption>,
) -> Error {
    let mut e = Error::empty(Kind::Normalized, message.into_go_string());
    for opt in opts {
        match opt {
            NormalizeOption::RedactArgs(pos) => e.redact_args_pos = pos,
            NormalizeOption::RFCCodeText(t) => e.code_text = t,
            NormalizeOption::MySQLErrorCode(c) => e.code = c,
        }
    }
    e
}

/// `errors.New` (pingcap/errors, like the standard library's): a plain error with a stack.
// go-dep: github.com/pingcap/errors@v0.11.5-0.20250523034308-74f78ae071ee.New L1 [M-ERR]
#[track_caller]
pub fn new(message: impl IntoGoString) -> Error {
    Error::plain(message)
}

/// `errors.Errorf`: a plain error with a formatted message.
// go-dep: github.com/pingcap/errors@v0.11.5-0.20250523034308-74f78ae071ee.Errorf L1 [M-ERR]
#[track_caller]
pub fn errorf(format: impl AsRef<[u8]>, args: &[Arg]) -> Error {
    Error::plain(gofmt::sprintf(format, args))
}

/// `errors.Cause`: the error without its `Trace`/`Annotate` wrappers. Wrappers are not
/// ported (`M-ERR`), so this is the error itself.
// go-dep: github.com/pingcap/errors@v0.11.5-0.20250523034308-74f78ae071ee.Cause L1 [M-ERR]
pub fn cause(err: &Error) -> &Error {
    err
}

/// `RedactErrorArg` redacts the args by position if RedactLogEnabled is enabled.
// go-dep: github.com/pingcap/errors@v0.11.5-0.20250523034308-74f78ae071ee.RedactErrorArg L1
pub fn redact_error_arg(args: &mut [Arg], position: &[usize]) {
    match redact_log_enabled_load() {
        RedactLogEnable => {
            for &pos in position {
                if args.len() > pos {
                    args[pos] = Arg::from("?");
                }
            }
        }
        RedactLogMarker => {
            for &pos in position {
                if args.len() > pos {
                    let a = std::mem::replace(&mut args[pos], Arg::Nil);
                    args[pos] = Arg::Redact(Box::new(a));
                }
            }
        }
        _ => {}
    }
}

/// `ErrorEqual` returns a boolean indicating whether err1 is equal to err2.
// go-dep: github.com/pingcap/errors@v0.11.5-0.20250523034308-74f78ae071ee.ErrorEqual L1
pub fn error_equal(err1: Option<&Error>, err2: Option<&Error>) -> bool {
    let (e1, e2) = match (err1.map(cause), err2.map(cause)) {
        (None, None) => return true,
        (Some(a), Some(b)) => (a, b),
        _ => return false,
    };
    if e1.same_instance(e2) {
        return true;
    }
    if e1.kind == Kind::Normalized && e2.kind == Kind::Normalized {
        return e1.equal(e2);
    }
    e1.error() == e2.error()
}

/// `ErrorNotEqual` returns a boolean indicating whether err1 isn't equal to err2.
// go-dep: github.com/pingcap/errors@v0.11.5-0.20250523034308-74f78ae071ee.ErrorNotEqual L1
pub fn error_not_equal(err1: Option<&Error>, err2: Option<&Error>) -> bool {
    !error_equal(err1, err2)
}

impl Error {
    // glue: field initialisation shared by the constructors
    fn empty(kind: Kind, message: Cow<'static, [u8]>) -> Error {
        Error(Box::new(Inner {
            kind,
            code: 0,
            code_text: Cow::Borrowed(""),
            message,
            redact_args_pos: Cow::Borrowed(&[]),
            cause: None,
            args: Vec::new(),
            file: "",
            line: 0,
            stack: None,
        }))
    }

    /// A plain Go `error` (not a `*errors.Error`): `Error()` is `message`.
    // glue: the Rust carrier of non-terror Go errors (M-ERR)
    #[track_caller]
    pub fn plain(message: impl IntoGoString) -> Error {
        let mut e = Error::empty(Kind::Plain, message.into_go_string());
        e.fill_line_and_file();
        e
    }

    /// Whether this is a `*errors.Error` (`err.(*terror.Error)` succeeds).
    // glue: Go type assertion err.(*errors.Error)
    pub fn is_normalized(&self) -> bool {
        self.kind == Kind::Normalized
    }

    /// Pointer identity of Go sentinels: an error and its clones without new arguments or
    /// site. Go compares `cause == sentinel` before comparing IDs.
    // glue: Go interface equality (pointer identity) approximated by value identity
    fn same_instance(&self, other: &Error) -> bool {
        self.kind == other.kind
            && self.code == other.code
            && self.code_text == other.code_text
            && self.message == other.message
            && self.args == other.args
            && self.file == other.file
            && self.line == other.line
    }

    /// `Code` returns the numeric code of this error.
    // go-dep: github.com/pingcap/errors@v0.11.5-0.20250523034308-74f78ae071ee.Error.Code L1
    pub fn code(&self) -> ErrCode {
        self.code
    }

    /// `RFCCode`, by the RFC: `{Component}:{ErrorClass}:{InnerErrorCode}`.
    // go-dep: github.com/pingcap/errors@v0.11.5-0.20250523034308-74f78ae071ee.Error.RFCCode L1
    pub fn rfc_code(&self) -> Cow<'_, str> {
        self.id()
    }

    /// `ID` returns the ID of this error.
    // go-dep: github.com/pingcap/errors@v0.11.5-0.20250523034308-74f78ae071ee.Error.ID L1
    pub fn id(&self) -> Cow<'_, str> {
        if !self.code_text.is_empty() {
            return Cow::Borrowed(&self.code_text);
        }
        Cow::Owned(self.code.to_string())
    }

    /// `Location` returns the location where the error is created.
    // go-dep: github.com/pingcap/errors@v0.11.5-0.20250523034308-74f78ae071ee.Error.Location L1
    pub fn location(&self) -> (&'static str, i32) {
        (self.file, self.line)
    }

    /// `MessageTemplate` returns the error message template of this error.
    // go-dep: github.com/pingcap/errors@v0.11.5-0.20250523034308-74f78ae071ee.Error.MessageTemplate L1
    pub fn message_template(&self) -> &[u8] {
        &self.message
    }

    /// `Args` returns the message arguments of this error.
    // go-dep: github.com/pingcap/errors@v0.11.5-0.20250523034308-74f78ae071ee.Error.Args L1
    pub fn args(&self) -> &[Arg] {
        &self.args
    }

    /// The redaction positions given by `RedactArgs`.
    // glue: read access to the unexported redactArgsPos (govec checks it)
    pub fn redact_args_pos(&self) -> &[usize] {
        &self.redact_args_pos
    }

    /// The stack captured at creation (`AddStack`), if `RUST_BACKTRACE` was set.
    // glue: M-ERR optional creation-site stack
    pub fn stack(&self) -> Option<&Backtrace> {
        self.stack.as_deref()
    }

    /// `Error` implements error interface (`M-NAME`: also `Display`).
    // go-dep: github.com/pingcap/errors@v0.11.5-0.20250523034308-74f78ae071ee.Error.Error L1
    pub fn error(&self) -> Vec<u8> {
        if self.kind == Kind::Plain {
            return self.get_msg();
        }
        let mut out = Vec::new();
        out.push(b'[');
        out.extend_from_slice(self.rfc_code().as_bytes());
        out.push(b']');
        out.extend_from_slice(&self.get_msg());
        if let Some(c) = &self.cause {
            out.extend_from_slice(b": ");
            out.extend_from_slice(&c.error());
        }
        out
    }

    /// `GetMsg`: the message template formatted with the arguments.
    // go-dep: github.com/pingcap/errors@v0.11.5-0.20250523034308-74f78ae071ee.Error.GetMsg L1
    pub fn get_msg(&self) -> Vec<u8> {
        if !self.args.is_empty() {
            return gofmt::sprintf(&self.message, &self.args);
        }
        self.message.to_vec()
    }

    // go-dep: github.com/pingcap/errors@v0.11.5-0.20250523034308-74f78ae071ee.Error.GetSelfMsg L1
    pub fn get_self_msg(&self) -> Vec<u8> {
        self.get_msg()
    }

    /// `fillLineAndFile`: the caller of the public constructor (`#[track_caller]`).
    // go-dep: github.com/pingcap/errors@v0.11.5-0.20250523034308-74f78ae071ee.Error.fillLineAndFile L2 (#[track_caller] instead of runtime.Caller)
    #[track_caller]
    fn fill_line_and_file(&mut self) {
        let loc = Location::caller();
        self.file = loc.file();
        self.line = loc.line() as i32;
        let bt = Backtrace::capture();
        if bt.status() == std::backtrace::BacktraceStatus::Captured {
            self.stack = Some(Arc::new(bt));
        }
    }

    /// `GenWithStack` generates a new *Error with the same class and code, and a new
    /// formatted message.
    // go-dep: github.com/pingcap/errors@v0.11.5-0.20250523034308-74f78ae071ee.Error.GenWithStack L1 [M-ERR]
    #[track_caller]
    pub fn gen_with_stack(&self, format: impl IntoGoString, args: Vec<Arg>) -> Error {
        // TODO: RedactErrorArg
        let mut err = self.clone();
        err.message = format.into_go_string();
        err.args = args;
        err.fill_line_and_file();
        err
    }

    /// `GenWithStackByArgs` generates a new *Error with the same class and code, and new
    /// arguments.
    // go-dep: github.com/pingcap/errors@v0.11.5-0.20250523034308-74f78ae071ee.Error.GenWithStackByArgs L1 [M-ERR]
    #[track_caller]
    pub fn gen_with_stack_by_args(&self, mut args: Vec<Arg>) -> Error {
        redact_error_arg(&mut args, &self.redact_args_pos);
        let mut err = self.clone();
        err.args = args;
        err.fill_line_and_file();
        err
    }

    /// `FastGen` generates a new *Error with the same class and code, and a new formatted
    /// message. This will not call runtime.Caller to get file and line.
    // go-dep: github.com/pingcap/errors@v0.11.5-0.20250523034308-74f78ae071ee.Error.FastGen L1 [M-ERR]
    pub fn fast_gen(&self, format: impl IntoGoString, args: Vec<Arg>) -> Error {
        // TODO: RedactErrorArg
        let mut err = self.clone();
        err.message = format.into_go_string();
        err.args = args;
        err
    }

    /// `FastGenByArgs` generates a new *Error with the same class and code, and a new
    /// arguments. This will not call runtime.Caller to get file and line.
    // go-dep: github.com/pingcap/errors@v0.11.5-0.20250523034308-74f78ae071ee.Error.FastGenByArgs L1 [M-ERR]
    pub fn fast_gen_by_args(&self, mut args: Vec<Arg>) -> Error {
        redact_error_arg(&mut args, &self.redact_args_pos);
        let mut err = self.clone();
        err.args = args;
        err
    }

    /// `Equal` checks if err is equal to e.
    // go-dep: github.com/pingcap/errors@v0.11.5-0.20250523034308-74f78ae071ee.Error.Equal L1
    pub fn equal(&self, err: &Error) -> bool {
        let origin_err = cause(err);
        if self.same_instance(origin_err) {
            return true;
        }
        if origin_err.kind != Kind::Normalized || self.kind != Kind::Normalized {
            return false;
        }
        self.id() == origin_err.id()
    }

    /// `NotEqual` checks if err is not equal to e.
    // go-dep: github.com/pingcap/errors@v0.11.5-0.20250523034308-74f78ae071ee.Error.NotEqual L1
    pub fn not_equal(&self, err: &Error) -> bool {
        !self.equal(err)
    }

    /// `Wrap`: a copy of e with cause err.
    // go-dep: github.com/pingcap/errors@v0.11.5-0.20250523034308-74f78ae071ee.Error.Wrap L1
    pub fn wrap(&self, err: Error) -> Error {
        let mut new_err = self.clone();
        new_err.cause = Some(Arc::new(err));
        new_err
    }

    /// `Unwrap` returns cause of the error.
    // go-dep: github.com/pingcap/errors@v0.11.5-0.20250523034308-74f78ae071ee.Error.Unwrap L1
    pub fn unwrap_cause(&self) -> Option<&Error> {
        self.cause.as_deref()
    }

    /// `Is` checks if e has the same error ID with other.
    // go-dep: github.com/pingcap/errors@v0.11.5-0.20250523034308-74f78ae071ee.Error.Is L1
    pub fn is(&self, other: &Error) -> bool {
        other.kind == Kind::Normalized && self.kind == Kind::Normalized && self.id() == other.id()
    }

    /// `Cause` (method): the root of the wrapped cause.
    // go-dep: github.com/pingcap/errors@v0.11.5-0.20250523034308-74f78ae071ee.Error.Cause L1 [M-ERR]
    pub fn cause(&self) -> Option<&Error> {
        self.cause.as_deref()
    }

    // go-dep: github.com/pingcap/errors@v0.11.5-0.20250523034308-74f78ae071ee.Error.FastGenWithCause L1 [M-ERR]
    pub fn fast_gen_with_cause(&self, args: Vec<Arg>) -> Error {
        let mut err = self.clone();
        if let Some(c) = &self.cause {
            err.message = Cow::Owned(c.error());
        }
        err.args = args;
        err
    }

    // go-dep: github.com/pingcap/errors@v0.11.5-0.20250523034308-74f78ae071ee.Error.GenWithStackByCause L1 [M-ERR]
    #[track_caller]
    pub fn gen_with_stack_by_cause(&self, args: Vec<Arg>) -> Error {
        let mut err = self.clone();
        if let Some(c) = &self.cause {
            err.message = Cow::Owned(c.error());
        }
        err.args = args;
        err.fill_line_and_file();
        err
    }
}

impl PartialEq for Error {
    /// `terror.ErrorEqual` semantics: same RFC code for `*errors.Error`, same text otherwise.
    // glue: Rust equality = errors.ErrorEqual
    fn eq(&self, other: &Self) -> bool {
        error_equal(Some(self), Some(other))
    }
}
