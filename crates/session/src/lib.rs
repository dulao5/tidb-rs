//! Port of Go TiDB v8.5.8 `pkg/session` (block D05.3; N0 rank 15).
//!
//! The session of N2 and ADR 0010 §4.1: protocol-neutral. It takes statements as AST
//! (`tidb_parser::ast`, the boundary to the semantic core, ADR 0010 §4.2) and parameters as
//! `Datum`s, returns results, warnings and `tidb_base::Error`s, and answers questions about
//! its state (in a transaction, autocommit, current database); protocol adapters translate.
//! It holds the connection's [`Dialect`] (ADR 0010 §4.4).
//!
//! VS1-14 part 1 (`specs/D05.3-session/spec.md`): `ExecuteStmt` / `runStmt` /
//! `finishStmt` ([`session`], [`tidb`]), the lazy transaction ([`txn`]), the transaction
//! manager ([`txnmanager`]) over the sessiontxn providers (D05.4), the result of a statement
//! with rows ([`exec_result`]), and the statement checkpoint of the router ([`checkpoint`],
//! VS1 plan §4.6). Part 2 (VS1-14b): the prepared-statement entry points (`PrepareStmt`,
//! `ExecutePreparedStmt`, `DropPreparedStmt`); what they prepare and execute is the
//! plan-cache statement of prepared-statement planning (VS1-15), a boundary stub until then.
//!
//! N2 §5: the session is `Send`; `SessionVars` is not `Sync`, so an executor worker cannot
//! share it:
//!
//! ```compile_fail
//! fn sync<T: Sync>() {}
//! sync::<tidb_sessionctx::variable::session::SessionVars>();
//! ```
#![deny(unsafe_code)]
// N13 rule 5 (M-SHARE): a clone of an `Arc` is written `Arc::clone(&x)`.
#![warn(clippy::clone_on_ref_ptr)]

pub mod checkpoint;
pub mod exec_result;
pub mod expensive_query;
pub mod glue;
pub mod metrics;
pub mod session;
pub mod sessionexpr;
pub mod tidb;
pub mod txn;
pub mod txnmanager;

pub use checkpoint::StmtCheckpoint;
pub use exec_result::ExecStmtResult;
pub use session::{Dialect, PreparedStmtInfo, SessCtx, Session, SessionCore};
#[cfg(test)]
mod tests;
