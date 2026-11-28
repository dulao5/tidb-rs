//! The context executors run with (N3 §3.1). The executor interface itself is
//! [`crate::internal::exec::executor::Executor`] (D12.0, VS1-13), re-exported here.
//!
//! glue: N3 (`ExecCtx`).

use std::sync::Arc;

use tidb_base::Error;
use tidb_memory::sqlkiller::SqlKiller;
use tidb_memory::tracker::Tracker;
use tidb_runtime::cancel::KillCancel;
use tidb_runtime::pool::ExecPool;
use tidb_runtime::slice::CpuRecorder;
use tidb_runtime::{CheckpointHook, StoreWindows};
use tidb_sessionctx::stmtctx::StatementContext;

/// What an executor (and any worker task it spawns) may use while the statement runs (N3
/// §3.1, N2): cheap to clone, `Send + Sync`, no `SessionVars`.
#[derive(Clone)]
pub struct ExecCtx {
    /// The statement context, frozen for the execution (N2 `Arc<StmtCtx>`).
    pub stmt: Arc<StatementContext>,
    /// The `ExecPool` the tasks of an executor are spawned on (mapping rule M-GO; the parallel
    /// sort's workers, D12.5, and the parallel projection's, D12.2): the session's pool
    /// (`SessionContext::exec_pool`), so that the tasks' checkpoints yield and their stalls are
    /// detected (N3-A1 §3.1 item 5, §3.6). `None` spawns them on the current tokio runtime, as
    /// the coprocessor client does without a pool: contexts that are not a session, and tests.
    pub pool: Option<ExecPool>,
    /// Per-session SQL killer carrying kill / OOM / timeout signals and 1s connection-alive probes.
    pub killer: Arc<SqlKiller>,
    /// Resettable session-level cancellation notifier paired with [`Self::killer`].
    pub cancel: KillCancel,
    /// Optional per-statement CPU time recorder.
    pub cpu: Option<CpuRecorder>,
    /// Optional per-store latency/concurrency windows.
    pub store_windows: Option<Arc<StoreWindows>>,
}

impl ExecCtx {
    /// A context for the statement `stmt` with an isolated [`SqlKiller`] and [`KillCancel`].
    // glue: N3
    pub fn new(stmt: Arc<StatementContext>) -> ExecCtx {
        let killer = SqlKiller::new();
        let cancel = KillCancel::new(killer.signal_handle());
        ExecCtx {
            stmt,
            pool: None,
            killer,
            cancel,
            cpu: None,
            store_windows: None,
        }
    }

    /// A context for `stmt` wired to an existing [`SqlKiller`] and [`KillCancel`].
    // glue: N3
    pub fn with_killer(
        stmt: Arc<StatementContext>,
        killer: Arc<SqlKiller>,
        cancel: KillCancel,
    ) -> ExecCtx {
        ExecCtx {
            stmt,
            pool: None,
            killer,
            cancel,
            cpu: None,
            store_windows: None,
        }
    }

    /// Yields cleanly at a checkpoint and checks the statement's kill / OOM signal.
    // glue: N3 §3.1 cooperative checkpoint
    #[inline]
    pub async fn checkpoint(&self) -> Result<(), Error> {
        tidb_runtime::checkpoint(self).await
    }
}

impl CheckpointHook for ExecCtx {
    type Error = Error;

    // Invariant (A1 §3.3): never call `stmt.mem_tracker.checkpoint()` here; only leaf
    // operators owning allocations flush & checkpoint their own tracker.
    #[inline]
    fn check(&self) -> Result<(), Error> {
        self.killer.handle_signal()
    }
}

/// Combined leaf-memory and kill checkpoint hook for tight operator loops that both allocate
/// via `TrackedAlloc` and check kill signals.
pub struct OpCheckpoint<'a> {
    pub ctx: &'a ExecCtx,
    pub mem: &'a Arc<Tracker>,
}

impl CheckpointHook for OpCheckpoint<'_> {
    type Error = Error;

    #[inline]
    fn check(&self) -> Result<(), Error> {
        self.mem.checkpoint()?;
        self.ctx.check()
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
