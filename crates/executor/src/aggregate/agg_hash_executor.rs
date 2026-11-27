//! go-file: pkg/executor/aggregate/agg_hash_executor.go
//!
//! The hash aggregation executor (D12.3 part B; spec `specs/D12.3-agg-executors/spec.md`).
//!
//! Implements both single-threaded (`unparallelExec`) and parallel (`parallelExec`) execution
//! paths plus the spill-trigger state machines; real disk writes stop at `blocked-by:D01.10`
//! (`DataInDiskByChunks.Add` / `GetChunk` for unparallel, `HashAggExec.spill` for parallel).
//!
//! Differences from Go (spec §4, §6):
//! - Memory accounting follows A1 (`TrackedAlloc`): `group_set`, `group_keys`,
//!   `partial_result_map`, and growing partial-result state allocate through `ColAlloc::Tracked`
//!   backed by `mem_tracker`, replacing Go's manual delta estimates. `mem_tracker.checkpoint()?`
//!   runs quota/soft-limit actions at Go's positive `Consume` sites.
//! - Parallel tasks are spawned on `ExecCtx.pool` (or the current tokio runtime when `pool`
//!   is `None`, mapping rule M-GO). `finishCh` is a `watch::channel(false)`. `Drop` sends
//!   finish and aborts all live tasks (including if `Close`'s future is dropped mid-await,
//!   spec §4.4).
//! - `HashAggInput` carries the target `worker_idx` instead of a channel sender so dropping
//!   `partial_input_txs` in the fetcher closes every partial worker's input channel (spec §4.3).
//! - Session values (`enable_vectorized_expression`, `track_aggregate_memory_usage`,
//!   `enable_parallel_hashagg_spill`, partial/final concurrency) are captured when the
//!   executor is built (N2); statement trackers, `RuntimeStatsColl`, `ErrCtx`, and time zone
//!   come from `ExecCtx`.
//! - The Rust session does not yet carry a session-level `MemTracker` (A1 follow-up, spec
//!   §6 item 11, §8.2 decision 6); when `session_mem_tracker` is supplied (e.g. by tests or
//!   once A1 wires it into `SessionVars`), soft-limit spill actions are registered on it.
//! - `prepared` and `executed` are plain `bool` (accessed only by `&mut self`, spec §4.1).
//! - Failpoints and test-only hook methods (spec §2.4, §6 item 14) are not ported.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use std::time::Instant;

use allocator_api2::boxed::Box as AllocBox;
use allocator_api2::vec::Vec as AllocVec;
use async_trait::async_trait;
use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_base::mysql;
use tidb_chunk::ColAlloc;
use tidb_chunk::chunk::{self, Chunk};
use tidb_chunk::chunk_in_disk::{DataInDiskByChunks, new_data_in_disk_by_chunks};
use tidb_execdetails::execdetails::{RuntimeStats, RuntimeStatsColl};
use tidb_expression::ExprRef;
use tidb_expression::eval::RpnState;
use tidb_expression::expression::EvalState;
use tidb_kv::set::{StringSetWithMemoryUsage, new_string_set_with_memory_usage};
use tidb_memory::TrackedAlloc;
use tidb_memory::action::ActionOnExceed;
use tidb_memory::tracker::{Tracker, new_tracker};
use tidb_runtime::channel::clear as channel_clear;
use tidb_runtime::pool::ExecPool;
use tidb_runtime::{DoneGuard, WaitGroup};
use tidb_sessionctx::exprctx::EvalContextRef;
use tidb_sessionctx::variable::tidb_vars::EnableTmpStorageOnOOM;
use tidb_types::field_type::new_field_type;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::aggfuncs::{
    AggFunc, AggPartialResultMapper, PartialResult, new_agg_partial_result_mapper_in,
};
use crate::exec::ExecCtx;
use crate::internal::exec::executor::{self as exec, BaseExecutor, BaseExecutorV2, Executor};

use super::agg_hash_base_worker::{new_base_hash_agg_worker, wait_finish};
use super::agg_hash_final_worker::{AfFinalResult, HashAggFinalWorker};
use super::agg_hash_partial_worker::HashAggPartialWorker;
use super::agg_spill::{
    AggSpillDiskAction, ParallelAggSpillDiskAction, ParallelHashAggSpillHelper, new_spill_helper,
};
use super::agg_util::{
    AggWorkerStat, HashAggRuntimeStats, get_buffer, get_group_key, recovery_hash_agg,
};

/// `HashAggInput` indicates the input chunk recycled between the data fetcher and a specific
/// partial worker.
///
/// Carries `worker_idx` instead of a channel sender so the fetcher exclusively owns all
/// `partial_input_txs` and closes them when exiting (spec §4.3).
pub struct HashAggInput {
    pub(crate) chk: Chunk,
    pub(crate) worker_idx: usize,
}

/// Return value of the fetcher task: the child executor and `input_rx` handed back to `Close`.
type FetcherReturn = (Box<dyn Executor>, mpsc::Receiver<HashAggInput>);

/// Owned task handles of a running parallel `HashAggExec`; `Drop` signals `finish_tx` and
/// aborts every remaining live task (spec §4.4, N3 3.5).
#[derive(Default)]
struct ParallelTasks {
    finish_tx: Option<watch::Sender<bool>>,
    fetcher: Option<JoinHandle<FetcherReturn>>,
    partial_workers: Vec<JoinHandle<()>>,
    final_workers: Vec<JoinHandle<()>>,
    aux_tasks: Vec<JoinHandle<()>>,
}

impl ParallelTasks {
    // glue: N3 3.5 (abort on drop)
    fn abort_all(&mut self) {
        if let Some(tx) = &self.finish_tx {
            tx.send_replace(true);
        }
        if let Some(h) = &self.fetcher {
            h.abort();
        }
        for h in &self.partial_workers {
            h.abort();
        }
        for h in &self.final_workers {
            h.abort();
        }
        for h in &self.aux_tasks {
            h.abort();
        }
    }
}

impl Drop for ParallelTasks {
    // glue: N3 3.5 (executor dropped without Close, or Close future dropped mid-await)
    fn drop(&mut self) {
        self.abort_all();
    }
}

/// Helper that spawns tasks on `ExecCtx.pool` when present, or the current tokio runtime
/// otherwise (same pattern as D12.2 / D12.5, mapping rule M-GO).
struct TaskSpawner {
    pool: Option<ExecPool>,
}

impl TaskSpawner {
    // glue: N3 ExecPool or current tokio runtime spawn (M-GO)
    fn spawn<F, R>(&self, name: &'static str, fut: F) -> JoinHandle<R>
    where
        F: Future<Output = R> + Send + 'static,
        R: Send + 'static,
    {
        match &self.pool {
            Some(pool) => pool.spawn(name, fut),
            None => tokio::spawn(fut),
        }
    }
}

/// HashAggExec deals with all the aggregate functions.
/// It is built from the Aggregate Plan. When Next() is called, it reads all the data from Src
/// and updates all the items in PartialAggFuncs.
pub struct HashAggExec {
    pub(crate) base: BaseExecutor,

    pub partial_agg_funcs: Vec<Option<Arc<dyn AggFunc>>>,
    pub final_agg_funcs: Vec<Option<Arc<dyn AggFunc>>>,
    partial_result_map: Option<AggPartialResultMapper<ColAlloc>>,
    group_set: Option<StringSetWithMemoryUsage<ColAlloc>>,
    group_keys: AllocVec<AllocBox<[u8], ColAlloc>, ColAlloc>,
    cursor4_group_key: usize,
    pub group_by_items: Vec<ExprRef>,
    group_key_buffer: Vec<Vec<u8>>,

