//! Port of Go TiDB v8.5.8 `pkg/sessionctx` (blocks D00.1, D05.1, D05.2). N0 rank 7.
//!
//! So far: block D05.1, the system variable registry and validation (`variable`, N8 §3.1);
//! block D00.1 part 1: `errctx` and `stmtctx` (warnings, error levels, type flags).

#![deny(unsafe_code)]

pub mod errctx;
pub mod stmtctx;
pub mod variable;
