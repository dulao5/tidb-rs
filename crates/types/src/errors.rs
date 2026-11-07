//! go-file: pkg/types/errors.go
//!
//! Temporary boundary stub (blocked-by D01.1, D00.2): only the five `types` errors that
//! `mydecimal.go` returns, with Go's error codes and message templates, plus `terror.Log`.
//! D00.2 (terror / errno) and D01.1 (the rest of `pkg/types`) replace this file.

use std::borrow::Cow;
use std::fmt;

/// glue: stub blocked-by D00.2 — the subset of `terror.Error` that MyDecimal needs.
///
/// Equality is by class and code, like `terror.Error.Equal`; Go compares `err == ErrOverflow`
/// against sentinels, which is the same test for these errors. An empty class marks an
/// error that is not a `terror.Error` (Go's `encoding/json` errors from `UnmarshalJSON`).
#[derive(Clone, Debug)]
pub struct Error {
    class: &'static str,
    code: u16,
    message: Cow<'static, str>,
}

impl PartialEq for Error {
    // glue: stub blocked-by D00.2 (terror.Error.Equal)
    fn eq(&self, other: &Self) -> bool {
        self.class == other.class && self.code == other.code
    }
}

impl Eq for Error {}

impl fmt::Display for Error {
    // glue: stub blocked-by D00.2 (terror.Error.Error: "[class:code]message")
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.class.is_empty() {
            f.write_str(&self.message)
        } else {
            write!(f, "[{}:{}]{}", self.class, self.code, self.message)
        }
    }
}

impl std::error::Error for Error {}

impl Error {
    // glue: stub blocked-by D00.2 (dbterror.ClassTypes.NewStd)
    const fn types(code: u16, message: &'static str) -> Error {
        Error {
            class: "types",
            code,
            message: Cow::Borrowed(message),
        }
    }

    /// MySQL error code (`terror.Error.Code`); 0 for non-terror errors.
    // glue: stub blocked-by D00.2 (terror.Error.Code)
    pub fn code(&self) -> u16 {
        self.code
    }

    /// An error that Go returns from the standard library (no class or code).
    // glue: stub blocked-by D00.2 (non-terror Go errors, e.g. from encoding/json)
    pub(crate) fn other(message: String) -> Error {
        Error {
            class: "",
            code: 0,
            message: Cow::Owned(message),
        }
    }

    /// `terror.Error.GenWithStackByArgs`: the message template formatted with `args`
    /// (Go `fmt.Sprintf`; only the `%s` verb with an optional `-` flag and `.N` precision
    /// occurs in the templates used here). Arguments are raw bytes like Go's `[]byte`/string.
    // glue: stub blocked-by D00.2 (terror.Error.GenWithStackByArgs)
    pub(crate) fn gen_with_stack_by_args(&self, args: &[&[u8]]) -> Error {
        let tmpl = self.message.as_bytes();
        let mut out: Vec<u8> = Vec::with_capacity(tmpl.len() + 16);
        let mut args = args.iter();
        let mut i = 0;
        while i < tmpl.len() {
            if tmpl[i] != b'%' {
                out.push(tmpl[i]);
                i += 1;
                continue;
            }
            i += 1;
            while i < tmpl.len() && tmpl[i] == b'-' {
                i += 1;
            }
            let mut prec: Option<usize> = None;
            if i < tmpl.len() && tmpl[i] == b'.' {
                i += 1;
                let mut p = 0usize;
                while i < tmpl.len() && tmpl[i].is_ascii_digit() {
                    p = p * 10 + usize::from(tmpl[i] - b'0');
                    i += 1;
                }
                prec = Some(p);
            }
            // The verb: only %s is used by the templates of these errors.
            i += 1;
            let arg = args.next().copied().unwrap_or(b"%!s(MISSING)");
            out.extend_from_slice(truncate_runes(arg, prec));
        }
        Error {
            class: self.class,
            code: self.code,
            // Go keeps the raw bytes; they reach users through UTF-8 encoders that replace
            // invalid sequences, as from_utf8_lossy does.
            message: Cow::Owned(String::from_utf8_lossy(&out).into_owned()),
        }
    }
}

/// Go fmt's precision for `%s`: keep at most `prec` runes, counting each invalid UTF-8 byte
/// as one rune (`utf8.DecodeRune` returns width 1 for it).
// glue: stub blocked-by D00.2 (Go fmt.(*fmt).truncate)
fn truncate_runes(s: &[u8], prec: Option<usize>) -> &[u8] {
    let Some(prec) = prec else { return s };
    let mut idx = 0;
    let mut n = 0;
    while idx < s.len() {
        if n == prec {
            return &s[..idx];
        }
        idx += crate::gostd::decode_rune(&s[idx..]).1;
        n += 1;
    }
    s
}

