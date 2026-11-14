//! Port of Go TiDB v8.5.8 `pkg/session` (block D05.3; N0 rank 15).
//!
//! The session of N2 and ADR 0010 §4.1: protocol-neutral. It takes statements as AST
//! (`tidb_parser::ast`, the boundary to the semantic core, ADR 0010 §4.2) and parameters as
//! `Datum`s, returns results, warnings and `tidb_base::Error`s, and answers questions about
//! its state (in a transaction, autocommit, current database); protocol adapters translate.
//! It holds the connection's [`Dialect`] (ADR 0010 §4.4).
//!
//! VS1 phase 0 (`specs/VS1-vertical-slice/plan.md` §4): the session exists so that the PoC's
//! router can keep its state in sync and roll it back ([`checkpoint`]); statement
//! execution (`ExecuteStmt`, `PrepareStmt`, `ExecutePreparedStmt`) is a boundary stub
//! (`blocked-by:D05.3`) until VS1-14, so every statement still runs on the PoC.
#![deny(unsafe_code)]

pub mod checkpoint;
pub mod session;

pub use checkpoint::StmtCheckpoint;
pub use session::{Dialect, PreparedStmtInfo, RecordSet, Session};
