//! go-file: pkg/parser/mysql/error.go

// Go identifiers for constants and statics (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use std::sync::LazyLock;

use super::{DefaultMySQLState, MySQLErrName, MySQLState};
use crate::errors::{self, Error};
use crate::gofmt::{self, Arg};

// Portable analogs of some common call errors.

/// `ErrBadConn` (plain Go error).
pub static ErrBadConn: LazyLock<Error> = LazyLock::new(|| errors::new("connection was bad"));
/// `ErrMalformPacket` (plain Go error).
pub static ErrMalformPacket: LazyLock<Error> =
    LazyLock::new(|| errors::new("malform packet error"));

/// SQLError records an error information, from executing SQL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SQLError {
    pub code: u16,
    /// Go string (bytes).
    pub message: Vec<u8>,
    pub state: String,
}

impl SQLError {
    /// Error prints errors, with a formatted string.
    // go: SQLError.Error L1 [M-NAME]
    pub fn error(&self) -> Vec<u8> {
        let mut out = format!("ERROR {} ({}): ", self.code, self.state).into_bytes();
        out.extend_from_slice(&self.message);
        out
    }
}

impl std::fmt::Display for SQLError {
    // glue: Display delegates to Error() (M-NAME), lossy (logs only)
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&String::from_utf8_lossy(&self.error()))
    }
}

impl std::error::Error for SQLError {}

/// NewErr generates a SQL error, with an error code and default format specifier defined in
/// MySQLErrName.
// go: NewErr L1
pub fn new_err(err_code: u16, mut args: Vec<Arg>) -> SQLError {
    let mut e = SQLError {
        code: err_code,
        message: Vec::new(),
        state: String::new(),
    };

    if let Some(s) = MySQLState.get(&err_code) {
        e.state = (*s).to_owned();
    } else {
        e.state = DefaultMySQLState.to_owned();
    }

    if let Some(sql_err) = MySQLErrName.get(&err_code) {
        errors::redact_error_arg(&mut args, sql_err.redact_arg_pos);
        e.message = gofmt::sprintf(sql_err.raw, &args);
    } else {
        e.message = gofmt::sprint(&args);
    }

    e
}

/// NewErrf creates a SQL error, with an error code and a format specifier.
// go: NewErrf L1
pub fn new_errf(
    err_code: u16,
    format: impl AsRef<[u8]>,
    redact_arg_pos: &[usize],
    mut args: Vec<Arg>,
) -> SQLError {
    let mut e = SQLError {
        code: err_code,
        message: Vec::new(),
        state: String::new(),
    };

    if let Some(s) = MySQLState.get(&err_code) {
        e.state = (*s).to_owned();
    } else {
        e.state = DefaultMySQLState.to_owned();
    }

    errors::redact_error_arg(&mut args, redact_arg_pos);
    e.message = gofmt::sprintf(format, &args);

    e
}
