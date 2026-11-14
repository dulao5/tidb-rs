//! Port of Go TiDB v8.5.8 `pkg/executor` (blocks D12.x; N0 rank 14).
//!
//! This crate starts with the executor interface of N3
//! (`specs/exceptions/N3-A1-concurrency-memory/spec.md` §3.1, foundation-interfaces §2):
//! [`exec::Executor`] is Go's Volcano interface `Open` / `Next(ctx, chk)` / `Close` with
//! async methods, used as `Box<dyn Executor>` (one boxed future per call); it is `Send`, not
//! `Sync`. Worker tasks only get an [`exec::ExecCtx`], never the session.
//!
//! VS1-13 (block D12.0): the executor interface and base executor (`internal::exec`),
//! `ExecStmt` / `recordSet` ([`adapter`]), the [`compiler`], the executor builder's dispatch
//! ([`builder`]), [`detach`], the executor metrics and helpers. What the framework needs from
//! the session and from unported blocks is in [`glue`]. The executors come with VS1-12 and
//! VS1-34…VS1-36.
#![deny(unsafe_code)]
// N13 rule 5 (M-SHARE): a clone of an `Arc` is written `Arc::clone(&x)`, so a bare `.clone()`
// is always a value clone (Go `Clone`).
#![warn(clippy::clone_on_ref_ptr)]

pub mod adapter;
pub mod builder;
pub mod compiler;
pub mod detach;
pub mod exec;
pub mod glue;
pub mod internal;
pub mod metrics;
pub mod utils;

pub use exec::{ExecCtx, Executor};
