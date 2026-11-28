//! SQL errors as sent in ERR packets.

use std::fmt;

/// Error codes used by the protocol layer (values from TiDB's `pkg/errno`).
pub mod code {
    pub const ER_NET_PACKET_TOO_LARGE: u16 = 1153;
    pub const ER_ACCESS_DENIED: u16 = 1045;
    pub const ER_NO_DB: u16 = 1046;
    pub const ER_BAD_DB: u16 = 1049;
    pub const ER_UNKNOWN: u16 = 1105;
    pub const ER_WRONG_ARGUMENTS: u16 = 1210;
    pub const ER_NOT_SUPPORTED_YET: u16 = 1235;
    pub const ER_UNKNOWN_STMT_HANDLER: u16 = 1243;
    pub const ER_NOT_SUPPORTED_AUTH_MODE: u16 = 1251;
    pub const ER_INVALID_SEQUENCE: u16 = 8052;
    pub const ER_UNKNOWN_FIELD_TYPE: u16 = 8051;
    pub const ER_INVALID_TYPE: u16 = 8057;
}

/// An error reported to the client as an ERR packet: MySQL error code, SQLSTATE and message.
///
/// Optionally carries the originating [`tidb_base::Error`] so `command dispatched failed`
/// (`err_str_for_log`) can emit the exact Go RFC code and stack trace (`ErrorStack`).
#[derive(Clone, Debug)]
pub struct SqlError {
    pub code: u16,
    /// Five-character SQLSTATE.
    pub state: &'static str,
    pub message: String,
    /// Optional underlying `tidb_base::Error` preserving RFC prefix & stack trace.
    pub source_err: Option<tidb_base::Error>,
}

impl PartialEq for SqlError {
    fn eq(&self, other: &Self) -> bool {
        self.code == other.code && self.state == other.state && self.message == other.message
    }
}

impl Eq for SqlError {}

impl SqlError {
    /// Builds an error with the SQLSTATE Go gives `code` ([`sql_state`]: `HY000` when it has
    /// no entry).
    pub fn new(code: u16, message: impl Into<String>) -> Self {
        SqlError {
            code,
            state: sql_state(code),
            message: message.into(),
            source_err: None,
        }
    }

    /// Attach the originating `tidb_base::Error` for structured logging (`err_str_for_log`).
    pub fn with_source_err(mut self, err: tidb_base::Error) -> Self {
        self.source_err = Some(err);
        self
    }

    /// Error 1105, used for errors without a specific code (e.g. TiDB's `ErrMalformPacket`).
    pub fn unknown(message: impl Into<String>) -> Self {
        Self::new(code::ER_UNKNOWN, message)
    }

    /// TiDB's `mysql.ErrMalformPacket`, which reaches clients as error 1105.
    pub fn malformed() -> Self {
        Self::unknown("malform packet error")
    }

    /// Error 1235 `ER_NOT_SUPPORTED_YET` with TiDB's message format.
    pub fn not_supported(what: impl fmt::Display) -> Self {
        Self::new(
            code::ER_NOT_SUPPORTED_YET,
            format!("This version of TiDB doesn't yet support '{what}'"),
        )
    }

    /// Error 1045 with TiDB's message format.
    pub fn access_denied(user: &str, host: &str, using_password: bool) -> Self {
        let yes_no = if using_password { "YES" } else { "NO" };
        Self::new(
            code::ER_ACCESS_DENIED,
            format!("Access denied for user '{user}'@'{host}' (using password: {yes_no})"),
        )
    }

    /// Error 1049.
    pub fn bad_db(db: &str) -> Self {
        Self::new(code::ER_BAD_DB, format!("Unknown database '{db}'"))
    }

    /// Error 1243.
    pub fn unknown_stmt(stmt_id: u32, command: &str) -> Self {
        Self::new(
            code::ER_UNKNOWN_STMT_HANDLER,
            format!("Unknown prepared statement handler ({stmt_id}) given to {command}"),
        )
    }

    /// Format this error the way Go `zap.Error(err)` formats a `terror` (with `[class:code]`
    /// RFC prefix when known, matching `pkg/server/conn.go:677` and `server.go:712`).
    pub fn error_for_log(&self) -> String {
        if let Some(src) = &self.source_err {
            return String::from_utf8_lossy(&src.error()).into_owned();
        }
        match self.code {
            code::ER_ACCESS_DENIED => format!("[privilege:{}]{}", self.code, self.message),
            code::ER_BAD_DB => format!("[schema:{}]{}", self.code, self.message),
            code::ER_NOT_SUPPORTED_AUTH_MODE => format!("[server:{}]{}", self.code, self.message),
            _ => self.message.clone(),
        }
    }
}

impl fmt::Display for SqlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ERROR {} ({}): {}", self.code, self.state, self.message)
    }
}

impl std::error::Error for SqlError {}

/// SQLSTATE for an error code: Go's `mysql.MySQLState` entry for the code, `HY000`
/// (`mysql.DefaultMySQLState`) when it has none. Go derives the SQLSTATE of every ERR packet
/// this way: `clientConn.writeError` builds the packet from `terror.ToSQLError` /
/// `mysql.NewErrf`, which look the code up in that table (pkg/parser/mysql/state.go).
// glue: the SQLSTATE step of mysql.NewErr / mysql.NewErrf (pkg/parser/mysql/error.go), over the generated MySQLState table
pub fn sql_state(code: u16) -> &'static str {
    match tidb_base::mysql::MySQLState.get(&code) {
        Some(s) => s,
        None => tidb_base::mysql::DefaultMySQLState,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Codes with their SQLSTATE in Go v8.5.8 (pkg/parser/mysql/state.go), including the
    /// coprocessor errors TiKV returns (out of range, division by zero, truncation).
    #[test]
    fn sql_state_follows_go_state_table() {
        for (code, state) in [
            (1690, "22003"), // ErrDataOutOfRange
            (1365, "22012"), // ErrDivisionByZero
            (1292, "22007"), // ErrTruncatedWrongValue
            (1062, "23000"), // ErrDupEntry
            (1065, "42000"), // ErrEmptyQuery
            (1045, "28000"),
            (1046, "3D000"),
            (1049, "42000"),
            (1054, "42S22"),
            (1064, "42000"),
            (1146, "42S02"),
            (1153, "08S01"),
            (1235, "42000"),
            (1251, "08004"),
            // No entry: HY000.
            (1105, "HY000"),
            (1193, "HY000"),
            (1243, "HY000"),
            (9001, "HY000"),
        ] {
            assert_eq!(sql_state(code), state, "code {code}");
            assert_eq!(SqlError::new(code, "m").state, state, "code {code}");
        }
    }
}