    // Parallel execution channels and workers (populated in initForParallelExec, moved into
    // tasks in prepare4ParallelExec).
    finish_rx: Option<watch::Receiver<bool>>,
    final_output_tx: Option<mpsc::Sender<AfFinalResult>>,
    final_output_rx: Option<mpsc::Receiver<AfFinalResult>>,
    input_tx: Option<mpsc::Sender<HashAggInput>>,
    input_rx: Option<mpsc::Receiver<HashAggInput>>,
    partial_input_txs: Vec<mpsc::Sender<Chunk>>,
    partial_workers: Vec<HashAggPartialWorker>,
    final_workers: Vec<HashAggFinalWorker>,
    tasks: ParallelTasks,

    pub default_val: Option<Chunk>,
    child_result: Option<Chunk>,

    /// IsChildReturnEmpty indicates whether the child executor only returns an empty input.
    pub is_child_return_empty: bool,
    /// Whether this executor runs single-threaded instead of in parallel.
    pub is_unparallel_exec: bool,
    parallel_exec_valid: bool,
    prepared: bool,
    executed: bool,

    pub(crate) mem_tracker: Arc<Tracker>,
    pub(crate) alloc: TrackedAlloc,
    pub(crate) disk_tracker: Option<Arc<Tracker>>,

    pub(crate) stats: Option<Arc<HashAggRuntimeStats>>,
    runtime_stats_coll: Option<Arc<RuntimeStatsColl>>,

    data_in_disk: Option<DataInDiskByChunks>,
    num_of_spilled_chks: usize,
    offset_of_spilled_chks: usize,
    pub(crate) in_spill_mode: Arc<AtomicU32>,
    tmp_chk_for_spill: Option<Chunk>,
    inflight_chunk_sync: Option<Arc<WaitGroup>>,
    pub(crate) spill_action: Option<Arc<AggSpillDiskAction>>,
    pub(crate) parallel_agg_spill_action: Option<Arc<ParallelAggSpillDiskAction>>,
    pub(crate) spill_helper: Option<Arc<ParallelHashAggSpillHelper>>,
    is_child_drained: bool,

    pub file_name_prefix_for_test: String,

    // Session values captured when built (N2).
    pub(crate) eval_ctx: EvalContextRef,
    pub(crate) enable_vectorized_expression: bool,
    pub(crate) track_aggregate_memory_usage: bool,
    pub(crate) enable_parallel_hashagg_spill: bool,
    pub(crate) partial_concurrency: usize,
    pub(crate) final_concurrency: usize,
    /// Optional session-level memory tracker (`SessionVars.MemTracker` in Go).
    /// None until A1 wires the session tracker into `SessionVars` (spec §6 item 11, §8.2
    /// decision 6); kept `pub(crate)` for tests in this crate only.
    pub(crate) session_mem_tracker: Option<Arc<Tracker>>,

    eval_state: EvalState,
    rpn: RpnState,
}

/// Constructs a `HashAggExec` from its base executor and session values captured at build
/// time (N2). Creates `mem_tracker` and `TrackedAlloc` up front so aggregate functions
/// receive the allocator before being wrapped in `Arc` (A1).
// glue: Go composite literal &aggregate.HashAggExec{...}
#[allow(clippy::too_many_arguments)]
pub fn new_hash_agg_exec(
    base: BaseExecutor,
    group_by_items: Vec<ExprRef>,
    eval_ctx: EvalContextRef,
    enable_vectorized_expression: bool,
    track_aggregate_memory_usage: bool,
    enable_parallel_hashagg_spill: bool,
    partial_concurrency: usize,
    final_concurrency: usize,
) -> HashAggExec {
    let mem_tracker = new_tracker(base.base.meta.id(), -1);
    let alloc = TrackedAlloc::new(Arc::clone(&mem_tracker));
    HashAggExec {
        base,
        partial_agg_funcs: Vec::new(),
        final_agg_funcs: Vec::new(),
        partial_result_map: None,
        group_set: None,
        group_keys: AllocVec::new_in(ColAlloc::Global),
        cursor4_group_key: 0,
        group_by_items,
        group_key_buffer: Vec::new(),
        finish_rx: None,
        final_output_tx: None,
        final_output_rx: None,
        input_tx: None,
        input_rx: None,
        partial_input_txs: Vec::new(),
        partial_workers: Vec::new(),
        final_workers: Vec::new(),
        tasks: ParallelTasks::default(),
        default_val: None,
        child_result: None,
        is_child_return_empty: false,
        is_unparallel_exec: false,
        parallel_exec_valid: false,
        prepared: false,
        executed: false,
        mem_tracker,
        alloc,
        disk_tracker: None,
        stats: None,
        runtime_stats_coll: None,
        data_in_disk: None,
        num_of_spilled_chks: 0,
        offset_of_spilled_chks: 0,
        in_spill_mode: Arc::new(AtomicU32::new(0)),
        tmp_chk_for_spill: None,
        inflight_chunk_sync: None,
        spill_action: None,
        parallel_agg_spill_action: None,
        spill_helper: None,
        is_child_drained: false,
        file_name_prefix_for_test: String::new(),
        eval_ctx,
        enable_vectorized_expression,
        track_aggregate_memory_usage,
        enable_parallel_hashagg_spill,
        partial_concurrency,
        final_concurrency,
        session_mem_tracker: None,
        eval_state: EvalState::default(),
        rpn: RpnState::default(),
    }
}

/// Go's panic when a nil `AggFunc` (from `aggfuncs.Build`) is called, as an error (M-PANIC).
// glue: a method call on Go's nil AggFunc interface
pub(crate) fn nil_agg_func(f: &Option<Arc<dyn AggFunc>>) -> Result<&dyn AggFunc, Error> {
    f.as_deref().ok_or_else(|| {
        Error::plain("runtime error: invalid memory address or nil pointer dereference")
    })
}

/// Copies a key byte slice into a `ColAlloc`-backed boxed slice so its bytes are tracked
/// under A1 `TrackedAlloc`.
// glue: A1 tracked byte-key copy (Go `string(key)`)
pub(crate) fn tracked_key_box(key: &[u8], alloc: ColAlloc) -> AllocBox<[u8], ColAlloc> {
    let mut v = AllocVec::with_capacity_in(key.len(), alloc);
    v.extend_from_slice(key);
    v.into_boxed_slice()
}

/// Drop guard that registers `HashAggRuntimeStats` on every return path of `Close` (spec
/// §4.4, §4.5: Go's `defer RegisterStats(e.ID(), e.stats)`).
struct RegisterStatsGuard {
    id: i64,
    coll: Option<Arc<RuntimeStatsColl>>,
    stats: Option<Arc<HashAggRuntimeStats>>,
}

impl Drop for RegisterStatsGuard {
    // glue: defer e.Ctx().GetSessionVars().StmtCtx.RuntimeStatsColl.RegisterStats(e.ID(), e.stats) (M-DEFER, spec §4.5)
    fn drop(&mut self) {
        if let (Some(coll), Some(stats)) = (self.coll.take(), self.stats.take()) {
            coll.register_stats(self.id, stats.clone_stats());
        }
    }
}

#[async_trait]
impl Executor for HashAggExec {
    // glue: M-SELF
    fn base(&self) -> &BaseExecutorV2 {
        &self.base.base
    }

    // glue: M-SELF
    fn base_mut(&mut self) -> &mut BaseExecutorV2 {
        &mut self.base.base
    }

