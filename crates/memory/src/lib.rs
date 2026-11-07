//! Port of Go TiDB v8.5.8 block D15.1: memory accounting and quota (`pkg/util/memory`),
//! with `pkg/util/sqlkiller` (block D00.3, ported whole into its lowest user) and the A1
//! allocator [`TrackedAlloc`].
//!
//! N0 rank 1: depends on `tidb-base` and `tidb-metrics` only. The design is fixed by the
//! A1 spec (`specs/exceptions/N3-A1-concurrency-memory/spec.md`, part 2); the block spec is
//! `specs/D15.1-memory/`.

#![deny(unsafe_code)]

pub mod action;
mod alloc;
pub mod cgroup;
pub mod meminfo;
pub mod memstats;
pub mod sqlkiller;
pub mod tracker;

pub use action::ActionOnExceed;
pub use action::ActionRef;
pub use alloc::TRACKED_ALLOC_BATCH;
pub use alloc::TrackedAlloc;
pub use sqlkiller::SqlKiller;
pub use tracker::Tracker;
