//! The executor interface (N3 §3.1; Go `pkg/executor/internal/exec/executor.go`, interface
//! `Executor`, L51–76).
//!
//! glue: N3 (the interface shape). Go's `BaseExecutor` / `BaseExecutorV2`, `exec.Open`,
//! `exec.Next` (with its kill checkpoints) and `exec.Close` are D12.0 (VS1-13).

use std::sync::Arc;

use async_trait::async_trait;
use tidb_base::Error;
use tidb_chunk::chunk::Chunk;
use tidb_sessionctx::stmtctx::StatementContext;

/// What an executor (and any worker task it spawns) may use while the statement runs (N3
/// §3.1, N2): cheap to clone, `Send + Sync`, no `SessionVars`.
///
/// TODO(VS1-13): the other fields of foundation-interfaces §2 (`SqlKiller`, cancellation
/// token, statement `MemTracker`, `ExecPool` handle, resource group, CPU recorder, store
/// windows) and `checkpoint()` arrive with D12.0, wired to `tidb-runtime`.
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

/// Executor is the physical implementation of an algebra operator (Go `exec.Executor`).
///
/// Open / Next / Close are async (N3 §3.1 item 1). Go allows `Close` concurrently with
/// `Next`; here `&mut self` forbids it, so an external stop goes through the cancellation in
/// `ExecCtx` and the owner calls `close` (N3 §2, first table).
#[async_trait]
pub trait Executor: Send {
    /// `Open` initializes children recursively and "ctx" information.
    // glue: N3 (Go interface method Executor.Open)
    async fn open(&mut self, ctx: &ExecCtx) -> Result<(), Error>;
    /// `Next` fetches the next chunk of rows into `chk`; an empty chunk means the end.
    // glue: N3 (Go interface method Executor.Next)
    async fn next(&mut self, ctx: &ExecCtx, chk: &mut Chunk) -> Result<(), Error>;
    /// `Close` resets "ctx" information and releases resources (joins the worker tasks).
    // glue: N3 (Go interface method Executor.Close)
    async fn close(&mut self) -> Result<(), Error>;
}

const _: () = {
    // glue: compile-time check
    const fn send_sync<T: Send + Sync>() {}
    // glue: compile-time check
    const fn send<T: Send + ?Sized>() {}
    send_sync::<ExecCtx>();
    send::<Box<dyn Executor>>();
};