    /// Close implements the Executor Close interface.
    ///
    /// In parallel mode, signals `finish_tx`, drains `final_output_rx` via `channel::clear`,
    /// awaits all worker/fetcher/helper tasks (taking back the child executor and draining
    /// `input_rx`), flushes `TrackedAlloc`, and closes `BaseExecutor`. Runtime stats are
    /// registered on every exit path via [`RegisterStatsGuard`] (spec §4.4, §4.5).
    // go: HashAggExec.Close L2 (awaits JoinHandles and puts child back; A1 tracked release; no failpoint [M-CH, M-WG])
    async fn close(&mut self) -> Result<(), Error> {
        let _stats_guard = RegisterStatsGuard {
            id: self.base.base.meta.id() as i64,
            coll: self.runtime_stats_coll.clone(),
            stats: self.stats.clone(),
        };

        if self.is_unparallel_exec {
            if let Some(child_result) = self.child_result.take() {
                let _ = self.mem_tracker.consume(-child_result.memory_usage());
            }
            self.drop_unparallel_tracked_state();
            let _ = self.mem_tracker.replace_bytes_used(0);
            if let Some(data_in_disk) = self.data_in_disk.take() {
                data_in_disk.close();
            }
            if let Some(spill_action) = self.spill_action.take() {
                spill_action.set_finished();
            }
            self.tmp_chk_for_spill = None;
            return self.base.base.close().await;
        }

        if self.parallel_exec_valid {
            // `Close` may be called after `Open` without calling `Next`.
            if !self.prepared {
                self.prepared = true;
                self.input_tx = None;
                self.partial_input_txs.clear();
                self.final_output_tx = None;
                self.finish_rx = None;
                // Release chunks stored in unstarted partial workers and inputCh.
                for w in self.partial_workers.drain(..) {
                    let _ = self.mem_tracker.consume(-w.chk.memory_usage());
                }
                self.final_workers.clear();
                if let Some(mut input_rx) = self.input_rx.take() {
                    while let Some(input) = input_rx.recv().await {
                        let _ = self.mem_tracker.consume(-input.chk.memory_usage());
                    }
                }
            }

            if let Some(tx) = &self.tasks.finish_tx {
                tx.send_replace(true);
            }

            // Drain finalOutputCh until waitAllWorkersAndCloseFinalOutputCh closes it.
            if let Some(mut final_rx) = self.final_output_rx.take() {
                channel_clear(&mut final_rx).await;
            }

            // Join all tasks; keep each handle in `self.tasks` until its await finishes so
            // dropping the `Close` future mid-await still lets `ParallelTasks::drop` abort
            // any remaining task (spec §4.4).
            let mut returned_input_rx: Option<mpsc::Receiver<HashAggInput>> = None;
            if let Some(h) = self.tasks.fetcher.as_mut() {
                match h.await {
                    Ok((child, input_rx)) => {
                        let mut children = self.base.base.meta.take_all_children();
                        children.insert(0, child);
                        self.base.base.meta.set_all_children(children);
                        returned_input_rx = Some(input_rx);
                    }
                    Err(join_err) => {
                        log::error!("HashAggExec fetcher task failed to join: {join_err}");
                    }
                }
                self.tasks.fetcher = None;
            }
            while let Some(h) = self.tasks.partial_workers.first_mut() {
                let _ = h.await;
                self.tasks.partial_workers.remove(0);
            }
            while let Some(h) = self.tasks.final_workers.first_mut() {
                let _ = h.await;
                self.tasks.final_workers.remove(0);
            }
            while let Some(h) = self.tasks.aux_tasks.first_mut() {
                let _ = h.await;
                self.tasks.aux_tasks.remove(0);
            }
            self.tasks.finish_tx = None;

            // Drain remaining chunks from inputCh after all partial workers have exited
            // (spec §4.3 item 2).
            if let Some(mut input_rx) = returned_input_rx {
                while let Some(input) = input_rx.recv().await {
                    let _ = self.mem_tracker.consume(-input.chk.memory_usage());
                }
            }

            self.alloc.flush();

            self.executed = false;
            let consumed = self.mem_tracker.bytes_consumed();
            if consumed < 0 {
                log::warn!("Memory tracker's counter is invalid, counter={consumed}");
            }
            let _ = self.mem_tracker.replace_bytes_used(0);
            self.parallel_exec_valid = false;
            if let Some(action) = self.parallel_agg_spill_action.take() {
                action.set_finished();
            }
            if let Some(helper) = self.spill_helper.take() {
                helper.close();
            }
        }

        self.base.base.close().await
    }

    /// Open implements the Executor Open interface.
    // go: HashAggExec.Open L2 (no failpoint)
    async fn open(&mut self, ctx: &ExecCtx) -> Result<(), Error> {
        self.base.base.open(ctx).await?;
        self.open_self(ctx)
    }

    /// Next implements the Executor Next interface.
    // go: HashAggExec.Next L1
    async fn next(&mut self, ctx: &ExecCtx, req: &mut Chunk) -> Result<(), Error> {
        req.reset();
        if self.is_unparallel_exec {
            return self.unparallel_exec(ctx, req).await;
        }
        self.parallel_exec(ctx, req).await
    }
}

impl HashAggExec {
    /// Installs `TrackedAlloc` on `agg_func` before wrapping it in `Arc` and appending to
    /// `partial_agg_funcs` (A1: avoids relying on `Arc::get_mut` after sharing).
    // glue: A1 TrackedAlloc wiring before Arc::from
    pub fn push_partial_agg_func(&mut self, mut agg_func: Option<Box<dyn AggFunc>>) {
        if let Some(f) = &mut agg_func {
            f.set_alloc(ColAlloc::Tracked(self.alloc.clone()));
        }
        self.partial_agg_funcs.push(agg_func.map(Arc::from));
    }

    /// Installs `TrackedAlloc` on `agg_func` before wrapping it in `Arc` and appending to
    /// `final_agg_funcs` (A1: avoids relying on `Arc::get_mut` after sharing).
    // glue: A1 TrackedAlloc wiring before Arc::from
    pub fn push_final_agg_func(&mut self, mut agg_func: Option<Box<dyn AggFunc>>) {
        if let Some(f) = &mut agg_func {
            f.set_alloc(ColAlloc::Tracked(self.alloc.clone()));
        }
        self.final_agg_funcs.push(agg_func.map(Arc::from));
    }

    /// OpenSelf just opens the hash aggregation executor.
    // go: HashAggExec.OpenSelf L2 (A1: tracker and TrackedAlloc set up at construction; resets tracker before init)
    pub fn open_self(&mut self, ctx: &ExecCtx) -> Result<(), Error> {
        if self.base.base.meta.children_len() == 0 {
            return Err(Error::plain(
                "HashAggExec child executor missing after aborted task",
            ));
        }
        self.prepared = false;
        self.runtime_stats_coll = ctx.stmt.runtime_stats_coll.clone();

        // Drop any tracked state left by an earlier Open without Close before resetting the
        // tracker, so TrackedAlloc does not later subtract already-reset bytes.
        self.drop_unparallel_tracked_state();
        if let Some(child_result) = self.child_result.take() {
            let _ = self.mem_tracker.consume(-child_result.memory_usage());
        }

        self.mem_tracker.reset()?;

        if self.track_aggregate_memory_usage
            && let Some(parent) = &ctx.stmt.mem_tracker
        {
            self.mem_tracker.attach_to(parent)?;
        }

        if self.is_unparallel_exec {
            return self.init_for_unparallel_exec(ctx);
        }
        self.init_for_parallel_exec(ctx)
    }

    /// Returns the `ColAlloc` tied to `self.mem_tracker`.
    // glue: A1 TrackedAlloc accessor
    fn col_alloc(&self) -> ColAlloc {
        ColAlloc::Tracked(self.alloc.clone())
    }

