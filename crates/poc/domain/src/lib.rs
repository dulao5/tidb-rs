//! Go-TiDB-style schema sync for the read-only node (Go: `pkg/domain`, `pkg/ddl/schemaver`,
//! `pkg/domain/infosync`).
//!
//! The node registers in PD's etcd as a TiDB server, keeps the newest schema loaded in the
//! background, and follows the metadata-lock (MDL) protocol, so the DDL owner waits for it
//! the way it waits for a Go TiDB. Statements then use the in-memory schema without reading
//! the schema version, and autocommit point gets read at MaxTS without a TSO ("synced mode").
//! Like Go, the cached schema stays usable for one schema lease after the last successful
//! reload ([`SchemaCheckError`], Go's `SchemaValidator`), so a slow reload does not stop the
//! node. When the node cannot prove it is in sync it stops admitting statements to synced
//! mode, then removes its registration so DDL is never blocked by it, and statements fall back
//! to the snapshot-consistent path of ADR 0003. See `docs/adr/0005-schema-sync.md`.
//!
//! - [`ServerInfo`]: the `/tidb/server/info/<ddl_id>` record.
//! - [`Registry`]: the loaded schema, the health state machine and the in-flight statements.
//! - [`SchemaSync`]: the etcd registration, reload loop, MDL reports and watchdog.

#![forbid(unsafe_code)]

pub mod keys;
mod registry;
mod schema_validator;
pub mod serverinfo;
mod sync;
pub mod systable_io;

pub use crate::registry::Admission;
pub use crate::registry::DEF_SCHEMA_LEASE;
pub use crate::registry::Limits;
pub use crate::registry::MdlJob;
pub use crate::registry::Registry;
pub use crate::registry::ResultCheck;
pub use crate::registry::StmtGuard;
pub use crate::registry::SyncStats;
pub use crate::schema_validator::SchemaCheckError;
pub use crate::schema_validator::SchemaCheckRetry;
pub use crate::serverinfo::ServerInfo;
pub use crate::sync::SchemaSync;
pub use crate::sync::SyncConfig;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("etcd: {0}")]
    Etcd(Box<etcd_client::Error>),
    #[error("etcd request timed out")]
    Timeout,
    #[error(transparent)]
    Kv(#[from] poc_kv::Error),
    #[error(transparent)]
    Meta(#[from] tidb_meta::Error),
    #[error("{0}")]
    Sql(String),
    #[error("{0}")]
    Other(String),
}

impl From<etcd_client::Error> for Error {
    fn from(e: etcd_client::Error) -> Error {
        Error::Etcd(Box::new(e))
    }
}

impl From<tidb_mysql_protocol::SqlError> for Error {
    fn from(e: tidb_mysql_protocol::SqlError) -> Error {
        Error::Sql(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;
