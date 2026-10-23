//! The boundary between the protocol layer and SQL execution.
//!
//! The protocol layer owns everything on the wire: framing, handshake and authentication,
//! statement ids, parameter decoding (including `COM_STMT_SEND_LONG_DATA`), and result
//! encoding in the text or binary protocol. A [`SessionHandler`] only sees SQL text, decoded
//! parameters, and returns protocol-level [`Response`]s.

use std::future::Future;

use crate::auth::Credential;
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
}

/// Information about an authenticated connection, passed when a session is opened.
#[derive(Clone, Debug)]
pub struct ConnectionInfo {
    pub connection_id: u64,
    pub user: String,
    /// Client address (`ip:port`).
    pub peer: String,
    /// Default database from the handshake, if any.
    pub database: Option<String>,
    /// Collation id the client asked for in the handshake.
    pub collation: u8,
    /// Effective capability flags (client & server).
    pub capability: u32,
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

    /// `COM_STMT_CLOSE` (no response is sent to the client). Also called for every open
    /// statement on `COM_RESET_CONNECTION`.
    fn close(&mut self, stmt_id: u32);

    /// `COM_INIT_DB` and the handshake's default database.
    fn use_db(&mut self, db: &str) -> impl Future<Output = Result<(), SqlError>> + Send;

    /// Server status flags for OK/EOF packets. Read-only autocommit node: always
    /// `SERVER_STATUS_AUTOCOMMIT` unless overridden.
    fn status_flags(&self) -> u16 {
        status::AUTOCOMMIT
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

    /// Opens a session for an authenticated connection. The default database, if any, is then
    /// applied with [`SessionHandler::use_db`].
    fn open_session(&self, info: &ConnectionInfo) -> Result<Self::Session, SqlError>;
}