    /// Drops unparallel `TrackedAlloc`-backed containers and flushes the pending counter so
    /// every released byte is recorded in `mem_tracker` before reset/close.
    // glue: A1 release of unparallel tracked containers
    fn drop_unparallel_tracked_state(&mut self) {
        self.group_keys = AllocVec::new_in(ColAlloc::Global);
        self.group_set = None;
        self.partial_result_map = None;
        self.alloc.flush();
    }

    /// Flushes `TrackedAlloc` and runs the tracker's action checkpoint (Go's positive
    /// `Consume`).
    // glue: A1 checkpoint at Go's positive Consume sites
    fn checkpoint_mem(&self) -> Result<(), Error> {
        self.alloc.flush();
        self.mem_tracker.checkpoint()
    }

    // go: HashAggExec.initForUnparallelExec L2 (A1: TrackedAlloc containers; session tracker optional per spec §8.2 decision 6; no failpoint)
    fn init_for_unparallel_exec(&mut self, ctx: &ExecCtx) -> Result<(), Error> {
        let alloc = self.col_alloc();
        let (group_set, _) = new_string_set_with_memory_usage(alloc.clone());
        self.group_set = Some(group_set);
        self.partial_result_map = Some(new_agg_partial_result_mapper_in(alloc.clone(), 0));
        self.group_keys = AllocVec::new_in(alloc);
        self.cursor4_group_key = 0;
        self.checkpoint_mem()?;

        self.group_key_buffer = Vec::with_capacity(8);
        let child_result = exec::try_new_cache_chunk(self.base.base.meta.children(0).as_ref());
        let child_mem = child_result.memory_usage();
        self.child_result = Some(child_result);
        self.mem_tracker.consume(child_mem)?;

        self.offset_of_spilled_chks = 0;
        self.num_of_spilled_chks = 0;
        self.executed = false;
        self.is_child_drained = false;
        self.in_spill_mode.store(0, Ordering::SeqCst);

        let (data_in_disk, tmp_chk_for_spill) = {
            let child = self.base.base.meta.children(0);
            let child_ret_types = exec::ret_types(child.as_ref());
            (
                new_data_in_disk_by_chunks(child_ret_types, self.file_name_prefix_for_test.clone()),
                exec::try_new_cache_chunk(child.as_ref()),
            )
        };
        self.tmp_chk_for_spill = Some(tmp_chk_for_spill);

        if self.track_aggregate_memory_usage && EnableTmpStorageOnOOM.load(Ordering::SeqCst) {
            let disk_tracker = match &self.disk_tracker {
                Some(dt) => {
                    dt.reset()?;
                    Arc::clone(dt)
                }
                None => {
                    let dt = new_tracker(self.base.base.meta.id(), -1);
                    self.disk_tracker = Some(Arc::clone(&dt));
                    dt
                }
            };
            if let Some(stmt_disk) = &ctx.stmt.disk_tracker {
                disk_tracker.attach_to(stmt_disk)?;
            }
            data_in_disk.get_disk_tracker().attach_to(&disk_tracker)?;
            if let Some(session_tracker) = self.session_mem_tracker.clone() {
                let action = self.action_spill();
                session_tracker.fallback_old_and_set_new_action_for_soft_limit(Some(action));
            }
            // Otherwise: the Rust session does not yet carry SessionVars.MemTracker (A1
            // follow-up, spec §6 item 11 and §8.2 decision 6); no soft-limit action is
            // registered until A1 wires it into the session.
        }
        self.data_in_disk = Some(data_in_disk);
        Ok(())
    }

    // go: HashAggExec.initPartialWorkers L2 (A1 TrackedAlloc; mpsc channels; no failpoint [M-CH])
    fn init_partial_workers(
        &mut self,
        partial_concurrency: usize,
        final_concurrency: usize,
        partial_input_rxs: Vec<mpsc::Receiver<Chunk>>,
        partial_output_txs: &[mpsc::Sender<AggPartialResultMapper<ColAlloc>>],
    ) -> Result<(), Error> {
        let mut mem_usage: i64 = 0;
        let mem_tracker = Arc::clone(&self.mem_tracker);
        let alloc = self.alloc.clone();
        let finish_rx = self
            .finish_rx
            .clone()
            .expect("initForParallelExec initialized finishCh");
        let final_output_tx = self
            .final_output_tx
            .clone()
            .expect("initForParallelExec initialized finalOutputCh");
        let input_tx = self
            .input_tx
            .clone()
            .expect("initForParallelExec initialized inputCh");
        let spill_helper = Arc::clone(
            self.spill_helper
                .as_ref()
                .expect("initForParallelExec initialized spillHelper"),
        );
        let inflight_chunk_sync = Arc::clone(
            self.inflight_chunk_sync
                .as_ref()
                .expect("initForParallelExec initialized inflightChunkSync"),
        );
        let child_ret_types: Vec<_> = {
            let child = self.base.base.meta.children(0);
            exec::ret_types(child.as_ref()).to_vec()
        };
        let max_chunk_size = self.base.base.alloc.max_chunk_size() as usize;

        self.partial_workers = Vec::with_capacity(partial_concurrency);
        let mut worker_stats = Vec::new();

        for (i, input_rx) in partial_input_rxs.into_iter().enumerate() {
            let mut partial_results_map = Vec::with_capacity(final_concurrency);
            for _ in 0..final_concurrency {
                partial_results_map.push(new_agg_partial_result_mapper_in(
                    ColAlloc::Tracked(alloc.clone()),
                    0,
                ));
            }
            let (partial_results_buffer, group_key_buf) = get_buffer();
            let mut base_worker = new_base_hash_agg_worker(
                finish_rx.clone(),
                self.partial_agg_funcs.clone(),
                max_chunk_size,
                Arc::clone(&mem_tracker),
                alloc.clone(),
            );
            if self.stats.is_some() {
                let st = Arc::new(AggWorkerStat::default());
                base_worker.stats = Some(Arc::clone(&st));
                worker_stats.push(st);
            }
            let partial_result_num_in_row =
                base_worker.get_partial_result_slice_len_consider_byte_align();
            let chk = self.base.base.alloc.new_chunk_with_capacity(
                &child_ret_types,
                0,
                max_chunk_size as isize,
            );
            mem_usage += chk.memory_usage();

            let worker = HashAggPartialWorker {
                base: base_worker,
                worker_id: i,
                eval_ctx: Arc::clone(&self.eval_ctx),
                enable_vectorized_expression: self.enable_vectorized_expression,
                input_rx,
                output_txs: partial_output_txs.to_vec(),
                global_output_tx: final_output_tx.clone(),
                give_back_tx: input_tx.clone(),
                partial_results_buffer,
                partial_result_num_in_row,
                partial_results_map,
                partial_results_map_mem: AtomicI64::new(0),
                group_by_items: self.group_by_items.clone(),
                group_key_buf,
                chk,
                spill_helper: Arc::clone(&spill_helper),
                spilled_chunks_io: Vec::new(),
                inflight_chunk_sync: Arc::clone(&inflight_chunk_sync),
                eval_state: EvalState::default(),
                rpn: RpnState::default(),
            };
            self.partial_workers.push(worker);

            self.checkpoint_mem()?;

            let input_chk = chunk::new(&child_ret_types, 0, max_chunk_size);
            mem_usage += input_chk.memory_usage();
            let input = HashAggInput {
                chk: input_chk,
                worker_idx: i,
            };
            // Channel capacity equals partial_concurrency, so try_send never blocks.
            input_tx
                .try_send(input)
                .expect("inputCh has capacity partial_concurrency");
        }

        if let Some(stats) = self.stats.as_mut().and_then(Arc::get_mut) {
            stats.partial_stats = worker_stats;
        }
        mem_tracker.consume(mem_usage)
    }

