//! go-file: pkg/executor/aggregate/agg_hash_executor.go
//!
//! The hash aggregation executor (D12.3 part B; spec `specs/D12.3-agg-executors/spec.md`).
//!
//! Part B1 implements the single-threaded execution path (`Open`, `OpenSelf`, `Close`,
//! `Next`, `initForUnparallelExec`, `unparallelExec`, `resetSpillMode`, `execute`,
//! `spillUnprocessedData`, `getNextChunk`, `getPartialResults`) and the unparallel spill
//! action state machine (stopping real disk writes at `DataInDiskByChunks.Add` / `GetChunk`,
//! `blocked-by:D01.10`).
//!
//! Differences from Go (spec §4, §6):
//! - Memory accounting follows A1 (`TrackedAlloc`): `group_set`, `group_keys`,
//!   `partial_result_map`, and growing partial-result state allocate through `ColAlloc::Tracked`
//!   backed by `mem_tracker`, replacing Go's manual delta estimates. `mem_tracker.checkpoint()?`
//!   runs quota/soft-limit actions at Go's positive `Consume` sites.
//! - Session values (`enable_vectorized_expression`, `track_aggregate_memory_usage`,
//!   `enable_parallel_hashagg_spill`, partial/final concurrency) are captured when the
//!   executor is built (N2); statement trackers, `ErrCtx`, and time zone come from `ExecCtx`.
//! - The Rust session does not yet carry a session-level `MemTracker` (A1 follow-up, spec
//!   §6 item 11, §8.2 decision 6); when `session_mem_tracker` is supplied (e.g. by tests or
//!   once A1 wires it into `SessionVars`), soft-limit spill actions are registered on it.
//! - `prepared` and `executed` are plain `bool` (accessed only by `&mut self`, spec §4.1).
//! - Failpoints and test-only hook methods (spec §2.4, §6 item 14) are not ported.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use allocator_api2::boxed::Box as AllocBox;
use allocator_api2::vec::Vec as AllocVec;
use async_trait::async_trait;
use tidb_base::Error;
use tidb_chunk::ColAlloc;
use tidb_chunk::chunk::Chunk;
use tidb_chunk::chunk_in_disk::{DataInDiskByChunks, new_data_in_disk_by_chunks};
use tidb_expression::ExprRef;
use tidb_expression::eval::RpnState;
use tidb_expression::expression::EvalState;
use tidb_kv::set::{StringSetWithMemoryUsage, new_string_set_with_memory_usage};
use tidb_memory::TrackedAlloc;
use tidb_memory::action::ActionOnExceed;
use tidb_memory::tracker::{Tracker, new_tracker};
use tidb_sessionctx::exprctx::{EvalContextRef, ExprContextRef};
use tidb_sessionctx::variable::tidb_vars::EnableTmpStorageOnOOM;

use crate::aggfuncs::{
    AggFunc, AggPartialResultMapper, PartialResult, new_agg_partial_result_mapper_in,
};
use crate::exec::ExecCtx;
use crate::internal::exec::executor::{self as exec, BaseExecutor, BaseExecutorV2, Executor};

use super::agg_spill::AggSpillDiskAction;
use super::agg_util::get_group_key;

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

    pub default_val: Option<Chunk>,
    child_result: Option<Chunk>,

    /// IsChildReturnEmpty indicates whether the child executor only returns an empty input.
    pub is_child_return_empty: bool,
    /// Whether this executor runs single-threaded instead of in parallel.
    pub is_unparallel_exec: bool,
    parallel_exec_valid: bool,
    prepared: bool,
    executed: bool,

    pub(crate) mem_tracker: Option<Arc<Tracker>>,
    pub(crate) alloc: Option<TrackedAlloc>,
    pub(crate) disk_tracker: Option<Arc<Tracker>>,

    data_in_disk: Option<DataInDiskByChunks>,
    num_of_spilled_chks: usize,
    offset_of_spilled_chks: usize,
    pub(crate) in_spill_mode: Arc<AtomicU32>,
    tmp_chk_for_spill: Option<Chunk>,
    pub(crate) spill_action: Option<Arc<AggSpillDiskAction>>,
    is_child_drained: bool,

    pub file_name_prefix_for_test: String,

    // Session values captured when built (N2).
    #[allow(dead_code)] // Used by B2 parallel workers.
    pub(crate) expr_ctx: ExprContextRef,
    pub(crate) eval_ctx: EvalContextRef,
    pub(crate) enable_vectorized_expression: bool,
    pub(crate) track_aggregate_memory_usage: bool,
    #[allow(dead_code)] // Used by B2 parallel init.
    pub(crate) enable_parallel_hashagg_spill: bool,
    #[allow(dead_code)] // Used by B2 parallel init.
    pub(crate) partial_concurrency: usize,
    #[allow(dead_code)] // Used by B2 parallel init.
    pub(crate) final_concurrency: usize,
    /// Optional session-level memory tracker (`SessionVars.MemTracker` in Go).
    /// None until A1 wires the session tracker into `SessionVars` (spec §6 item 11, §8.2
    /// decision 6); tests may set it to verify soft-limit spill action registration.
    pub session_mem_tracker: Option<Arc<Tracker>>,

    eval_state: EvalState,
    rpn: RpnState,
}

