//! Port of Go TiDB v8.5.8 `pkg/executor` (blocks D12.x; N0 rank 14).
//!
//! This crate starts with the executor interface of N3
//! (`specs/exceptions/N3-A1-concurrency-memory/spec.md` §3.1, foundation-interfaces §2):
//! [`exec::Executor`] is Go's Volcano interface `Open` / `Next(ctx, chk)` / `Close` with
//! async methods, used as `Box<dyn Executor>` (one boxed future per call); it is `Send`, not
//! `Sync`. Worker tasks only get an [`exec::ExecCtx`], never the session.
//!
//! The executors, `ExecStmt`, the compiler and the executor builder are ported by the VS1 work
//! items (VS1-12, VS1-13, VS1-34…VS1-36).
#![deny(unsafe_code)]

pub mod exec;

pub use exec::{ExecCtx, Executor};
