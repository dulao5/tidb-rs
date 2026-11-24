//! The context executors run with (N3 §3.1). The executor interface itself is
//! [`crate::internal::exec::executor::Executor`] (D12.0, VS1-13), re-exported here.
//!
//! glue: N3 (`ExecCtx`).

use std::sync::Arc;

use tidb_sessionctx::stmtctx::StatementContext;

/// What an executor (and any worker task it spawns) may use while the statement runs (N3
/// §3.1, N2): cheap to clone, `Send + Sync`, no `SessionVars`.
///
/// The other fields of foundation-interfaces §2 (cancellation token, statement `MemTracker`,
/// `ExecPool` handle, resource group, CPU recorder, store windows) and `checkpoint()` arrive
/// with the first executors that use them (VS1-34…VS1-36), wired to `tidb-runtime`. The SQL
/// killer is in the base executor (`executorKillerHandler`), as in Go.
#[derive(Clone)]
pub struct ExecCtx {
    /// The statement context, frozen for the execution (N2 `Arc<StmtCtx>`).
    pub stmt: Arc<StatementContext>,
}

impl ExecCtx {
    /// A context for the statement `stmt`.
    // glue: N3
    pub fn new(stmt: Arc<StatementContext>) -> ExecCtx {
        ExecCtx { stmt }
    }
}

pub use crate::internal::exec::executor::Executor;

const _: () = {
    // glue: compile-time check
    const fn send_sync<T: Send + Sync>() {}
    // glue: compile-time check
    const fn send<T: Send + ?Sized>() {}
    send_sync::<ExecCtx>();
    send::<Box<dyn Executor>>();
};
