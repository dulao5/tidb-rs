//! Port of Go TiDB v8.5.8 block D00.2: constants, error codes and errors
//! (`pkg/parser/mysql`, `pkg/parser/terror`, `pkg/errno`, `pkg/util/dbterror`,
//! `kv.StoreType`; `pkg/config` follows in the same crate).
//!
//! The lowest layer of tidb-rs (N0, rank 0): every other crate may depend on it, it depends
//! on no tidb-rs crate. The Go code is the specification; see `specs/D00.2-base/` and the
//! `// go:` annotations.

#![forbid(unsafe_code)]
#![allow(non_upper_case_globals, non_snake_case, non_camel_case_types)]

pub mod dbterror;
pub mod errno;
pub mod errors;
pub mod gofmt;
pub mod gomap;
pub mod gostd;
pub mod kv;
pub mod mysql;
pub mod terror;
mod terror_registry;

pub use errors::Error;
pub use gofmt::Arg;