/// Constructs a `HashAggExec` from its base executor and session values captured at build
/// time (N2).
// glue: Go composite literal &aggregate.HashAggExec{...}
#[allow(clippy::too_many_arguments)]
pub fn new_hash_agg_exec(
    base: BaseExecutor,
    group_by_items: Vec<ExprRef>,
    expr_ctx: ExprContextRef,
    enable_vectorized_expression: bool,
    track_aggregate_memory_usage: bool,
    enable_parallel_hashagg_spill: bool,
    partial_concurrency: usize,
    final_concurrency: usize,
) -> HashAggExec {
    let eval_ctx = expr_ctx.get_eval_ctx();
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
        default_val: None,
        child_result: None,
        is_child_return_empty: false,
        is_unparallel_exec: false,
        parallel_exec_valid: false,
        prepared: false,
        executed: false,
        mem_tracker: None,
        alloc: None,
        disk_tracker: None,
        data_in_disk: None,
        num_of_spilled_chks: 0,
        offset_of_spilled_chks: 0,
        in_spill_mode: Arc::new(AtomicU32::new(0)),
        tmp_chk_for_spill: None,
        spill_action: None,
        is_child_drained: false,
        file_name_prefix_for_test: String::new(),
        expr_ctx,
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
    // go: HashAggExec.Close L2 (A1: drops tracked containers before zeroing tracker; no failpoint)
    async fn close(&mut self) -> Result<(), Error> {
        if self.is_unparallel_exec {
            if let Some(child_result) = self.child_result.take()
                && let Some(t) = &self.mem_tracker
            {
                let _ = t.consume(-child_result.memory_usage());
            }
            self.drop_unparallel_tracked_state();
            if let Some(t) = &self.mem_tracker {
                let _ = t.replace_bytes_used(0);
            }
            if let Some(data_in_disk) = self.data_in_disk.take() {
                data_in_disk.close();
            }
            if let Some(spill_action) = self.spill_action.take() {
                spill_action.set_finished();
            }
            self.tmp_chk_for_spill = None;
            return self.base.base.close().await;
        }
        // Part B2 fills the parallel Close branch.
        let _ = self.parallel_exec_valid;
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
        unimplemented!("HashAggExec.parallelExec lands with B2")
    }
}

impl HashAggExec {
    /// OpenSelf just opens the hash aggregation executor.
    // go: HashAggExec.OpenSelf L2 (A1: tracker and TrackedAlloc set up before allocating containers)
    pub fn open_self(&mut self, ctx: &ExecCtx) -> Result<(), Error> {
        self.prepared = false;

        // Drop any tracked state left by an earlier Open without Close before resetting the
        // tracker, so TrackedAlloc does not later subtract already-reset bytes.
        self.drop_unparallel_tracked_state();
        if let Some(child_result) = self.child_result.take()
            && let Some(t) = &self.mem_tracker
        {
            let _ = t.consume(-child_result.memory_usage());
        }

        let mem_tracker = match &self.mem_tracker {
            Some(t) => {
                t.reset()?;
                Arc::clone(t)
            }
            None => {
                let t = new_tracker(self.base.base.meta.id(), -1);
                let alloc = TrackedAlloc::new(Arc::clone(&t));
                for f in self.partial_agg_funcs.iter_mut().flatten() {
                    if let Some(inner) = Arc::get_mut(f) {
                        inner.set_alloc(ColAlloc::Tracked(alloc.clone()));
                    }
                }
                for f in self.final_agg_funcs.iter_mut().flatten() {
                    if let Some(inner) = Arc::get_mut(f) {
                        inner.set_alloc(ColAlloc::Tracked(alloc.clone()));
                    }
                }
                self.mem_tracker = Some(Arc::clone(&t));
                self.alloc = Some(alloc);
                t
            }
        };

        if self.track_aggregate_memory_usage
            && let Some(parent) = &ctx.stmt.mem_tracker
        {
            mem_tracker.attach_to(parent)?;
        }

        if self.is_unparallel_exec {
            return self.init_for_unparallel_exec(ctx);
        }
        unimplemented!("HashAggExec.initForParallelExec lands with B2")
    }

    /// Returns the `ColAlloc` tied to `self.mem_tracker`.
    // glue: A1 TrackedAlloc accessor
    fn col_alloc(&self) -> ColAlloc {
        match &self.alloc {
            Some(a) => ColAlloc::Tracked(a.clone()),
            None => ColAlloc::Global,
        }
    }

