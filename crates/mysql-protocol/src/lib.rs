//! MySQL wire-protocol server layer for tidb-rs, mirroring the observable behaviour of TiDB's
//! `pkg/server` (handshake, capability flags, status flags, error codes, result encoding).
//!
//! The SQL layer plugs in through [`SessionFactory`] / [`SessionHandler`]; see `README.md`
//! for scope, design notes and known gaps.
//!
//! ```no_run
//! # use tidb_mysql_protocol::*;
//! # async fn demo<F: SessionFactory>(factory: F) -> std::io::Result<()> {
//! let listener = tokio::net::TcpListener::bind("127.0.0.1:4001").await?;
//! Server::new(ServerConfig::default(), factory).serve(listener, std::future::pending()).await
//! # }
//! ```

#![forbid(unsafe_code)]

pub mod auth;
pub mod codec;
mod conn;
pub mod constants;
pub mod error;
pub mod handler;
pub mod handshake;
pub mod packet;
pub mod params;
mod server;
pub mod tls;
pub mod value;

pub use auth::Credential;
pub use error::SqlError;
pub use handler::{
    ConnectionInfo, OkInfo, PreparedInfo, Response, ResultSet, SessionFactory, SessionHandler,
};
pub use params::Param;
pub use server::{Server, ServerConfig};
pub use value::{ColumnDef, ColumnType, DateTime, Duration, Value};
