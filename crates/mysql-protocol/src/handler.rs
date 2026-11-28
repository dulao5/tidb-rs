//! The boundary between the protocol layer and SQL execution.
//!
//! The protocol layer owns everything on the wire: framing, handshake and authentication,
//! statement ids, parameter decoding (including `COM_STMT_SEND_LONG_DATA`), and result
//! encoding in the text or binary protocol. A [`SessionHandler`] only sees SQL text, decoded
//! parameters, and returns protocol-level [`Response`]s.

use std::future::Future;

use crate::auth::{AccountAuth, Credential};
use crate::constants::status;
use crate::error::SqlError;
use crate::params::Param;
use crate::value::{ColumnDef, Value};

/// Result of a statement.
#[derive(Clone, Debug, PartialEq)]
pub enum Response {
    /// A statement without a result set (`SET`, `USE`, ...).
    Ok(OkInfo),
    /// A result set. Rows are fully materialised for now; streaming can be added later
    /// without changing the wire behaviour.
    ResultSet(ResultSet),
    /// A result set whose packets were encoded by the caller (the new stack's port of Go's
    /// `writeChunks`, D03.1): the connection only frames them and adds the EOF / OK
    /// packets.
    Encoded(EncodedResultSet),
}

/// A result set as packet payloads: one per column definition, then one per row (text or
/// binary, as the command asked). `columns` describe the same definitions for metadata
/// checks; they are not what is sent.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EncodedResultSet {
    pub columns: Vec<ColumnDef>,
    pub column_packets: Vec<Vec<u8>>,
    pub rows: Vec<Vec<u8>>,
    pub warnings: u16,
}

impl Response {
    pub fn ok() -> Self {
        Response::Ok(OkInfo::default())
    }
}

/// Payload of an OK packet.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OkInfo {
    pub affected_rows: u64,
    pub last_insert_id: u64,
    pub warnings: u16,
    /// Human-readable info string (empty for most statements).
    pub info: String,
}

/// A materialised result set.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResultSet {
    pub columns: Vec<ColumnDef>,
    /// Each row has exactly `columns.len()` values.
    pub rows: Vec<Vec<Value>>,
    pub warnings: u16,
}

/// Metadata returned by a successful prepare.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PreparedInfo {
    /// Number of `?` placeholders.
    pub param_count: u16,
    /// Result columns, if known at prepare time (may be empty; the execute response always
    /// carries the actual column definitions).
    pub columns: Vec<ColumnDef>,
    /// The column definition packets to send instead of encoding `columns` (the new
    /// stack's port of Go's `Info.Dump`); empty: encode `columns`.
    pub column_packets: Vec<Vec<u8>>,
}

/// The raw parameter section of a `COM_STMT_EXECUTE`, as Go's `parseBinaryParams` reads
/// it: the NULL bitmap, the types (from this or an earlier execute), the values, and the
/// data of `COM_STMT_SEND_LONG_DATA` per parameter.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RawParams {
    pub null_bitmap: Vec<u8>,
    pub param_types: Vec<u8>,
    pub param_values: Vec<u8>,
    pub bound: Vec<Option<Vec<u8>>>,
}

/// Information about an authenticated connection, passed when a session is opened.
#[derive(Clone, Debug)]
pub struct ConnectionInfo {
    pub connection_id: u64,
    pub user: String,
    /// Client address (`ip:port`).
    pub peer: String,
    /// Matched `mysql.user.User` from authentication (`None` when using legacy `lookup_user`).
    pub auth_user: Option<String>,
    /// Matched `mysql.user.Host` from authentication (`None` when using legacy `lookup_user`).
    pub auth_host: Option<String>,
    /// Default database from the handshake, if any.
    pub database: Option<String>,
    /// Collation id the client asked for in the handshake.
    pub collation: u8,
    /// Effective capability flags (client & server).
    pub capability: u32,
    /// Negotiated TLS cipher suite (e.g. `TLS13_AES_128_GCM_SHA256`); `None` for plain TCP.
    pub tls_cipher: Option<String>,
}