/// ErrTruncated is returned when data has been truncated during conversion.
pub const ERR_TRUNCATED: Error = Error::types(1265, "Data truncated for column '%s' at row %d");
/// ErrOverflow is returned when data is out of range for a field type.
pub const ERR_OVERFLOW: Error = Error::types(1690, "%s value is out of range in '%s'");
/// ErrDivByZero is return when do division by 0.
pub const ERR_DIV_BY_ZERO: Error = Error::types(1365, "Division by 0");
/// ErrBadNumber is return when parsing an invalid binary decimal number.
pub const ERR_BAD_NUMBER: Error = Error::types(8029, "Bad Number");
/// ErrTruncatedWrongVal is returned when data has been truncated during conversion.
pub const ERR_TRUNCATED_WRONG_VAL: Error =
    Error::types(1292, "Truncated incorrect %-.64s value: '%-.128s'");

/// `terror.Log(err)`: Go logs a non-nil error with its stack and continues.
// glue: stub blocked-by D00.2 (terror.Log) — the error is dropped until the logging
// infrastructure of D00.2 exists.
pub(crate) fn terror_log(err: Result<(), Error>) {
    let _ = err;
}

// ---------------------------------------------------------------------------------------
// The errors of `pkg/types/errors.go` as `tidb_base::Error` (D00.2, mapping rule M-ERR),
// with Go's identifiers (M-GONAME). The time code (D01.3) returns these; MyDecimal still
// returns the stub `Error` above until D01.1 ports `errors.go` whole (moving MyDecimal's
// hot error paths to the boxed `tidb_base::Error` needs its own benchmark review).

#[allow(non_upper_case_globals)]
mod go_errors {
    use std::sync::LazyLock;

    use tidb_base::dbterror::{ClassExecutor, ClassTypes};
    use tidb_base::{Error, errno, mysql};

    /// The type names used in error messages.
    pub const DateTimeStr: &str = "datetime";
    /// The type names used in error messages.
    pub const DateStr: &str = "date";
    /// The type names used in error messages.
    pub const TimeStr: &str = "time";
    /// The type names used in error messages.
    pub const TimestampStr: &str = "timestamp";

    /// ErrTruncated is returned when data has been truncated during conversion.
    pub static ErrTruncated: LazyLock<Error> =
        LazyLock::new(|| ClassTypes.new_std(mysql::WarnDataTruncated));
    /// ErrOverflow is returned when data is out of range for a field type.
    pub static ErrOverflow: LazyLock<Error> =
        LazyLock::new(|| ClassTypes.new_std(mysql::ErrDataOutOfRange));
    /// ErrWarnDataOutOfRange is returned when the value in a numeric column that is outside the permissible range of the column data type.
    /// See https://dev.mysql.com/doc/refman/5.5/en/out-of-range-and-overflow.html for details
    pub static ErrWarnDataOutOfRange: LazyLock<Error> =
        LazyLock::new(|| ClassTypes.new_std(mysql::ErrWarnDataOutOfRange));
    /// ErrDatetimeFunctionOverflow is returned when the calculation in datetime function cause overflow.
    pub static ErrDatetimeFunctionOverflow: LazyLock<Error> =
        LazyLock::new(|| ClassTypes.new_std(mysql::ErrDatetimeFunctionOverflow));
    /// ErrInvalidYearFormat is returned when the input is not a valid year format.
    pub static ErrInvalidYearFormat: LazyLock<Error> =
        LazyLock::new(|| ClassTypes.new_std(errno::ErrInvalidYearFormat));
    /// ErrTruncatedWrongVal is returned when data has been truncated during conversion.
    pub static ErrTruncatedWrongVal: LazyLock<Error> =
        LazyLock::new(|| ClassTypes.new_std(mysql::ErrTruncatedWrongValue));
    /// ErrWrongValue is returned when the input value is in wrong format.
    pub static ErrWrongValue: LazyLock<Error> = LazyLock::new(|| {
        ClassTypes.new_std_err(
            mysql::ErrTruncatedWrongValue,
            mysql::MySQLErrName
                .get(&mysql::ErrWrongValue)
                .expect("ErrWrongValue has a message"),
        )
    });
    /// ErrIncorrectDatetimeValue is returned when the input is not valid date time value.
    pub static ErrIncorrectDatetimeValue: LazyLock<Error> =
        LazyLock::new(|| ClassTypes.new_std(errno::ErrIncorrectDatetimeValue));
    /// ErrTimestampInDSTTransition is returned if the timestamp is in a Daylight Saving Time transition
    pub static ErrTimestampInDSTTransition: LazyLock<Error> =
        LazyLock::new(|| ClassExecutor.new_std(errno::ErrTimeStampInDSTTransition));
}

pub use go_errors::*;

impl From<Error> for tidb_base::Error {
    /// A MyDecimal (stub) error as the `tidb_base::Error` of the same class and code: what
    /// Go's time code returns when it passes a MyDecimal error through (`ParseTimeFromDecimal`).
    // glue: stub error -> tidb_base::Error, until D01.1 replaces the stub
    fn from(e: Error) -> tidb_base::Error {
        match e.code() {
            1265 => ErrTruncated.clone(),
            1690 => ErrOverflow.clone(),
            1365 => tidb_base::dbterror::ClassTypes.new_std(tidb_base::mysql::ErrDivisionByZero),
            8029 => tidb_base::dbterror::ClassTypes.new_std(tidb_base::errno::ErrBadNumber),
            _ => tidb_base::Error::plain(e.to_string()),
        }
    }
}