    // go: HashAggExec.initFinalWorkers L2 (A1 TrackedAlloc; mpsc channels [M-CH])
    fn init_final_workers(
        &mut self,
        final_concurrency: usize,
        partial_output_rxs: Vec<mpsc::Receiver<AggPartialResultMapper<ColAlloc>>>,
    ) -> Result<(), Error> {
        let mem_tracker = Arc::clone(&self.mem_tracker);
        let alloc = self.alloc.clone();
        let finish_rx = self
            .finish_rx
            .clone()
            .expect("initForParallelExec initialized finishCh");
        let final_output_tx = self
            .final_output_tx
            .clone()
            .expect("initForParallelExec initialized finalOutputCh");
        let spill_helper = Arc::clone(
            self.spill_helper
                .as_ref()
                .expect("initForParallelExec initialized spillHelper"),
        );
        let ret_types = exec::ret_types(self).to_vec();
        let max_chunk_size = self.base.base.alloc.max_chunk_size() as usize;

        self.final_workers = Vec::with_capacity(final_concurrency);
        let mut worker_stats = Vec::new();

        for input_rx in partial_output_rxs {
            let mut base_worker = new_base_hash_agg_worker(
                finish_rx.clone(),
                self.final_agg_funcs.clone(),
                max_chunk_size,
                Arc::clone(&mem_tracker),
                alloc.clone(),
            );
            if self.stats.is_some() {
                let st = Arc::new(AggWorkerStat::default());
                base_worker.stats = Some(Arc::clone(&st));
                worker_stats.push(st);
            }
            let (holder_tx, holder_rx) = mpsc::channel(1);
            holder_tx
                .try_send(chunk::new(&ret_types, 0, max_chunk_size))
                .expect("finalResultHolderCh has capacity 1");

            let worker = HashAggFinalWorker {
                base: base_worker,
                partial_result_map: new_agg_partial_result_mapper_in(
                    ColAlloc::Tracked(alloc.clone()),
                    0,
                ),
                input_rx,
                output_tx: final_output_tx.clone(),
                final_result_holder_rx: holder_rx,
                final_result_holder_tx: holder_tx,
                spill_helper: Arc::clone(&spill_helper),
                eval_ctx: Arc::clone(&self.eval_ctx),
                sql_killer: self.base.base.killer.clone(),
            };
            self.final_workers.push(worker);
            self.checkpoint_mem()?;
        }

        if let Some(stats) = self.stats.as_mut().and_then(Arc::get_mut) {
            stats.final_stats = worker_stats;
        }
        Ok(())
    }

    // go: HashAggExec.initForParallelExec L2 (N2 session values; watch finishCh + mpsc; session tracker optional per spec §8.2 decision 6)
    fn init_for_parallel_exec(&mut self, ctx: &ExecCtx) -> Result<(), Error> {
        let partial_concurrency = self.partial_concurrency;
        let final_concurrency = self.final_concurrency;
        if partial_concurrency == 0 || final_concurrency == 0 {
            return Err(Error::plain("partialConcurrency or finalConcurrency is 0"));
        }

        self.is_child_return_empty = true;
        let (final_output_tx, final_output_rx) =
            mpsc::channel(final_concurrency + partial_concurrency + 1);
        let (input_tx, input_rx) = mpsc::channel(partial_concurrency);
        let (finish_tx, finish_rx) = watch::channel(false);

        self.final_output_tx = Some(final_output_tx);
        self.final_output_rx = Some(final_output_rx);
        self.input_tx = Some(input_tx);
        self.input_rx = Some(input_rx);
        self.tasks.finish_tx = Some(finish_tx);
        self.finish_rx = Some(finish_rx);

        let mut partial_input_txs = Vec::with_capacity(partial_concurrency);
        let mut partial_input_rxs = Vec::with_capacity(partial_concurrency);
        for _ in 0..partial_concurrency {
            let (tx, rx) = mpsc::channel(1);
            partial_input_txs.push(tx);
            partial_input_rxs.push(rx);
        }
        self.partial_input_txs = partial_input_txs;

        let mut partial_output_txs = Vec::with_capacity(final_concurrency);
        let mut partial_output_rxs = Vec::with_capacity(final_concurrency);
        for _ in 0..final_concurrency {
            let (tx, rx) = mpsc::channel(partial_concurrency);
            partial_output_txs.push(tx);
            partial_output_rxs.push(rx);
        }

        self.inflight_chunk_sync = Some(Arc::new(WaitGroup::new()));

        let is_tracker_enabled =
            self.track_aggregate_memory_usage && EnableTmpStorageOnOOM.load(Ordering::SeqCst);
        let is_parallel_hash_agg_spill_enabled = self.enable_parallel_hashagg_spill;

        let base_ret_type_num = exec::ret_types(self).len();
        // Intermediate result for aggregate functions plus group key column.
        let mut spill_chunk_field_types = Vec::with_capacity(base_ret_type_num + 1);
        for _ in 0..base_ret_type_num {
            spill_chunk_field_types.push(new_field_type(mysql::TypeVarString));
        }
        spill_chunk_field_types.push(new_field_type(mysql::TypeString));

        let init_cap = self.base.base.alloc.init_cap() as usize;
        let max_chunk_size = self.base.base.alloc.max_chunk_size() as usize;
        let fts_for_closure = spill_chunk_field_types.clone();
        let mem_tracker = Arc::clone(&self.mem_tracker);
        let spill_helper = new_spill_helper(
            mem_tracker,
            self.partial_agg_funcs.clone(),
            self.final_agg_funcs.clone(),
            Box::new(move || chunk::new(&fts_for_closure, init_cap, max_chunk_size)),
            spill_chunk_field_types,
        )?;
        let spill_helper = Arc::new(spill_helper);
        self.spill_helper = Some(Arc::clone(&spill_helper));

        if is_tracker_enabled && is_parallel_hash_agg_spill_enabled {
            let disk_tracker = match &self.disk_tracker {
                Some(dt) => {
                    dt.reset()?;
                    Arc::clone(dt)
                }
                None => {
                    let dt = new_tracker(self.base.base.meta.id(), -1);
                    self.disk_tracker = Some(Arc::clone(&dt));
                    dt
                }
            };
            if let Some(stmt_disk) = &ctx.stmt.disk_tracker {
                disk_tracker.attach_to(stmt_disk)?;
            }
            spill_helper.set_disk_tracker(disk_tracker);
            if let Some(session_tracker) = self.session_mem_tracker.clone() {
                let action = self.action_spill();
                session_tracker.fallback_old_and_set_new_action_for_soft_limit(Some(action));
            }
            // Otherwise: the Rust session does not yet carry SessionVars.MemTracker (A1
            // follow-up, spec §6 item 11 and §8.2 decision 6).
        }

        self.init_runtime_stats();
        self.init_partial_workers(
            partial_concurrency,
            final_concurrency,
            partial_input_rxs,
            &partial_output_txs,
        )?;
        self.init_final_workers(final_concurrency, partial_output_rxs)?;
        self.parallel_exec_valid = true;
        self.executed = false;
        Ok(())
    }