/// Per-connection SQL session. All methods are called sequentially from the connection's task.
///
/// Methods return `impl Future + Send` so connections can run on a multi-threaded runtime;
/// implementations can simply write `async fn`.
pub trait SessionHandler: Send + 'static {
    /// `COM_QUERY`: execute one SQL statement (multi-statements are not enabled).
    fn query(&mut self, sql: &str) -> impl Future<Output = Result<Response, SqlError>> + Send;

    /// `COM_STMT_PREPARE`. `stmt_id` is allocated by the protocol layer and is used for the
    /// later `execute`/`close` calls of this statement.
    fn prepare(
        &mut self,
        stmt_id: u32,
        sql: &str,
    ) -> impl Future<Output = Result<PreparedInfo, SqlError>> + Send;

    /// `COM_STMT_EXECUTE`. `params` has exactly `param_count` entries (as returned by
    /// `prepare`), already decoded from the binary protocol.
    fn execute(
        &mut self,
        stmt_id: u32,
        params: Vec<Param>,
    ) -> impl Future<Output = Result<Response, SqlError>> + Send;

    /// The flags byte of the coming `COM_STMT_EXECUTE` (`cursor_type::*`), called right before
    /// [`SessionHandler::execute`]. `CURSOR_TYPE_READ_ONLY` is accepted without opening a
    /// cursor; the VS1 router keeps such executions on the PoC. The default ignores it.
    fn execute_flags(&mut self, _stmt_id: u32, _flags: u8) {}

    /// The raw parameters of the coming `COM_STMT_EXECUTE`, called right before
    /// [`SessionHandler::execute`] (after a successful decoding into `params`), for an
    /// engine that decodes them itself. The default ignores them.
    fn execute_raw_params(&mut self, _stmt_id: u32, _raw: RawParams) {}

    /// `COM_STMT_CLOSE` (no response is sent to the client). Also called for every open
    /// statement on `COM_RESET_CONNECTION`.
    fn close(&mut self, stmt_id: u32);

    /// `COM_INIT_DB` and the handshake's default database.
    fn use_db(&mut self, db: &str) -> impl Future<Output = Result<(), SqlError>> + Send;

    /// `COM_RESET_CONNECTION`, after every open statement was closed: Go's
    /// `handleResetConnection` closes the session (a transaction is rolled back) and opens a
    /// new one for the same user, with the current database restored. The OK packet that
    /// follows carries the new session's status flags. The default keeps the session.
    fn reset_connection(&mut self) -> impl Future<Output = Result<(), SqlError>> + Send {
        std::future::ready(Ok(()))
    }

    /// Called after a COM_QUERY / COM_STMT_EXECUTE response was flushed (also after errors):
    /// `write` is the time spent encoding and writing the response, `total` the time since the
    /// command packet was read. For statistics only; the default ignores it.
    fn command_done(&mut self, _write: std::time::Duration, _total: std::time::Duration) {}

    /// Type of the statement the last `COM_QUERY` / `COM_STMT_EXECUTE` ran (Go
    /// `StmtCtx.StmtType`, e.g. `Select`; it outlives the statement, so other commands report
    /// it too): the `sql_type` of `tidb_server_handle_query_duration_seconds`. `General` if
    /// unknown (no statement yet, a parse error, after a prepare).
    fn stmt_type(&self) -> tidb_metrics::StmtType {
        tidb_metrics::StmtType::General
    }

    /// Server status flags for OK/EOF packets. Read-only autocommit node: always
    /// `SERVER_STATUS_AUTOCOMMIT` unless overridden.
    fn status_flags(&self) -> u16 {
        status::AUTOCOMMIT
    }

    /// Session alias (`tidb_session_alias`, empty by default).
    fn session_alias(&self) -> &str {
        ""
    }

    /// Readable transaction mode for `command dispatched failed` (Go `GetReadableTxnMode`).
    fn txn_mode(&self) -> &str {
        "PESSIMISTIC"
    }

    /// Active transaction start TS (or stale read TS, or 0) for `command dispatched failed`.
    fn txn_timestamp(&self) -> u64 {
        0
    }

    /// Active `tidb_redact_log` mode (`"OFF"`, `"ON"`, or `"MARKER"`).
    fn redact_mode(&self) -> &str {
        tidb_base::errors::redact_log_enabled_load()
    }

    /// Connection collation name for `connInfo` (`utf8mb4_0900_ai_ci` / `utf8mb4_bin`).
    fn collation_name(&self) -> &str {
        "utf8mb4_bin"
    }
}

/// Creates sessions and looks up accounts. Shared by all connections.
pub trait SessionFactory: Send + Sync + 'static {
    type Session: SessionHandler;

    /// Returns the stored credential of `user` connecting from `host`, or `None` if the account
    /// does not exist (the client then gets `ER_ACCESS_DENIED`).
    fn lookup_user(
        &self,
        user: &str,
        host: &str,
    ) -> impl Future<Output = Option<Credential>> + Send;

    /// Returns full account authentication metadata (credential, matched `auth_user`/`auth_host`,
    /// and `account_locked` status). Defaults to wrapping [`Self::lookup_user`].
    fn lookup_account(
        &self,
        user: &str,
        host: &str,
    ) -> impl Future<Output = Option<AccountAuth>> + Send {
        async move { self.lookup_user(user, host).await.map(AccountAuth::from) }
    }

    /// Opens a session for an authenticated connection. The default database, if any, is then
    /// applied with [`SessionHandler::use_db`].
    fn open_session(&self, info: &ConnectionInfo) -> Result<Self::Session, SqlError>;
}
