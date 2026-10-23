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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SqlError {
    pub code: u16,
    /// Five-character SQLSTATE.
    pub state: &'static str,
    pub message: String,
}

impl SqlError {
    /// Builds an error with the SQLSTATE TiDB uses for `code` (`HY000` when it has no mapping).
    pub fn new(code: u16, message: impl Into<String>) -> Self {
        SqlError {
            code,
            state: sql_state(code),
            message: message.into(),
        }
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
}

impl fmt::Display for SqlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ERROR {} ({}): {}", self.code, self.state, self.message)
    }
}

impl std::error::Error for SqlError {}

/// SQLSTATE for an error code, following TiDB's `pkg/parser/mysql/state.go` for the codes this
/// crate uses; everything else maps to `HY000`.
pub fn sql_state(code: u16) -> &'static str {
    match code {
        code::ER_ACCESS_DENIED => "28000",
        code::ER_NO_DB => "3D000",
        code::ER_BAD_DB => "42000",
        code::ER_NET_PACKET_TOO_LARGE => "08S01",
        code::ER_NOT_SUPPORTED_YET => "42000",
        code::ER_NOT_SUPPORTED_AUTH_MODE => "08004",
        _ => "HY000",
    }
}