    /// Reads chunks from the child executor and dispatches them to partial workers.
    /// Returns `(child, input_rx)` so `Close` puts the child back and drains `input_rx`
    /// after partial workers exit (spec §4.3, §4.5).
    // go: HashAggExec.fetchChildData L2 (async task; takes and returns child + input_rx; ordered cleanup matching Go defer; no failpoint)
    #[allow(clippy::too_many_arguments)]
    async fn fetch_child_data(
        ctx: ExecCtx,
        mut child: Box<dyn Executor>,
        mut input_rx: mpsc::Receiver<HashAggInput>,
        mut partial_input_txs: Vec<mpsc::Sender<Chunk>>,
        mut finish_rx: watch::Receiver<bool>,
        final_output_tx: mpsc::Sender<AfFinalResult>,
        mem_tracker: Arc<Tracker>,
        inflight_chunk_sync: Arc<WaitGroup>,
        spill_helper: Arc<ParallelHashAggSpillHelper>,
        wait_group: Arc<WaitGroup>,
    ) -> (Box<dyn Executor>, mpsc::Receiver<HashAggInput>) {
        let wg_guard = DoneGuard(wait_group);

        let res = tidb_base::panic_guard::catch_unwind_future(async {
            loop {
                let input = tokio::select! {
                    _ = wait_finish(&mut finish_rx) => return,
                    res = input_rx.recv() => match res {
                        Some(inp) => inp,
                        None => return,
                    },
                };
                let mut chk = input.chk;
                let worker_idx = input.worker_idx;

                let m_size = chk.memory_usage();
                if let Err(err) = exec::next(&ctx, child.as_mut(), &mut chk).await {
                    tokio::select! {
                        _ = wait_finish(&mut finish_rx) => {}
                        _ = final_output_tx.send(AfFinalResult::err(err)) => {}
                    }
                    let _ = mem_tracker.consume(-m_size);
                    return;
                }

                if chk.num_rows() == 0 {
                    let _ = mem_tracker.consume(-m_size);
                    return;
                }

                let new_size = chk.memory_usage();
                if let Err(err) = mem_tracker.consume(new_size - m_size) {
                    // Quota action error in task is treated like Go's panic (spec §4.5).
                    recovery_hash_agg(&final_output_tx, Box::new(err)).await;
                    let _ = mem_tracker.consume(-new_size);
                    return;
                }

                inflight_chunk_sync.add(1);
                let sent = tokio::select! {
                    _ = wait_finish(&mut finish_rx) => false,
                    res = partial_input_txs[worker_idx].send(chk) => res.is_ok(),
                };
                if !sent {
                    inflight_chunk_sync.done();
                    let _ = mem_tracker.consume(-new_size);
                    return;
                }

                if Self::spill_if_need(&inflight_chunk_sync, &spill_helper, &final_output_tx).await
                {
                    return;
                }
            }
        })
        .await;

        // Ordered cleanup matching Go's defer (spec §4.5):
        // 1. Recover panic -> send to finalOutputCh.
        if let Err((payload, _)) = res {
            recovery_hash_agg(&final_output_tx, payload).await;
        }
        // 2. Wait for all in-flight chunks to finish in partial workers.
        inflight_chunk_sync.wait().await;
        // 3. Spill remaining data if spill was triggered without error (blocked-by:D01.10).
        if !spill_helper.is_no_spill() && !spill_helper.check_error() {
            Self::spill(&spill_helper, &final_output_tx).await;
        }
        // 4. Close all partialInputChs by dropping their senders.
        partial_input_txs.clear();
        // 5. waitGroup.Done().
        drop(wg_guard);

        (child, input_rx)
    }

    // go: HashAggExec.spillIfNeed L2 (async wait on inflightChunkSync; calls spill stub)
    async fn spill_if_need(
        inflight_chunk_sync: &WaitGroup,
        spill_helper: &ParallelHashAggSpillHelper,
        final_output_tx: &mpsc::Sender<AfFinalResult>,
    ) -> bool {
        if spill_helper.check_error() {
            return true;
        }

        if !spill_helper.check_need_spill() {
            return false;
        }

        // Wait for the finish of all partial workers.
        inflight_chunk_sync.wait().await;
        Self::spill(spill_helper, final_output_tx).await;
        // In Part B, `spill` always fails with blocked-by:D01.10 and sets error; stop the
        // fetcher immediately instead of reading another chunk.
        spill_helper.check_error()
    }

    /// Parallel spill-to-disk entry point: whole-function stub returning `blocked-by:D01.10`
    /// along Go's `worker.processError` path (`finalOutputCh` + `spillHelper.setError`, spec
    /// §3 and §8.2 decision 5).
    // glue: stub of HashAggExec.spill, blocked-by:D01.10 (spilling parallel hash aggregation partial results to disk)
    async fn spill(
        spill_helper: &ParallelHashAggSpillHelper,
        final_output_tx: &mpsc::Sender<AfFinalResult>,
    ) {
        let err = blocked(
            "D01.10",
            "spilling parallel hash aggregation partial results to disk (HashAggExec.spill)",
        );
        spill_helper.set_error();
        let _ = final_output_tx.send(AfFinalResult::err(err)).await;
    }

    /// Waits for all partial workers to finish (their `output_txs` drop with them, closing
    /// `partialOutputChs`; remaining `inputCh` chunks are drained once the fetcher returns
    /// `input_rx` in `Close`, spec §4.3 item 2).
    // go: HashAggExec.waitPartialWorkerAndCloseOutputChs L2 (partialOutputChs close on sender drop; inputCh drained in Close after fetcher returns input_rx [M-CH, M-WG])
    async fn wait_partial_worker_and_close_output_chs(wait_group: Arc<WaitGroup>) {
        wait_group.wait().await;
    }

    /// Waits for fetcher, partial, and final workers, then drops the final sender of
    /// `finalOutputCh` so `parallelExec` / `Close` observes channel closure.
    // go: HashAggExec.waitAllWorkersAndCloseFinalOutputCh L2 (drops final_output_tx to close mpsc channel [M-CH, M-WG])
    async fn wait_all_workers_and_close_final_output_ch(
        wait_groups: Vec<Arc<WaitGroup>>,
        final_output_tx: mpsc::Sender<AfFinalResult>,
    ) {
        for wg in &wait_groups {
            wg.wait().await;
        }
        drop(final_output_tx);
    }

