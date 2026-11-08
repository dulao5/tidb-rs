//! Port of Go TiDB v8.5.8 `pkg/kv` (block D07.0): `Key`, `KeyRange`, the row handles and
//! handle maps, `KeyRanges`, the KV request type, the storage interfaces of the read path,
//! `RunInNewTxn` and the KV errors.
//!
//! tidb-rs is a read-only compute node: the write side of `pkg/kv` (memdb key flags,
//! mutations, locking, commit options) is done by Go TiDB and is not ported (tracker
//! scoping; spec `specs/D07.0-kv/`). N0 rank 5.

#![deny(unsafe_code)]

pub mod cachedb;
pub mod checker;
pub mod context;
pub mod error;
pub mod hack;
pub mod iter;
pub mod key;
pub mod kv;
pub mod mpp;
pub mod option;
pub mod txn;
pub mod txn_scope_var;
pub mod version;

pub use context::Context;
pub use key::Handle;
pub use key::Key;
pub use key::KeyRange;
