//! Port of Go TiDB v8.5.8 `pkg/sessionctx` (blocks D00.1, D05.1, D05.2). N0 rank 7.
//!
//! So far: block D05.1, the system variable registry and validation (`variable`, N8 §3.1);
//! block D00.1 part 1: `errctx` and `stmtctx` (warnings, error levels, type flags);
//! part 2a: the expression context interfaces (`exprctx`), the optional property providers
//! (`expropt`), the static expression contexts (`exprstatic`), the ranger types (`ranger`) and
//! the `planctx` trait skeleton.

#![deny(unsafe_code)]

pub mod errctx;
pub mod exprctx;
pub mod expropt;
pub mod exprstatic;
pub mod planctx;
pub mod ranger;
pub mod stmtctx;
pub mod variable;