    // go: HashAggExec.prepare4ParallelExec L2 (spawns tasks on ExecCtx.pool / current runtime; saves JoinHandles for Close/Drop [M-GO, M-WG])
    fn prepare4_parallel_exec(&mut self, ctx: &ExecCtx) {
        let spawner = TaskSpawner {
            pool: ctx.pool.clone(),
        };

        let fetch_child_worker_wg = Arc::new(WaitGroup::new());
        fetch_child_worker_wg.add(1);

        let mut children = self.base.base.meta.take_all_children();
        let child = children.remove(0);
        self.base.base.meta.set_all_children(children);

        let input_rx = self
            .input_rx
            .take()
            .expect("initForParallelExec initialized inputCh");
        // Drop HashAggExec's own sender so inputCh closes once all partial workers finish.
        self.input_tx = None;

        let partial_input_txs = std::mem::take(&mut self.partial_input_txs);
        let finish_rx = self
            .finish_rx
            .take()
            .expect("initForParallelExec initialized finishCh");
        let final_output_tx = self
            .final_output_tx
            .take()
            .expect("initForParallelExec initialized finalOutputCh");
        let mem_tracker = Arc::clone(&self.mem_tracker);
        let inflight_chunk_sync = Arc::clone(
            self.inflight_chunk_sync
                .as_ref()
                .expect("initForParallelExec initialized inflightChunkSync"),
        );
        let spill_helper = Arc::clone(
            self.spill_helper
                .as_ref()
                .expect("initForParallelExec initialized spillHelper"),
        );

        let fetcher_handle = spawner.spawn(
            "hash agg fetcher",
            Self::fetch_child_data(
                ctx.clone(),
                child,
                input_rx,
                partial_input_txs,
                finish_rx,
                final_output_tx.clone(),
                mem_tracker,
                inflight_chunk_sync,
                spill_helper,
                Arc::clone(&fetch_child_worker_wg),
            ),
        );
        self.tasks.fetcher = Some(fetcher_handle);

        let partial_workers = std::mem::take(&mut self.partial_workers);
        let final_workers = std::mem::take(&mut self.final_workers);
        let final_concurrency = final_workers.len();

        let partial_worker_wg = Arc::new(WaitGroup::new());
        partial_worker_wg.add(partial_workers.len() as i64);
        let partial_start = Instant::now();
        for worker in partial_workers {
            let h = spawner.spawn(
                "hash agg partial worker",
                worker.run(
                    ctx.clone(),
                    Arc::clone(&partial_worker_wg),
                    final_concurrency,
                ),
            );
            self.tasks.partial_workers.push(h);
        }

        let stats_for_partial = self.stats.clone();
        let partial_wg_for_aux = Arc::clone(&partial_worker_wg);
        self.tasks
            .aux_tasks
            .push(spawner.spawn("hash agg partial waiter", async move {
                Self::wait_partial_worker_and_close_output_chs(partial_wg_for_aux).await;
                if let Some(stats) = stats_for_partial {
                    stats
                        .partial_wall_time
                        .fetch_add(partial_start.elapsed().as_nanos() as i64, Ordering::SeqCst);
                }
            }));

        let final_worker_wg = Arc::new(WaitGroup::new());
        final_worker_wg.add(final_workers.len() as i64);
        let final_start = Instant::now();
        for worker in final_workers {
            let h = spawner.spawn(
                "hash agg final worker",
                worker.run(Arc::clone(&final_worker_wg), Arc::clone(&partial_worker_wg)),
            );
            self.tasks.final_workers.push(h);
        }

        let stats_for_final = self.stats.clone();
        let final_wg_for_aux = Arc::clone(&final_worker_wg);
        self.tasks
            .aux_tasks
            .push(spawner.spawn("hash agg final waiter", async move {
                final_wg_for_aux.wait().await;
                if let Some(stats) = stats_for_final {
                    stats
                        .final_wall_time
                        .fetch_add(final_start.elapsed().as_nanos() as i64, Ordering::SeqCst);
                }
            }));

        // All workers may send errors to finalOutputCh; close it after every task exits.
        self.tasks.aux_tasks.push(spawner.spawn(
            "hash agg close final output",
            Self::wait_all_workers_and_close_final_output_ch(
                vec![fetch_child_worker_wg, partial_worker_wg, final_worker_wg],
                final_output_tx,
            ),
        ));
    }

    /// Executes hash aggregation in parallel using one fetcher, P partial workers, and F
    /// final workers.
    // go: HashAggExec.parallelExec L2 (async mpsc recv; no failpoint)
    async fn parallel_exec(&mut self, ctx: &ExecCtx, chk: &mut Chunk) -> Result<(), Error> {
        if !self.prepared {
            self.prepared = true;
            self.prepare4_parallel_exec(ctx);
        }

        if self.executed {
            return Ok(());
        }

        loop {
            let result = self
                .final_output_rx
                .as_mut()
                .expect("initForParallelExec initialized finalOutputCh")
                .recv()
                .await;
            let Some(result) = result else {
                self.executed = true;
                if self.is_child_return_empty
                    && let Some(default_val) = &self.default_val
                {
                    chk.append(default_val, 0, 1);
                }
                return Ok(());
            };
            if let Some(err) = result.err {
                return Err(err);
            }
            let mut result_chk = result.chk.expect("non-error AfFinalResult carries a chunk");
            chk.swap_columns(&mut result_chk);
            result_chk.reset();

            // Return the chunk so the final worker can reuse it.
            if let Some(give_back_tx) = result.give_back_tx {
                let _ = give_back_tx.send(result_chk).await;
            }
            if chk.num_rows() > 0 {
                self.is_child_return_empty = false;
                return Ok(());
            }
        }
    }

    /// unparallelExec executes hash aggregation algorithm in single thread.
    // go: HashAggExec.unparallelExec L2 (A1: TrackedAlloc checkpoint instead of memDelta Consume)
    async fn unparallel_exec(&mut self, ctx: &ExecCtx, chk: &mut Chunk) -> Result<(), Error> {
        chk.reset();
        loop {
            if self.prepared {
                // Since we return e.MaxChunkSize() rows every time, we should not traverse
                // `groupSet` because of its randomness.
                while self.cursor4_group_key < self.group_keys.len() {
                    let map = self
                        .partial_result_map
                        .as_mut()
                        .expect("Open initialized partialResultMap");
                    let partial_results = Self::get_partial_results_in(
                        map,
                        &self.partial_agg_funcs,
                        &self.alloc,
                        &self.mem_tracker,
                        self.group_keys[self.cursor4_group_key].as_ref(),
                    )?;
                    if self.partial_agg_funcs.is_empty() {
                        chk.set_num_virtual_rows(chk.num_rows() + 1);
                    }
                    let eval_ctx = &*self.eval_ctx;
                    for (i, af) in self.partial_agg_funcs.iter().enumerate() {
                        nil_agg_func(af)?.append_final_result2_chunk(
                            eval_ctx,
                            &mut partial_results[i],
                            chk,
                        )?;
                    }
                    if chk.is_full() {
                        self.cursor4_group_key += 1;
                        return Ok(());
                    }
                    self.cursor4_group_key += 1;
                }
                self.reset_spill_mode();
            }
            if self.executed {
                return Ok(());
            }
            self.execute(ctx).await?;
            let group_count = self
                .group_set
                .as_ref()
                .expect("Open initialized groupSet")
                .count();
            if group_count == 0 && self.group_by_items.is_empty() {
                // If no groupby and no data, we should add an empty group.
                // For example:
                // "select count(c) from t;" should return one row [0]
                // "select count(c) from t group by c1;" should return empty result set.
                let alloc = self.col_alloc();
                self.group_set
                    .as_mut()
                    .expect("Open initialized groupSet")
                    .insert(b"");
                self.group_keys.push(tracked_key_box(b"", alloc));
                self.checkpoint_mem()?;
            }
            self.prepared = true;
        }
    }

    // go: HashAggExec.resetSpillMode L2 (A1: drops old tracked containers and flushes without OOM checkpoint, matching Go ReplaceBytesUsed)
    fn reset_spill_mode(&mut self) {
        self.cursor4_group_key = 0;
        self.drop_unparallel_tracked_state();
        let alloc = self.col_alloc();
        let (group_set, _) = new_string_set_with_memory_usage(alloc.clone());
        self.group_set = Some(group_set);
        self.partial_result_map = Some(new_agg_partial_result_mapper_in(alloc.clone(), 0));
        self.group_keys = AllocVec::new_in(alloc);
        self.prepared = false;
        let num_chunks = self
            .data_in_disk
            .as_ref()
            .expect("Open initialized dataInDisk")
            .num_chunks();
        // No data is spilling again, all data have been processed.
        self.executed = self.num_of_spilled_chks == num_chunks;
        self.num_of_spilled_chks = num_chunks;
        self.alloc.flush();
        self.in_spill_mode.store(0, Ordering::SeqCst);
    }

