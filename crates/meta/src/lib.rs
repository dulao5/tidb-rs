//! TiDB schema metadata for the read-only node.
//!
//! - [`keys`]: raw TiKV keys of the meta structures (`m` prefix, string/hash layout).
//! - [`model`]: `DBInfo` / `TableInfo` / `ColumnInfo` / `IndexInfo` decoded from their JSON.
//! - [`MetaReader`]: schema version, databases and tables at one snapshot timestamp.
//! - [`InfoSchema`]: all public schemas of one version, looked up by name or id.
//! - [`SchemaCache`]: picks the schema for a statement from its `start_ts`, reloading when
//!   the version visible at `start_ts` is not cached. See `docs/adr/0003-schema-loading.md`.

#![forbid(unsafe_code)]

mod infoschema;
pub mod keys;
pub mod model;
mod reader;

pub use crate::infoschema::CacheStats;
pub use crate::infoschema::InfoSchema;
pub use crate::infoschema::SchemaCache;
pub use crate::infoschema::SchemaCheck;
pub use crate::reader::MetaReader;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Kv(#[from] tidb_kv::Error),
    /// A meta value could not be decoded.
    #[error("corrupt meta: {0}")]
    Corrupt(String),
}

impl Error {
    fn corrupt(what: String, e: serde_json::Error) -> Error {
        Error::Corrupt(format!("{what}: {e}"))
    }
}

pub type Result<T> = std::result::Result<T, Error>;
