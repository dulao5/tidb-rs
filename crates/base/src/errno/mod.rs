//! Port of Go TiDB v8.5.8 `pkg/errno`: TiDB's error codes and messages, and the
//! `INFORMATION_SCHEMA.*_ERRORS_SUMMARY` counters (block D00.2).
//!
//! `errcode.rs` and `errname.rs` are generated from the Go source (tracker
//! `govec/cmd/rsgen`) and checked against Go by the govec domain "base".

#![allow(non_upper_case_globals)]

mod errcode;
mod errname;
mod infoschema;

pub use errcode::*;
pub use errname::*;
pub use infoschema::*;