    /// execute fetches Chunks from src and updates each aggregate function for each row in Chunk.
    // go: HashAggExec.execute L2 (A1: TrackedAlloc checkpoint; defer flush of tmpChkForSpill on Ok; no failpoints)
    async fn execute(&mut self, ctx: &ExecCtx) -> Result<(), Error> {
        let mut res = self.execute_loop(ctx).await;
        if res.is_ok()
            && let Some(tmp) = &mut self.tmp_chk_for_spill
            && tmp.num_rows() > 0
        {
            let data_in_disk = self
                .data_in_disk
                .as_ref()
                .expect("Open initialized dataInDisk");
            res = data_in_disk.add(tmp);
            tmp.reset();
        }
        res
    }

    // glue: body of HashAggExec.execute so the deferred spill flush runs on every Ok return
    async fn execute_loop(&mut self, ctx: &ExecCtx) -> Result<(), Error> {
        loop {
            let m_size = self
                .child_result
                .as_ref()
                .expect(CHILD_RESULT)
                .memory_usage();
            self.get_next_chunk(ctx).await?;
            let new_size = self
                .child_result
                .as_ref()
                .expect(CHILD_RESULT)
                .memory_usage();
            self.mem_tracker.consume(new_size - m_size)?;

            let num_rows = self.child_result.as_ref().expect(CHILD_RESULT).num_rows();
            // no more data.
            if num_rows == 0 {
                return Ok(());
            }

            let buf = std::mem::take(&mut self.group_key_buffer);
            let child_ref = self.child_result.as_ref().expect(CHILD_RESULT);
            let loc = ctx.stmt.time_zone();
            let err_ctx = ctx.stmt.err_ctx();
            self.group_key_buffer = get_group_key(
                &*self.eval_ctx,
                &err_ctx,
                &loc,
                self.enable_vectorized_expression,
                &self.eval_state,
                &mut self.rpn,
                child_ref,
                buf,
                &self.group_by_items,
            )?;

            let mut sel: Vec<usize> = Vec::with_capacity(num_rows);
            let group_set = self.group_set.as_mut().expect("Open initialized groupSet");
            let partial_result_map = self
                .partial_result_map
                .as_mut()
                .expect("Open initialized partialResultMap");
            let child_ref = self.child_result.as_ref().expect(CHILD_RESULT);
            let eval_ctx = &*self.eval_ctx;
            for j in 0..num_rows {
                let group_key = self.group_key_buffer[j].as_slice();
                if !group_set.exist(group_key) {
                    if self.in_spill_mode.load(Ordering::SeqCst) == 1 && group_set.count() > 0 {
                        sel.push(j);
                        continue;
                    }
                    group_set.insert(group_key);
                    self.group_keys.push(tracked_key_box(
                        group_key,
                        ColAlloc::Tracked(self.alloc.clone()),
                    ));
                }
                let partial_results = Self::get_partial_results_in(
                    partial_result_map,
                    &self.partial_agg_funcs,
                    &self.alloc,
                    &self.mem_tracker,
                    group_key,
                )?;
                let tmp_buf = [child_ref.get_row(j)];
                for (i, af) in self.partial_agg_funcs.iter().enumerate() {
                    nil_agg_func(af)?.update_partial_result(
                        eval_ctx,
                        &self.eval_state,
                        &tmp_buf,
                        &mut partial_results[i],
                    )?;
                }
            }

            // spill unprocessed data when exceeded.
            if !sel.is_empty() {
                let is_full_chk = sel.len() == sel.capacity();
                self.child_result
                    .as_mut()
                    .expect(CHILD_RESULT)
                    .set_sel(Some(sel));
                self.spill_unprocessed_data(is_full_chk)?;
            }

            self.checkpoint_mem()?;
        }
    }

    // go: HashAggExec.spillUnprocessedData L1
    fn spill_unprocessed_data(&mut self, is_full_chk: bool) -> Result<(), Error> {
        let data_in_disk = self
            .data_in_disk
            .as_ref()
            .expect("Open initialized dataInDisk");
        let child_result = self.child_result.as_ref().expect(CHILD_RESULT);
        if is_full_chk {
            return data_in_disk.add(child_result);
        }
        let tmp = self
            .tmp_chk_for_spill
            .as_mut()
            .expect("Open initialized tmpChkForSpill");
        for i in 0..child_result.num_rows() {
            tmp.append_row(child_result.get_row(i));
            if tmp.is_full() {
                data_in_disk.add(tmp)?;
                tmp.reset();
            }
        }
        Ok(())
    }

    // go: HashAggExec.getNextChunk L1
    async fn get_next_chunk(&mut self, ctx: &ExecCtx) -> Result<(), Error> {
        let child_result = self.child_result.as_mut().expect(CHILD_RESULT);
        child_result.reset();
        if !self.is_child_drained {
            exec::next(ctx, self.base.base.meta.children(0).as_mut(), child_result).await?;
            if child_result.num_rows() != 0 {
                return Ok(());
            }
            self.is_child_drained = true;
        }
        if self.offset_of_spilled_chks < self.num_of_spilled_chks {
            let chk = self
                .data_in_disk
                .as_ref()
                .expect("Open initialized dataInDisk")
                .get_chunk(self.offset_of_spilled_chks)?;
            self.child_result = Some(chk);
            self.offset_of_spilled_chks += 1;
        }
        Ok(())
    }

    /// Looks up (or allocates) the partial results slice for `group_key`.
    ///
    /// Takes split borrows of `HashAggExec` fields so callers can borrow `group_key`
    /// zero-copy from `group_key_buffer` / `group_keys` (spec §4.1). Runs the memory
    /// checkpoint only when a new group is created (matching Go's positive `Consume` in
    /// `getPartialResults`, A1 §3.3).
    // go: HashAggExec.getPartialResults L2 (split borrows for zero-copy key; A1 checkpoint only on new group; returns Result for nil AggFunc/quota)
    fn get_partial_results_in<'a>(
        partial_result_map: &'a mut AggPartialResultMapper<ColAlloc>,
        partial_agg_funcs: &[Option<Arc<dyn AggFunc>>],
        alloc: &TrackedAlloc,
        mem_tracker: &Arc<Tracker>,
        group_key: &[u8],
    ) -> Result<&'a mut AllocVec<PartialResult, ColAlloc>, Error> {
        if !partial_result_map.m.contains_key(group_key) {
            let col_alloc = ColAlloc::Tracked(alloc.clone());
            let mut partial_results =
                AllocVec::with_capacity_in(partial_agg_funcs.len(), col_alloc.clone());
            for af in partial_agg_funcs {
                let pr: PartialResult = nil_agg_func(af)?.alloc_partial_result();
                partial_results.push(pr);
            }
            let key_box = tracked_key_box(group_key, col_alloc);
            partial_result_map.set(key_box, partial_results);
            alloc.flush();
            mem_tracker.checkpoint()?;
        }
        Ok(partial_result_map
            .m
            .get_mut(group_key)
            .expect("key present after lookup or insert"))
    }

    // go: HashAggExec.initRuntimeStats L1
    fn init_runtime_stats(&mut self) {
        if self.base.base.stats.runtime_stats().is_some() {
            let stats = HashAggRuntimeStats {
                partial_concurrency: self.partial_concurrency,
                partial_wall_time: AtomicI64::new(0),
                final_concurrency: self.final_concurrency,
                final_wall_time: AtomicI64::new(0),
                partial_stats: Vec::with_capacity(self.partial_concurrency),
                final_stats: Vec::with_capacity(self.final_concurrency),
            };
            self.stats = Some(Arc::new(stats));
        }
    }
}

const CHILD_RESULT: &str = "HashAggExec.Next before Open or after Close (Go: nil chunk)";
