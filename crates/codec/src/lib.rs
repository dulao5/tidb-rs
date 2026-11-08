//! Port of Go TiDB v8.5.8 `pkg/util/codec` (block D01.8): the memcomparable and value
//! encodings of integers, floats, byte strings and decimals; the Datum codec used for keys,
//! values and ranges; decoding into a chunk; and the chunk-row hashing and key serialisation
//! of hash join and aggregation.
//!
//! The port is L1 from Go (block spec `specs/D01.8-codec/spec.md` §2, owner decision pending
//! in tidb-rs#39); the vendored TiKV encoders are a second, byte-for-byte oracle in
//! `tests/tikv_oracle.rs`, not an implementation dependency. N0 rank 4.

#![deny(unsafe_code)]

pub mod bytes;
pub mod codec;
pub mod decimal;
pub mod float;
pub mod gobinary;
pub mod number;

pub use bytes::*;
pub use codec::*;
pub use decimal::*;
pub use float::*;
pub use number::*;
