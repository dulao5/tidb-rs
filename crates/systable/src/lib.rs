//! Block D06.7: the system-table writes behind `SET GLOBAL` (D05.2 spec §6, owner decision
//! tidb-rs #58 option (c)).
//!
//! Go persists a global variable with `REPLACE INTO mysql.global_variables` (and the
//! `tidb_gc_*` variables with `REPLACE INTO mysql.tidb`) through its generic executor, then
//! notifies the other instances through etcd. tidb-rs does not port the generic DML executor
//! (X); this crate has the specialized pure logic instead:
//! - [`replace`]: the REPLACE of one row, as Go's executor writes it in an optimistic
//!   transaction (record, unique index, key flags, assertions);
//! - [`autoid`]: the row-ID allocator (`alloc4Signed`), shared with Go through the meta
//!   counter;
//! - [`memdb`]: the transaction buffer the writes go to, over a caller-provided snapshot;
//! - [`sysvar_cache`]: the domain's sysvar cache and its rebuild from the table.
//!
//! The caller (the legacy `poc-domain`, until D06.5 / D07.1) runs the TiKV transactions,
//! the etcd notification and the reload loop.
// N13 rule 5 (M-SHARE): a clone of an `Arc` is written `Arc::clone(&x)`, so a bare `.clone()`
// is always a value clone (Go `Clone`).
#![warn(clippy::clone_on_ref_ptr)]

pub mod autoid;
pub mod memdb;
pub mod replace;
pub mod session_io;
pub mod sysvar_cache;
