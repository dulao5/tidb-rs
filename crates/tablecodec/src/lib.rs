//! Port of Go TiDB v8.5.8 `pkg/tablecodec` and `pkg/util/rowcodec` (block D01.11): the table,
//! record and index key layouts, the row formats (v1: datum pairs; v2: `rowcodec`) and the
//! index value layouts (old encoding, version 0, clustered-index version 1).
//!
//! tidb-rs is a read-only compute node (block spec `specs/D01.11-tablecodec/spec.md` §2): the
//! write path (row and index value encoders, temp-index values, untouched-key checks) is
//! done by Go TiDB and is not ported (tracker block X). L1 port from Go; the vendored TiKV
//! codecs are a second, byte-for-byte oracle in `tests/tikv_oracle.rs` (owner decision
//! tidb-rs#39). N0 rank 6.

#![deny(unsafe_code)]

pub mod rowcodec;
pub mod rowindexcodec;
pub mod tablecodec;

pub use tablecodec::*;
