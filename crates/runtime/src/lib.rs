//! Execution runtime of tidb-rs: the parts of N3 (`specs/exceptions/N3-A1-concurrency-memory/
//! spec.md`) that sit below every executor, session and store crate (N0 rank 1).
//!
//! | Module | N3 | What |
//! |---|---|---|
//! | [`pool`] | 3.1 第 2 条, 3.2 | The one tokio runtime in two topologies (Steal / NoSteal) and the [`ExecPool`] handle: `spawn`, `spawn_with_recover` |
//! | [`slice`] | 3.1 第 5 条, 3.6 | Per-task poll instrumentation: CPU-slice accounting, poll-stall detection, the time-sliced [`checkpoint`] |
//! | [`admission`] | 3.6 Q1 | Statement admission: Go `util.TokenLimiter` (`token-limit`) |
//! | [`store_window`] | 3.6 Q2 | Per-store in-flight window and Go/client-go `tidb_store_limit` |
//! | [`blocking`] | 3.6 Q3, 3.7 | Bounded blocking pool |
//! | [`metrics`] | 3.6 观测 | Queue depth and wait time, poll stalls, CPU slices |
//!
//! Defaults equal Go: unlimited Q2 window, `tidb_store_limit` 0 (off), `token-limit` 1000 and
//! queueing (never rejecting). The functional exceptions of N3 3.6 (per-group shares, rejecting
//! on overload, AIMD) are configuration that is off by default and reports "not supported" when
//! switched on, until their benchmarks decide.
//!
//! Every task must be started through [`ExecPool`] (or [`Runtime::spawn_connection`]): that
//! is what installs the per-task slice state that [`checkpoint`] and the stall detector use.

#![forbid(unsafe_code)]

pub mod admission;
pub mod blocking;
pub mod metrics;
pub mod pool;
pub mod slice;
pub mod store_window;

pub use admission::Admission;
pub use admission::AdmissionConfig;
pub use admission::TokenLimiter;
pub use blocking::BlockingPool;
pub use pool::ExecPool;
pub use pool::PanicError;
pub use pool::Runtime;
pub use pool::RuntimeConfig;
pub use pool::Topology;
pub use slice::CheckpointHook;
pub use slice::SliceConfig;
pub use slice::checkpoint;
pub use store_window::InflightPermit;
pub use store_window::StoreTokenLimitError;
pub use store_window::StoreWindows;
pub use store_window::StoreWindowsConfig;

/// A configuration switch of a functional exception (N3 3.6) that has not been implemented:
/// the default (Go) behaviour is the only one available until its benchmark decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotSupported(pub &'static str);

impl std::fmt::Display for NotSupported {
    // glue: error text of a not-yet-implemented N3 functional exception
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} is not supported yet (N3 3.6 functional exception, off by default)",
            self.0
        )
    }
}

impl std::error::Error for NotSupported {}
