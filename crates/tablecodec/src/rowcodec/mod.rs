//! Port of `pkg/util/rowcodec`: the row format v2 (`row.go`), its decoders (`decoder.go`),
//! the shared helpers (`common.go`) and the encoder (`encoder.go`, brought back from X for
//! block D06.7, the `SET GLOBAL` system-table writer).

pub mod common;
pub mod decoder;
pub mod encoder;
pub mod row;

pub use common::*;
pub use decoder::*;
