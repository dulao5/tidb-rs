//! Port of Go TiDB v8.5.8 `pkg/server` (block D03.1; N0 rank 16), part 1 (VS1 work item
//! "result encoding and the prepared-statement protocol on the new stack"): what turns
//! the new session's results into MySQL protocol packets and the binary parameters of
//! `COM_STMT_EXECUTE` into values.
//!
//! - [`dump`]: length-encoded integers and strings, binary TIME / DATETIME
//!   (`internal/dump`);
//! - [`column`], [`convert`], [`result_encoder`]: column definitions, `DumpTextRow` /
//!   `DumpBinaryRow`, `ConvertColumnInfo`, the result encoder for
//!   `@@character_set_results` (`internal/column`);
//! - [`util`]: `AppendFormatFloat`, length-encoded parsing, the input decoder
//!   (`internal/util`);
//! - [`params`]: `parseBinaryParams` (`conn_stmt_params.go`);
//! - [`resultset`]: the result set over the session's result (`internal/resultset`);
//! - [`conn`]: `writeResultSet` / `writeChunks` / `writeColumnInfo` into packet payloads;
//! - [`server`]: `setSystemTimeZoneVariable` of the server start (`server.go`).
//!
//! The connection loop, handshake and authentication stay with the PoC
//! (`tidb-mysql-protocol`) until part 2 (spec `specs/D03.1-server/spec.md`).
#![deny(unsafe_code)]
// N13 rule 5 (M-SHARE): a clone of an `Arc` is written `Arc::clone(&x)`.
#![warn(clippy::clone_on_ref_ptr)]

pub mod column;
pub mod conn;
pub mod convert;
pub mod dump;
pub mod params;
pub mod result_encoder;
pub mod resultset;
pub mod server;
pub mod util;