    /// Drops unparallel `TrackedAlloc`-backed containers and flushes the pending counter so
    /// every released byte is recorded in `mem_tracker` before reset/close.
    // glue: A1 release of unparallel tracked containers
    fn drop_unparallel_tracked_state(&mut self) {
        self.group_keys = AllocVec::new_in(ColAlloc::Global);
        self.group_set = None;
        self.partial_result_map = None;
        if let Some(alloc) = &self.alloc {
            alloc.flush();
        }
    }

    /// Flushes `TrackedAlloc` and runs the tracker's action checkpoint (Go's positive
    /// `Consume`).
    // glue: A1 checkpoint at Go's positive Consume sites
    fn checkpoint_mem(&self) -> Result<(), Error> {
        if let Some(alloc) = &self.alloc {
            alloc.flush();
        }
        if let Some(t) = &self.mem_tracker {
            t.checkpoint()?;
        }
        Ok(())
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
        if let Some(t) = &self.mem_tracker {
            t.consume(child_mem)?;
        }

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

    /// unparallelExec executes hash aggregation algorithm in single thread.
    // go: HashAggExec.unparallelExec L2 (A1: TrackedAlloc checkpoint instead of memDelta Consume)
    async fn unparallel_exec(&mut self, ctx: &ExecCtx, chk: &mut Chunk) -> Result<(), Error> {
        chk.reset();
        loop {
            if self.prepared {
                // Since we return e.MaxChunkSize() rows every time, we should not traverse
                // `groupSet` because of its randomness.
                while self.cursor4_group_key < self.group_keys.len() {
                    let key = self.group_keys[self.cursor4_group_key].as_ref().to_vec();
                    self.ensure_partial_results(&key)?;
                    let partial_results = self
                        .partial_result_map
                        .as_mut()
                        .expect("Open initialized partialResultMap")
                        .m
                        .get_mut(key.as_slice())
                        .expect("ensure_partial_results inserted key");
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
                self.reset_spill_mode()?;
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

    // go: HashAggExec.resetSpillMode L2 (A1: drops old tracked containers instead of ReplaceBytesUsed)
    fn reset_spill_mode(&mut self) -> Result<(), Error> {
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
        self.checkpoint_mem()?;
        self.in_spill_mode.store(0, Ordering::SeqCst);
        Ok(())
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
            if let Some(t) = &self.mem_tracker {
                t.consume(new_size - m_size)?;
            }

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
            for j in 0..num_rows {
                let group_key = self.group_key_buffer[j].clone();
                let exists = self
                    .group_set
                    .as_ref()
                    .expect("Open initialized groupSet")
                    .exist(&group_key);
                if !exists {
                    let group_count = self
                        .group_set
                        .as_ref()
                        .expect("Open initialized groupSet")
                        .count();
                    if self.in_spill_mode.load(Ordering::SeqCst) == 1 && group_count > 0 {
                        sel.push(j);
                        continue;
                    }
                    let alloc = self.col_alloc();
                    self.group_set
                        .as_mut()
                        .expect("Open initialized groupSet")
                        .insert(&group_key);
                    self.group_keys.push(tracked_key_box(&group_key, alloc));
                }
                self.ensure_partial_results(&group_key)?;
                let partial_results = self
                    .partial_result_map
                    .as_mut()
                    .expect("Open initialized partialResultMap")
                    .m
                    .get_mut(group_key.as_slice())
                    .expect("ensure_partial_results inserted key");
                let child_ref = self.child_result.as_ref().expect(CHILD_RESULT);
                let tmp_buf = [child_ref.get_row(j)];
                let eval_ctx = &*self.eval_ctx;
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

    /// Ensures `group_key` has an entry in `partial_result_map` and runs the memory
    /// checkpoint.
    // go: HashAggExec.getPartialResults L2 (A1: TrackedAlloc checkpoint; returns Result for nil AggFunc/quota)
    fn ensure_partial_results(&mut self, group_key: &[u8]) -> Result<(), Error> {
        let exists = self
            .partial_result_map
            .as_ref()
            .expect("Open initialized partialResultMap")
            .m
            .contains_key(group_key);
        if !exists {
            let alloc = self.col_alloc();
            let mut partial_results =
                AllocVec::with_capacity_in(self.partial_agg_funcs.len(), alloc.clone());
            for af in &self.partial_agg_funcs {
                let pr: PartialResult = nil_agg_func(af)?.alloc_partial_result();
                partial_results.push(pr);
            }
            let key_box = tracked_key_box(group_key, alloc);
            self.partial_result_map
                .as_mut()
                .expect("Open initialized partialResultMap")
                .set(key_box, partial_results);
        }
        self.checkpoint_mem()
    }
}

const CHILD_RESULT: &str = "HashAggExec.Next before Open or after Close (Go: nil chunk)";
