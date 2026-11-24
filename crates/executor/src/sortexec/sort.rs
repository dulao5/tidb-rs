//! go-file: pkg/executor/sortexec/sort.go
//!
//! `SortExec`, the sorting executor.
//!
//! Go has two paths: the unparallel one (all rows of the child in sort partitions, sorted in
//! memory, merged when more than one partition was spilled) and the parallel one (a fetcher,
//! workers and a result generator, `fetchChunksParallel`). Go's `Open` always picks the
//! parallel one (`e.IsUnparallel = false`, sort.go:170); its unit tests switch to the
//! unparallel one after `Open` ([`SortExec::set_unparallel_for_test`] here).
//!
//! - The unparallel path (VS1-36, the in-memory sort part) is ported in memory. More than one
//!   partition only comes from a spill (`externalSorting`, the multi-way merge over spilled
//!   partitions): a boundary stub (D01.10, the disk container).
//! - The parallel path (the parallel sort part of D12.5, the sort executors) runs Go's
//!   goroutines as tasks (N3-A1, the concurrency exception: M-GO, M-CH, M-WG): the chunk
//!   fetcher ([`ChunkFetcher`]) owns the child while it runs, the workers
//!   ([`ParallelSortWorker`]) sort, and the result generator ([`ResultGenerator`]) merges the
//!   workers' rows into the result channel that `Next` reads. `Close` stops them through
//!   `finishCh`, drains the channels in Go's order, then awaits every task and gets the child
//!   back before it closes it (N3 3.5); dropping the executor without `Close` aborts the
//!   tasks. The order of rows with equal sort keys depends on the scheduling of the
//!   workers, as in Go.
//! - Spill itself is not triggered (its actions are not registered: the session memory
//!   tracker of the memory exception A1); the spill branches stop with `Blocked` (D01.10).

// Go identifiers for package constants (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use std::any::Any;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use futures::FutureExt;
use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_chunk::chunk::Chunk;
use tidb_chunk::compare::{CompareFunc, get_compare_func};
use tidb_chunk::row::{Row, RowHandle, RowSize};
use tidb_expression::{EvalContext, Expr, Schema};
use tidb_memory::action::ActionOnExceed;
use tidb_memory::tracker::{LabelForRowChunks, Tracker, new_tracker};
use tidb_parser::types::FieldType;
use tidb_parser::types::type_str;
use tidb_planner::util::byitem::ByItems;
use tidb_sessionctx::exprctx::ExprContextRef;
use tidb_sessionctx::variable::tidb_vars::EnableTmpStorageOnOOM;

use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use super::multi_way_merge::{DiskSource, MemorySource, MultiWayMerger, new_multi_way_merger};
use super::parallel_sort_spill_helper::{ParallelSortSpillHelper, new_parallel_sort_spill_helper};
use super::parallel_sort_worker::{
    ChunkReceiver, ParallelSortWorker, ResultSender, new_parallel_sort_worker,
};
use super::sort_partition::{SortPartition, new_sort_partition};
use super::sort_spill::SortPartitionSpillDiskAction;
use super::sort_util::{
    ChunkWithMemoryUsage, DoneGuard, FinishCh, RowWithError, SharedSortedRowsIter, WaitGroup,
    err_fail_to_add_chunk, inject_parallel_sort_random_fail, new_finish_ch, new_sorted_rows_iter,
    process_panic_and_log,
};
use crate::exec::ExecCtx;
use crate::internal::exec::executor::{
    self as exec, BaseExecutor, BaseExecutorV2, Executor, ret_types, try_new_cache_chunk,
};

/// `ResultChannelCapacity`: the capacity of the result channel of the parallel sort.
pub const ResultChannelCapacity: usize = 10;

/// The sort keys a comparison reads: Go's `e.keyColumns`, `e.keyCmpFuncs` and
/// `e.ByItems[i].Desc`, read through the executor in Go (`lessRow`, `compareRow`,
/// `greaterRow` are its methods). They are bundled so that the top-n heap can hold them
/// without a reference to its executor.
///
/// `key_cmp_funcs` and `descs` have one entry per by-item, `key_columns` one per column
/// by-item (constants are skipped), and the comparisons index all three by the position in
/// `key_columns`, as Go does.
#[derive(Clone)]
pub struct SortKeys {
    pub key_columns: Arc<[usize]>,
    pub key_cmp_funcs: Arc<[CompareFunc]>,
    pub descs: Arc<[bool]>,
}

impl SortKeys {
    // go: SortExec.lessRow L2 (a method of the sort keys, see SortKeys)
    pub fn less_row(&self, row_i: Row<'_>, row_j: Row<'_>) -> isize {
        for (i, &col_idx) in self.key_columns.iter().enumerate() {
            let cmp_func = &self.key_cmp_funcs[i];
            let mut cmp = cmp_func(row_i, col_idx, row_j, col_idx);
            if self.descs[i] {
                cmp = cmp.wrapping_neg();
            }
            if cmp != 0 {
                return cmp;
            }
        }
        0
    }

    // go: SortExec.compareRow L2 (a method of the sort keys, see SortKeys)
    pub fn compare_row(&self, row_i: Row<'_>, row_j: Row<'_>) -> isize {
        for (i, &col_idx) in self.key_columns.iter().enumerate() {
            let cmp_func = &self.key_cmp_funcs[i];
            let mut cmp = cmp_func(row_i, col_idx, row_j, col_idx);
            if self.descs[i] {
                cmp = cmp.wrapping_neg();
            }
            if cmp != 0 {
                return cmp;
            }
        }
        0
    }
}

/// `SortExec.Unparallel`.
#[derive(Default)]
pub struct Unparallel {
    pub idx: usize,
    /// sortPartitions is the chunks to store row values for partitions. Every partition is a
    /// sorted list.
    pub sort_partitions: Vec<SortPartition>,
    pub spill_action: Option<Arc<SortPartitionSpillDiskAction>>,
}

/// `SortExec.Parallel`: the channels, workers and merger of the parallel sort, made in
/// `Open`.
///
/// The senders of the channels are `None` once `closeChannels` closed them or once
/// `fetchChunksParallel` handed them to the tasks (a channel is closed when its last sender is
/// dropped; Go closes it explicitly).
#[derive(Default)]
pub struct Parallel {
    chunk_channel_tx: Option<mpsc::Sender<ChunkWithMemoryUsage>>,
    chunk_channel: Option<ChunkReceiver>,
    /// It's useful when spill is triggered and the fetcher could know when workers finish
    /// their works.
    fetcher_and_worker_syncer: Option<Arc<WaitGroup>>,
    /// A worker is `None` while its task runs (the task owns it) and after `Open`.
    workers: Vec<Option<ParallelSortWorker>>,

    /// Each worker will put their results into the given iter.
    sorted_rows_iters: Vec<SharedSortedRowsIter>,
    merger: Option<MultiWayMerger<MemorySource>>,

    result_channel_tx: Option<ResultSender>,
    result_channel: Option<mpsc::Receiver<RowWithError<RowHandle>>>,

    spill_helper: Option<Arc<ParallelSortSpillHelper>>,
    // spillAction: not built (see `Open`).
    /// The tasks Go starts as goroutines; `Close` awaits them (N3 3.5).
    tasks: ParallelTasks,
}

/// The tasks of a parallel sort (Go's goroutines, which cannot be joined): `Close` takes and
/// awaits them; dropping them without `Close` aborts them (the safety net of N3 3.5, which
/// also drops the child the fetcher owns).
#[derive(Default)]
struct ParallelTasks {
    fetcher: Option<JoinHandle<Box<dyn Executor>>>,
    workers: Vec<JoinHandle<ParallelSortWorker>>,
    generator: Option<JoinHandle<()>>,
}

impl ParallelTasks {
    // glue: N3 3.5 (abort on drop)
    fn abort_all(&self) {
        if let Some(h) = &self.fetcher {
            h.abort();
        }
        for h in &self.workers {
            h.abort();
        }
        if let Some(h) = &self.generator {
            h.abort();
        }
    }
}

impl Drop for ParallelTasks {
    // glue: N3 3.5 (the executor dropped without Close: its tasks are aborted)
    fn drop(&mut self) {
        self.abort_all();
    }
}

/// SortExec represents sorting executor.
pub struct SortExec {
    pub base: BaseExecutor,

    pub by_items: Vec<ByItems>,
    /// `fetched` (Go makes a new `atomic.Bool` in `Open`; one owner here).
    pub(crate) fetched: bool,
    pub exec_schema: Schema,

    /// keyColumns is the column index of the by items.
    pub(crate) key_columns: Arc<[usize]>,
    /// keyCmpFuncs is used to compare each ByItem.
    pub(crate) key_cmp_funcs: Arc<[CompareFunc]>,

    cur_partition: Option<SortPartition>,

    /// We can't spill if size of data is lower than the limit.
    spill_limit: i64,

    pub(crate) mem_tracker: Option<Arc<Tracker>>,
    pub(crate) disk_tracker: Option<Arc<Tracker>>,

    /// TODO delete this variable in the future and remove the unparallel sort.
    pub is_unparallel: bool,

    /// `finishCh`: the sender `Close` closes (`nil` until `Open`); the tasks hold receivers.
    finish_ch: Option<watch::Sender<bool>>,

    pub file_name_prefix_for_test: String,

    pub unparallel: Unparallel,

    pub parallel: Parallel,

    enable_tmp_storage_on_oom: bool,

    /// The session's expression context: Go reads `e.Ctx().GetExprCtx()` when `Next` starts.
    expr_ctx: ExprContextRef,
    /// The byte limit of the session memory tracker when the executor was built: Go reads
    /// `e.Ctx().GetSessionVars().MemTracker.GetBytesLimit()` in `Open`. The Rust session has
    /// no memory tracker (the memory exception A1); its limit is `tidb_mem_quota_query`, the value the
    /// statement set it to.
    session_mem_bytes_limit: i64,
    /// `tidb_executor_concurrency` when the executor was built: Go reads
    /// `e.Ctx().GetSessionVars().ExecutorConcurrency` in `Open`.
    executor_concurrency: i64,
    /// Set by [`SortExec::set_unparallel_for_test`].
    unparallel_for_test: bool,
}

/// A sort executor of `by_items` over the base executor `base` (Go's composite literal in
/// `buildSort`, with the session values `Open` and `Next` read).
// glue: Go composite literal sortexec.SortExec{...} (buildSort / buildTopN)
pub fn new_sort_exec(
    base: BaseExecutor,
    by_items: Vec<ByItems>,
    exec_schema: Schema,
    expr_ctx: ExprContextRef,
    session_mem_bytes_limit: i64,
    executor_concurrency: i64,
) -> SortExec {
    SortExec {
        base,
        by_items,
        fetched: false,
        exec_schema,
        key_columns: Arc::from(Vec::new()),
        key_cmp_funcs: Arc::from(Vec::new()),
        cur_partition: None,
        spill_limit: 0,
        mem_tracker: None,
        disk_tracker: None,
        is_unparallel: false,
        finish_ch: None,
        file_name_prefix_for_test: String::new(),
        unparallel: Unparallel::default(),
        parallel: Parallel::default(),
        enable_tmp_storage_on_oom: false,
        expr_ctx,
        session_mem_bytes_limit,
        executor_concurrency,
        unparallel_for_test: false,
    }
}

impl SortExec {
    /// Makes `Open` take the unparallel path. Go's tests set `IsUnparallel = true` and call
    /// `InitUnparallelModeForTest` after `Open` (which has made the parallel channels by
    /// then, unused); here the switch is made before `Open`, which then makes the unparallel
    /// state only.
    // glue: test hook (Go's tests write SortExec.IsUnparallel after Open)
    pub fn set_unparallel_for_test(&mut self) {
        self.unparallel_for_test = true;
    }

    /// The sort keys of the comparisons (see [`SortKeys`]).
    // glue: bundles e.keyColumns, e.keyCmpFuncs and e.ByItems[i].Desc (see SortKeys)
    pub fn sort_keys(&self) -> SortKeys {
        SortKeys {
            key_columns: Arc::clone(&self.key_columns),
            key_cmp_funcs: Arc::clone(&self.key_cmp_funcs),
            descs: self.by_items.iter().map(|b| b.desc).collect(),
        }
    }

    /// When fetcher and workers are not created, we need to initiatively close these
    /// channels. A channel is closed by dropping its sender (the senders the executor still
    /// holds; mapping rule M-CH).
    // go: SortExec.closeChannels L1 [M-CH]
    fn close_channels(&mut self) {
        self.parallel.result_channel_tx = None;
        self.parallel.chunk_channel_tx = None;
    }

    // go: SortExec.InitUnparallelModeForTest L1
    pub fn init_unparallel_mode_for_test(&mut self) {
        self.unparallel.idx = 0;
        self.unparallel.sort_partitions.clear();
    }

    // go: SortExec.appendResultToChunkInParallelMode L1 [M-CH, M-ROW]
    async fn append_result_to_chunk_in_parallel_mode(
        &mut self,
        req: &mut Chunk,
    ) -> Result<(), Error> {
        let result_channel = self
            .parallel
            .result_channel
            .as_mut()
            .expect("Open made the result channel");
        while !req.is_full() {
            let Some(row) = result_channel.recv().await else {
                return Ok(());
            };
            if let Some(err) = row.err {
                return Err(err);
            }
            let row = row.row.expect("a result row without an error has a row");
            req.append_row(row.row());
        }
        Ok(())
    }

    // go: SortExec.appendResultToChunkInUnparallelMode L1
    fn append_result_to_chunk_in_unparallel_mode(&mut self, req: &mut Chunk) -> Result<(), Error> {
        let sort_partition_list_len = self.unparallel.sort_partitions.len();
        if sort_partition_list_len == 0 {
            return Ok(());
        }

        if sort_partition_list_len == 1 {
            self.one_partition_sorting(req)?;
        } else {
            self.external_sorting(req)?;
        }
        Ok(())
    }

    // go: SortExec.onePartitionSorting L1
    fn one_partition_sorting(&mut self, req: &mut Chunk) -> Result<(), Error> {
        let partition = &mut self.unparallel.sort_partitions[0];
        partition.check_error()?;

        while !req.is_full() {
            let Some(row) = partition.get_next_sorted_row()? else {
                return Ok(());
            };

            req.append_row(row.row());
        }
        Ok(())
    }

    /// The multi-way merge of the partitions; more than one partition only comes from a
    /// spill.
    // glue: stub of SortExec.externalSorting, blocked-by:D01.10 (merging sort partitions spilled to disk)
    fn external_sorting(&mut self, _req: &mut Chunk) -> Result<(), Error> {
        Err(blocked("D01.10", "merging sort partitions spilled to disk"))
    }

    // go: SortExec.fetchChunks L1
    async fn fetch_chunks(&mut self, ctx: &ExecCtx) -> Result<(), Error> {
        if self.is_unparallel {
            return self.fetch_chunks_unparallel(ctx).await;
        }
        self.fetch_chunks_parallel(ctx)
    }

    /// Registering the partition's spill action on the session memory tracker
    /// (`FallbackOldAndSetNewAction`) is left out: the Rust session has no memory tracker.
    // go: SortExec.switchToNewSortPartition L2 (the spill action is not registered on the session tracker, see the comment in the body)
    fn switch_to_new_sort_partition(
        &mut self,
        fields: &[Arc<FieldType>],
        by_items_desc: &Arc<[bool]>,
        append_partition: bool,
    ) -> Result<(), Error> {
        if append_partition {
            // Put the full partition into list
            let full = self
                .cur_partition
                .take()
                .expect("invalid memory address or nil pointer dereference");
            self.unparallel.sort_partitions.push(full);
        }

        let cur = if append_partition {
            self.unparallel.sort_partitions.last()
        } else {
            self.cur_partition.as_ref()
        };
        if let Some(cur) = cur {
            cur.check_error()?;
        }

        let mut cur = new_sort_partition(
            fields.to_vec(),
            Arc::clone(by_items_desc),
            Arc::clone(&self.key_columns),
            Arc::clone(&self.key_cmp_funcs),
            self.spill_limit,
            self.file_name_prefix_for_test.clone(),
        );
        let mem_tracker = self
            .mem_tracker
            .as_ref()
            .expect("Open made the memory tracker");
        cur.get_mem_tracker().attach_to(mem_tracker)?;
        cur.get_mem_tracker().set_label(LabelForRowChunks)?;
        self.unparallel.spill_action = Some(cur.action_spill());
        if self.enable_tmp_storage_on_oom {
            let disk_tracker = self
                .disk_tracker
                .as_ref()
                .expect("Open made the disk tracker");
            cur.get_disk_tracker().attach_to(disk_tracker)?;
            cur.get_disk_tracker().set_label(LabelForRowChunks)?;
            // glue: blocked-by:D01.10 (e.Ctx().GetSessionVars().MemTracker.FallbackOldAndSetNewAction(e.Unparallel.spillAction): the spill to disk is not ported, and the session memory tracker it hangs on is not in the Rust session yet (exception A1; the missing session memory tracker is issue #344), so a sort never spills)
        }
        self.cur_partition = Some(cur);
        Ok(())
    }

    // go: SortExec.checkError L1
    fn check_error(&self) -> Result<(), Error> {
        for partition in &self.unparallel.sort_partitions {
            partition.check_error()?;
        }
        Ok(())
    }

    // go: SortExec.storeChunk L1 [M-ROW]
    fn store_chunk(
        &mut self,
        chk: Chunk,
        fields: &[Arc<FieldType>],
        by_items_desc: &Arc<[bool]>,
    ) -> Result<(), Error> {
        self.cur_partition().check_error()?;

        let chk = Arc::new(chk);
        if !self.cur_partition_mut().add(&chk)? {
            self.switch_to_new_sort_partition(fields, by_items_desc, true)?;

            if !self.cur_partition_mut().add(&chk)? {
                return Err(err_fail_to_add_chunk());
            }
        }
        Ok(())
    }

    // glue: e.curPartition (a nil pointer dereference panics in Go)
    fn cur_partition(&self) -> &SortPartition {
        self.cur_partition
            .as_ref()
            .expect("invalid memory address or nil pointer dereference")
    }

    // glue: e.curPartition (a nil pointer dereference panics in Go)
    fn cur_partition_mut(&mut self) -> &mut SortPartition {
        self.cur_partition
            .as_mut()
            .expect("invalid memory address or nil pointer dereference")
    }

    // go: SortExec.handleCurrentPartitionBeforeExit L1
    fn handle_current_partition_before_exit(&mut self) -> Result<(), Error> {
        self.check_error()?;

        self.cur_partition_mut().sort()?;

        Ok(())
    }

    // go: SortExec.fetchChunksUnparallel L2 (failpoints unholdSyncLock, waitForSpill and SignalCheckpointForSort not ported: no failpoints [M-ROW])
    async fn fetch_chunks_unparallel(&mut self, ctx: &ExecCtx) -> Result<(), Error> {
        let fields: Vec<Arc<FieldType>> = ret_types(self).to_vec();
        let by_items_desc: Arc<[bool]> = self.by_items.iter().map(|b| b.desc).collect();

        self.switch_to_new_sort_partition(&fields, &by_items_desc, false)?;

        loop {
            let mut chk = try_new_cache_chunk(self.base.base.meta.all_children()[0].as_ref());
            exec::next(ctx, self.base.base.meta.children(0).as_mut(), &mut chk).await?;
            if chk.num_rows() == 0 {
                break;
            }

            self.store_chunk(chk, &fields, &by_items_desc)?;
        }

        self.handle_current_partition_before_exit()?;

        let cur = self
            .cur_partition
            .take()
            .expect("invalid memory address or nil pointer dereference");
        self.unparallel.sort_partitions.push(cur);
        Ok(())
    }

    /// Starts the workers, the chunk fetcher and the result generator.
    ///
    /// The tasks are kept for `Close`, which awaits them (Go's goroutines are not joined).
    /// The child moves into the fetcher until `Close` gets it back. The merger `Open` made
    /// gets the sort keys `Next` has just built: Go's merger calls the method value
    /// `e.lessRow`, which reads them when it compares.
    // go: SortExec.fetchChunksParallel L2 (tasks kept for Close; the child moves into the fetcher; the merger gets the sort keys built by Next, see the comment [M-GO, M-WG])
    fn fetch_chunks_parallel(&mut self, ctx: &ExecCtx) -> Result<(), Error> {
        let spawner = match Spawner::new(ctx) {
            Ok(spawner) => spawner,
            Err(err) => {
                self.close_channels();
                return Err(err);
            }
        };
        // Wait for the finish of all workers
        let workers_waiter = Arc::new(WaitGroup::default());
        // Wait for the finish of chunk fetcher
        let fetcher_waiter = Arc::new(WaitGroup::default());

        let less_row = self.sort_keys();
        let finish_ch = FinishCh::from_sender(
            self.finish_ch
                .as_ref()
                .expect("Open made the finish channel"),
        );
        let mem_tracker = Arc::clone(
            self.mem_tracker
                .as_ref()
                .expect("Open made the memory tracker"),
        );
        let syncer = Arc::clone(
            self.parallel
                .fetcher_and_worker_syncer
                .as_ref()
                .expect("Open made the syncer"),
        );
        let spill_helper = Arc::clone(
            self.parallel
                .spill_helper
                .as_ref()
                .expect("Open made the spill helper"),
        );
        let chunk_channel = Arc::clone(
            self.parallel
                .chunk_channel
                .as_ref()
                .expect("Open made the chunk channel"),
        );
        let result_channel = self
            .parallel
            .result_channel_tx
            .take()
            .expect("Open made the result channel");
        let chunk_channel_tx = self
            .parallel
            .chunk_channel_tx
            .take()
            .expect("Open made the chunk channel");
        let max_chunk_size = self.max_chunk_size() as usize;

        for i in 0..self.parallel.workers.len() {
            let mut worker = new_parallel_sort_worker(
                i,
                less_row.clone(),
                Arc::clone(&chunk_channel),
                Arc::clone(&syncer),
                result_channel.clone(),
                finish_ch.clone(),
                Arc::clone(&mem_tracker),
                Arc::clone(&self.parallel.sorted_rows_iters[i]),
                max_chunk_size,
                Arc::clone(&spill_helper),
                Some(self.base.base.killer.clone()),
            );
            let handle = spawner.run_in(&workers_waiter, "parallel sort worker", async move {
                worker.run().await;
                worker.exit();
                worker
            });
            self.parallel.tasks.workers.push(handle);
        }

        // Fetch chunks from child and put chunks into chunkChannel
        let mut children = self.base.base.meta.take_all_children();
        let child = children.remove(0);
        self.base.base.meta.set_all_children(children);
        let fetcher = ChunkFetcher {
            ctx: ctx.clone(),
            child,
            chunk_channel: Some(chunk_channel_tx),
            fetcher_and_worker_syncer: syncer,
            result_channel: result_channel.clone(),
            finish_ch: finish_ch.clone(),
            mem_tracker,
            spill_helper: Arc::clone(&spill_helper),
        };
        self.parallel.tasks.fetcher =
            Some(spawner.run_in(&fetcher_waiter, "parallel sort fetcher", fetcher.run()));

        let mut merger = self.parallel.merger.take();
        if let Some(merger) = &mut merger {
            merger.multi_way_merge.less_row_function = less_row.clone();
        }
        let generator = ResultGenerator {
            less_row: self.sort_keys(),
            merger,
            sorted_rows_iters: self.parallel.sorted_rows_iters.clone(),
            result_channel: Some(result_channel),
            finish_ch,
            spill_helper,
            max_chunk_size,
        };
        self.parallel.tasks.generator = Some(spawner.spawn(
            "parallel sort result generator",
            generator.generate_result(vec![workers_waiter, fetcher_waiter]),
        ));
        Ok(())
    }

    // go: SortExec.initCompareFuncs L2 (failpoint ParallelSortRandomFail not ported: no failpoints)
    pub(crate) fn init_compare_funcs(&mut self, ctx: &dyn EvalContext) -> Result<(), Error> {
        let mut key_cmp_funcs = Vec::with_capacity(self.by_items.len());
        for by_item in &self.by_items {
            let key_type = by_item.expr.get_type(ctx);
            match get_compare_func(&key_type) {
                Some(f) => key_cmp_funcs.push(f),
                None => {
                    return Err(Error::plain(format!(
                        "Sort executor not supports type {}",
                        type_str(key_type.get_type())
                    )));
                }
            }
        }
        self.key_cmp_funcs = Arc::from(key_cmp_funcs);
        Ok(())
    }

    // go: SortExec.buildKeyColumns L1 [M-IFACE]
    pub(crate) fn build_key_columns(&mut self) -> Result<(), Error> {
        let mut key_columns = Vec::with_capacity(self.by_items.len());
        for by in &self.by_items {
            match by.expr.as_ref() {
                Expr::Column(col) => key_columns.push(col.index as usize),
                Expr::Constant(_) => {
                    // Ignore constant as constant can not affect the sorted result
                }
                _ => return Err(Error::plain("Get unexpected expression")),
            }
        }
        self.key_columns = Arc::from(key_columns);
        Ok(())
    }

    /// `IsSpillTriggeredInOnePartitionForTest`.
    // go: SortExec.IsSpillTriggeredInOnePartitionForTest L1
    pub fn is_spill_triggered_in_one_partition_for_test(&self, idx: usize) -> bool {
        self.unparallel.sort_partitions[idx].is_spill_triggered()
    }

    /// `GetRowNumInOnePartitionMemoryForTest`.
    // go: SortExec.GetRowNumInOnePartitionMemoryForTest L1
    pub fn get_row_num_in_one_partition_memory_for_test(&self, idx: usize) -> i64 {
        self.unparallel.sort_partitions[idx].num_row_in_memory_for_test()
    }

    /// `GetSortPartitionListLenForTest`.
    // go: SortExec.GetSortPartitionListLenForTest L1
    pub fn get_sort_partition_list_len_for_test(&self) -> usize {
        self.unparallel.sort_partitions.len()
    }

    /// IsSpillTriggeredInParallelSortForTest tells if spill is triggered in parallel sort.
    // go: SortExec.IsSpillTriggeredInParallelSortForTest L1
    pub fn is_spill_triggered_in_parallel_sort_for_test(&self) -> bool {
        self.parallel
            .spill_helper
            .as_ref()
            .expect("invalid memory address or nil pointer dereference")
            .is_spill_triggered()
    }

    /// GetSpilledRowNumInParallelSortForTest tells if spill is triggered in parallel sort.
    // go: SortExec.GetSpilledRowNumInParallelSortForTest L1
    pub fn get_spilled_row_num_in_parallel_sort_for_test(&self) -> i64 {
        let mut total_spilled_rows = 0;
        let spill_helper = self
            .parallel
            .spill_helper
            .as_ref()
            .expect("invalid memory address or nil pointer dereference");
        for disk in spill_helper.sorted_rows_in_disk() {
            total_spilled_rows += disk.num_rows();
        }
        total_spilled_rows
    }

    /// GetSortMetaForTest returns some sort meta, it's only used in test.
    // go: SortExec.GetSortMetaForTest L1
    pub fn get_sort_meta_for_test(&self) -> (Arc<[usize]>, Arc<[CompareFunc]>, Vec<bool>) {
        let key_columns = Arc::clone(&self.key_columns);
        let key_cmp_funcs = Arc::clone(&self.key_cmp_funcs);
        let by_items_desc = self.by_items.iter().map(|b| b.desc).collect();
        (key_columns, key_cmp_funcs, by_items_desc)
    }

    /// Puts the child back after the fetcher task handed it back (see `fetchChunksParallel`).
    // glue: M-GO (the child returns from the task that read it)
    fn put_child_back(&mut self, child: Box<dyn Executor>) {
        let mut children = self.base.base.meta.take_all_children();
        children.insert(0, child);
        self.base.base.meta.set_all_children(children);
    }

    /// The session's expression context (Go's `e.Ctx().GetExprCtx()`).
    // glue: e.Ctx().GetExprCtx()
    pub fn expr_ctx(&self) -> &ExprContextRef {
        &self.expr_ctx
    }

    /// The memory tracker of the executor (`e.memTracker`), for tests.
    // glue: accessor
    pub fn mem_tracker(&self) -> Option<&Arc<Tracker>> {
        self.mem_tracker.as_ref()
    }
}

#[async_trait]
impl Executor for SortExec {
    // glue: M-SELF (Go struct embedding)
    fn base(&self) -> &BaseExecutorV2 {
        &self.base.base
    }
    // glue: M-SELF (Go struct embedding)
    fn base_mut(&mut self) -> &mut BaseExecutorV2 {
        &mut self.base.base
    }

    /// Open implements the Executor Open interface.
    ///
    /// The statement's trackers are in the execution context (Go reads them from the session's
    /// statement context); the spill limit and the worker count are the session values kept
    /// when the executor was built. The sorted rows iterators are made before the merger that
    /// reads them (Go fills the slice the merger shares after making it). The spill action is
    /// not built: it is only registered with the session memory tracker, see the comment in
    /// the body.
    // go: SortExec.Open L2 (session values from the build; iterators made before the merger; no spill action, see the comment)
    async fn open(&mut self, ctx: &ExecCtx) -> Result<(), Error> {
        self.fetched = false;
        self.enable_tmp_storage_on_oom = EnableTmpStorageOnOOM.load(Ordering::SeqCst);
        let (finish_ch, _) = new_finish_ch();
        self.finish_ch = Some(finish_ch);

        // To avoid duplicated initialization for TopNExec.
        if self.mem_tracker.is_none() {
            let mem_tracker = new_tracker(self.base.base.meta.id(), -1);
            if let Some(stmt_tracker) = &ctx.stmt.mem_tracker {
                mem_tracker.attach_to(stmt_tracker)?;
            }
            self.mem_tracker = Some(mem_tracker);
            self.spill_limit = self.session_mem_bytes_limit / 10;
            let disk_tracker = new_tracker(self.base.base.meta.id(), -1);
            if let Some(stmt_tracker) = &ctx.stmt.disk_tracker {
                disk_tracker.attach_to(stmt_tracker)?;
            }
            self.disk_tracker = Some(disk_tracker);
        }

        self.is_unparallel = false;
        if self.unparallel_for_test {
            self.is_unparallel = true;
        }
        if self.is_unparallel {
            self.unparallel.idx = 0;
            self.unparallel.sort_partitions.clear();
        } else {
            let concurrency = self.executor_concurrency as usize;
            self.parallel = Parallel::default();
            self.parallel.workers = (0..concurrency).map(|_| None).collect();
            let (chunk_channel_tx, chunk_channel) = mpsc::channel(concurrency);
            self.parallel.chunk_channel_tx = Some(chunk_channel_tx);
            self.parallel.chunk_channel = Some(Arc::new(tokio::sync::Mutex::new(chunk_channel)));
            self.parallel.fetcher_and_worker_syncer = Some(Arc::new(WaitGroup::default()));
            self.parallel.sorted_rows_iters = (0..self.parallel.workers.len())
                .map(|_| Arc::new(parking_lot::Mutex::new(new_sorted_rows_iter(Vec::new()))))
                .collect();
            let (result_channel_tx, result_channel) = mpsc::channel(ResultChannelCapacity);
            self.parallel.result_channel_tx = Some(result_channel_tx);
            self.parallel.result_channel = Some(result_channel);
            self.parallel.merger = Some(new_multi_way_merger(
                MemorySource {
                    sorted_rows_iters: self.parallel.sorted_rows_iters.clone(),
                },
                self.sort_keys(),
            ));
            self.parallel.spill_helper = Some(Arc::new(new_parallel_sort_spill_helper()));
            // glue: blocked-by:D01.10 (newParallelSortSpillDiskAction and, if e.enableTmpStorageOnOOM, e.Ctx().GetSessionVars().MemTracker.FallbackOldAndSetNewAction(e.Parallel.spillAction): the spill to disk is not ported, and the session memory tracker the action hangs on is not in the Rust session yet (exception A1; issue #344), so the parallel sort never spills)
        }

        exec::open(ctx, self.base.base.meta.children(0).as_mut()).await
    }

    /// Next implements the Executor Next interface. See Go's comment for the steps of the
    /// unparallel and the parallel modes.
    // go: SortExec.Next L1
    async fn next(&mut self, ctx: &ExecCtx, req: &mut Chunk) -> Result<(), Error> {
        if !self.fetched {
            self.fetched = true;
            let eval_ctx = self.expr_ctx.get_eval_ctx();
            if let Err(err) = self.init_compare_funcs(eval_ctx.as_ref()) {
                self.close_channels();
                return Err(err);
            }

            if let Err(err) = self.build_key_columns() {
                self.close_channels();
                return Err(err);
            }
            self.fetch_chunks(ctx).await?;
        }

        req.reset();
        if self.is_unparallel {
            return self.append_result_to_chunk_in_unparallel_mode(req);
        }
        self.append_result_to_chunk_in_parallel_mode(req).await
    }

    /// Close implements the Executor Close interface.
    ///
    /// The parallel branch follows Go's order (close `finishCh`, drain the chunk channel, drain
    /// the result channel until the generator closes it, reset the workers, close the spill
    /// helper), with one step Go does not have: after the result channel is drained, every
    /// task is awaited, and the fetcher hands the child back, before the child is closed
    /// (N3 3.5).
    // go: SortExec.Close L2 (the tasks are awaited and the child taken back from the fetcher, see the comment [M-CH, M-WG])
    async fn close(&mut self) -> Result<(), Error> {
        // TopN not initializes `e.finishCh` but it will call the Close function
        if let Some(finish_ch) = &self.finish_ch {
            finish_ch.send_replace(true);
        }
        if let Some(spill_action) = &self.unparallel.spill_action {
            spill_action.set_finished();
        }

        let mut join_err: Option<Error> = None;
        if self.is_unparallel {
            for partition in &mut self.unparallel.sort_partitions {
                partition.close()?;
            }
        } else if self.finish_ch.is_some() {
            if !self.fetched {
                self.fetched = true;
                self.close_channels();
            } else if let Some(chunk_channel) = &self.parallel.chunk_channel {
                let syncer = self
                    .parallel
                    .fetcher_and_worker_syncer
                    .as_ref()
                    .expect("Open made the syncer");
                let mut chunk_channel = chunk_channel.lock().await;
                while chunk_channel.recv().await.is_some() {
                    syncer.done();
                }
            }

            // Ensure that `generateResult()` has exited,
            // or data race may happen as `generateResult()`
            // will use `e.Parallel.workers` and `e.Parallel.merger`.
            if let Some(result_channel) = &mut self.parallel.result_channel {
                while result_channel.recv().await.is_some() {}
            }
            join_err = self.join_parallel_tasks().await;
            for worker in self.parallel.workers.iter_mut().flatten() {
                worker.reset()?;
            }
            self.parallel.merger = None;
            // e.Parallel.spillAction is nil: it is not built (see Open).
            if let Some(spill_helper) = &self.parallel.spill_helper {
                spill_helper.close();
            }
        }

        if let Some(mem_tracker) = &self.mem_tracker {
            mem_tracker.replace_bytes_used(0)?;
        }

        // A task that did not complete (join_err) may have taken the child with it; the
        // child is closed when it is back, and the task's error comes first.
        let closed = if self.base.base.meta.children_len() > 0 {
            exec::close(self.base.base.meta.children(0).as_mut()).await
        } else {
            Ok(())
        };
        match join_err {
            Some(err) => Err(err),
            None => closed,
        }
    }
}

impl SortExec {
    /// Awaits the tasks of the parallel sort (they have ended or are ending once the result
    /// channel is closed), puts the workers back for `reset` and the child back for `Close`.
    /// A task that did not complete (the runtime shut down) is an error, returned after the
    /// other steps of `Close`.
    // glue: N3 3.5 (Close joins the tasks)
    async fn join_parallel_tasks(&mut self) -> Option<Error> {
        let mut first_err = None;
        let tasks = &mut self.parallel.tasks;
        let fetcher = tasks.fetcher.take();
        let workers = std::mem::take(&mut tasks.workers);
        let generator = tasks.generator.take();
        if let Some(h) = generator
            && let Err(e) = h.await
        {
            first_err.get_or_insert(join_error(e));
        }
        if let Some(h) = fetcher {
            match h.await {
                Ok(child) => self.put_child_back(child),
                Err(e) => {
                    first_err.get_or_insert(join_error(e));
                }
            }
        }
        for (i, h) in workers.into_iter().enumerate() {
            match h.await {
                Ok(worker) => self.parallel.workers[i] = Some(worker),
                Err(e) => {
                    first_err.get_or_insert(join_error(e));
                }
            }
        }
        first_err
    }
}

#[cfg(test)]
impl SortExec {
    /// The parallel sort's tasks not yet awaited, the counter of the chunks sent and not yet
    /// processed, and the largest strong count of a worker's sorted rows (1 once every task
    /// and worker has let go of them).
    // glue: test accessor
    pub(crate) fn parallel_state_for_test(&self) -> (usize, i64, usize) {
        let t = &self.parallel.tasks;
        let tasks =
            t.workers.len() + usize::from(t.fetcher.is_some()) + usize::from(t.generator.is_some());
        let syncer = self
            .parallel
            .fetcher_and_worker_syncer
            .as_ref()
            .map_or(0, |s| s.count());
        let iters = self
            .parallel
            .sorted_rows_iters
            .iter()
            .map(Arc::strong_count)
            .max()
            .unwrap_or(1);
        (tasks, syncer, iters)
    }
}

/// The error of a task that did not complete.
// glue: tokio JoinError as an error
fn join_error(e: tokio::task::JoinError) -> Error {
    Error::plain(format!("a task of the parallel sort did not complete: {e}"))
}

/// Where the tasks of the parallel sort are spawned (mapping rule M-GO): the `ExecPool` of
/// the execution context, or the current tokio runtime when the context has none.
struct Spawner {
    pool: Option<tidb_runtime::pool::ExecPool>,
    handle: Option<tokio::runtime::Handle>,
}

impl Spawner {
    /// Fails outside a tokio runtime when the context has no pool (Go can always start a
    /// goroutine; nothing is spawned then).
    // glue: M-GO
    fn new(ctx: &ExecCtx) -> Result<Spawner, Error> {
        if let Some(pool) = &ctx.pool {
            return Ok(Spawner {
                pool: Some(pool.clone()),
                handle: None,
            });
        }
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => Ok(Spawner {
                pool: None,
                handle: Some(handle),
            }),
            Err(_) => Err(Error::plain(
                "the parallel sort needs a tokio runtime to run its tasks",
            )),
        }
    }

    /// `go f()`.
    // glue: go f() on the N3 ExecPool (M-GO)
    fn spawn<T: Send + 'static>(
        &self,
        name: &'static str,
        fut: impl Future<Output = T> + Send + 'static,
    ) -> JoinHandle<T> {
        match (&self.pool, &self.handle) {
            (Some(pool), _) => pool.spawn(name, fut),
            (None, Some(handle)) => handle.spawn(fut),
            (None, None) => unreachable!("a spawner has a pool or a runtime"),
        }
    }

    /// `util.WaitGroupWrapper.Run`: `wg` counts the task until it ends (also when it panics
    /// or is cancelled).
    // glue: util.WaitGroupWrapper.Run as a task (M-GO, M-WG)
    fn run_in<T: Send + 'static>(
        &self,
        wg: &Arc<WaitGroup>,
        name: &'static str,
        fut: impl Future<Output = T> + Send + 'static,
    ) -> JoinHandle<T> {
        wg.add(1);
        let done = DoneGuard(Arc::clone(wg));
        self.spawn(name, async move {
            let _done = done;
            fut.await
        })
    }
}

/// `select { case <-finishCh: return; case ch <- row: }`: false when `finishCh` is closed
/// first, or when the receiver is gone (the executor was dropped, which aborts this task
/// too).
// glue: select on finishCh and a send to the result channel (M-CH)
async fn send_or_finish(
    finish_ch: &mut FinishCh,
    ch: &ResultSender,
    row: RowWithError<RowHandle>,
) -> bool {
    tokio::select! {
        _ = finish_ch.closed() => false,
        sent = ch.send(row) => sent.is_ok(),
    }
}

/// The state of the chunk fetcher's task (Go's goroutine reads `e.Children(0)` and
/// `e.Parallel`): it owns the child while it runs and hands it back when it ends.
struct ChunkFetcher {
    ctx: ExecCtx,
    child: Box<dyn Executor>,
    /// `None` once closed.
    chunk_channel: Option<mpsc::Sender<ChunkWithMemoryUsage>>,
    fetcher_and_worker_syncer: Arc<WaitGroup>,
    result_channel: ResultSender,
    finish_ch: FinishCh,
    mem_tracker: Arc<Tracker>,
    spill_helper: Arc<ParallelSortSpillHelper>,
}

impl ChunkFetcher {
    /// Fetch chunks from child and put chunks into chunkChannel.
    ///
    /// Go's deferred function runs after the loop: a panic of the task (or the error of a
    /// memory quota, which Go panics with) is sent, then the fetcher waits for the workers,
    /// spills what remains if needed and closes the chunk channel. The child is returned.
    // go: SortExec.fetchChunksFromChild L2 (a method of the fetcher task's state, which returns the child; failpoint SignalCheckpointForSort not ported: no failpoints [M-PANIC, M-CH])
    async fn run(mut self) -> Box<dyn Executor> {
        let ret = AssertUnwindSafe(self.fetch_loop()).catch_unwind().await;
        let r: Option<Box<dyn Any + Send>> = match ret {
            Ok(Ok(())) => None,
            Ok(Err(err)) => Some(Box::new(err)),
            Err(payload) => Some(payload),
        };
        if let Some(r) = r {
            process_panic_and_log(&self.result_channel, r).await;
        }

        self.fetcher_and_worker_syncer.wait().await;
        if let Err(err) = self.spill_remaining_rows_when_needed() {
            let _ = self
                .result_channel
                .send(RowWithError {
                    row: None,
                    err: Some(err),
                })
                .await;
        }

        // We must place it after the spill as workers will process its received
        // chunks after channel is closed and this will cause data race.
        self.chunk_channel = None;
        self.child
    }

    /// The loop of `fetchChunksFromChild`. Returns the error Go panics with (a memory quota
    /// exceeded while consuming); the other errors are sent here, as Go sends them.
    // glue: the body of fetchChunksFromChild under its deferred recover (M-PANIC)
    async fn fetch_loop(&mut self) -> Result<(), Error> {
        loop {
            let mut chk = try_new_cache_chunk(self.child.as_ref());
            if let Err(err) = exec::next(&self.ctx, self.child.as_mut(), &mut chk).await {
                let _ = self
                    .result_channel
                    .send(RowWithError {
                        row: None,
                        err: Some(err),
                    })
                    .await;
                return Ok(());
            }

            let row_count = chk.num_rows();
            if row_count == 0 {
                break;
            }

            let memory_usage = chk.memory_usage() + RowSize * row_count as i64;
            let chk_with_memory_usage = ChunkWithMemoryUsage {
                chk: Arc::new(chk),
                memory_usage,
            };

            self.mem_tracker
                .consume(chk_with_memory_usage.memory_usage)?;

            self.fetcher_and_worker_syncer.add(1);

            let chunk_channel = self
                .chunk_channel
                .as_ref()
                .expect("the chunk channel is open while the fetcher runs");
            tokio::select! {
                _ = self.finish_ch.closed() => {
                    self.fetcher_and_worker_syncer.done();
                    return Ok(());
                }
                sent = chunk_channel.send(chk_with_memory_usage) => {
                    if sent.is_err() {
                        // glue: every receiver is gone (the executor was dropped, which
                        // aborts this task too): nothing will process the chunk.
                        self.fetcher_and_worker_syncer.done();
                        return Ok(());
                    }
                }
            }

            if let Err(err) = self.check_spill_and_execute().await {
                let _ = self
                    .result_channel
                    .send(RowWithError {
                        row: None,
                        err: Some(err),
                    })
                    .await;
                return Ok(());
            }
            inject_parallel_sort_random_fail(3);
        }
        Ok(())
    }

    // go: SortExec.spillRemainingRowsWhenNeeded L2 (a method of the fetcher task's state)
    fn spill_remaining_rows_when_needed(&self) -> Result<(), Error> {
        if self.spill_helper.is_spill_triggered() {
            return self.spill_helper.spill();
        }
        Ok(())
    }

    // go: SortExec.checkSpillAndExecute L2 (a method of the fetcher task's state [M-WG])
    async fn check_spill_and_execute(&mut self) -> Result<(), Error> {
        if self.spill_helper.is_spill_needed() {
            // Wait for the stop of all workers
            self.fetcher_and_worker_syncer.wait().await;
            return self.spill_helper.spill();
        }
        Ok(())
    }
}

/// The state of the result generator's task (Go's goroutine reads `e.Parallel` and
/// `e.finishCh`).
struct ResultGenerator {
    merger: Option<MultiWayMerger<MemorySource>>,
    sorted_rows_iters: Vec<SharedSortedRowsIter>,
    /// `None` once closed.
    result_channel: Option<ResultSender>,
    finish_ch: FinishCh,
    spill_helper: Arc<ParallelSortSpillHelper>,
    max_chunk_size: usize,
    /// `e.lessRow` (the merge of spilled rows reads it).
    less_row: SortKeys,
}

impl ResultGenerator {
    // glue: e.Parallel.resultChannel while the generator runs
    fn result_channel(&self) -> &ResultSender {
        self.result_channel
            .as_ref()
            .expect("the result channel is open while the generator runs")
    }

    /// Sends an error to the result channel and waits for room, as Go's send does.
    // glue: e.Parallel.resultChannel <- rowWithError{err: err}
    async fn send_error(&self, err: Error) {
        let _ = self
            .result_channel()
            .send(RowWithError {
                row: None,
                err: Some(err),
            })
            .await;
    }

    /// Waits for the workers and the fetcher, then sends the sorted rows. Go's deferred
    /// function resets the iterators, drops the merger and closes the result channel (here
    /// by dropping its last sender).
    // go: SortExec.generateResult L2 (a method of the generator task's state; the channel closes with its last sender [M-WG, M-PANIC, M-CH])
    async fn generate_result(mut self, wait_groups: Vec<Arc<WaitGroup>>) {
        for wait_group in &wait_groups {
            wait_group.wait().await;
        }

        let ret = AssertUnwindSafe(self.generate_result_body())
            .catch_unwind()
            .await;
        if let Err(r) = ret {
            process_panic_and_log(self.result_channel(), r).await;
        }

        for iter in &self.sorted_rows_iters {
            iter.lock().reset(Vec::new());
        }
        self.merger = None;
        self.result_channel = None;
    }

    // glue: the body of generateResult under its deferred recover (M-PANIC)
    async fn generate_result_body(&mut self) {
        if !self.spill_helper.is_spill_triggered() {
            let spill_triggered = match self.generate_result_from_memory().await {
                Ok(t) => t,
                Err(err) => {
                    self.send_error(err).await;
                    return;
                }
            };

            if !spill_triggered {
                return;
            }

            if let Err(err) = self.spill_sorted_rows_in_memory() {
                self.send_error(err).await;
                return;
            }
        }

        if let Err(err) = self.generate_result_from_disk().await {
            self.send_error(err).await;
        }
    }

    /// We call this function to generate result when sorted rows are in memory. Return true
    /// when spill is triggered.
    // go: SortExec.generateResultFromMemory L2 (a method of the generator task's state [M-CH])
    async fn generate_result_from_memory(&mut self) -> Result<bool, Error> {
        let Some(merger) = &mut self.merger else {
            // Sort has been closed
            return Ok(false);
        };
        merger.init()?;

        let max_chunk_size = self.max_chunk_size;
        let mut res_buf: Vec<RowWithError<RowHandle>> = Vec::with_capacity(3);
        let idx: i64 = 0;
        let result_channel = self
            .result_channel
            .as_ref()
            .expect("the result channel is open while the generator runs");
        loop {
            res_buf.clear();
            for _ in 0..max_chunk_size {
                // It's impossible to return error here as rows are in memory
                let row = merger.next().unwrap_or_default();
                let Some(row) = row else {
                    break;
                };
                res_buf.push(RowWithError {
                    row: Some(row),
                    err: None,
                });
            }

            if res_buf.is_empty() {
                return Ok(false);
            }

            for row in res_buf.drain(..) {
                if !send_or_finish(&mut self.finish_ch, result_channel, row).await {
                    return Ok(false);
                }
            }

            inject_parallel_sort_random_fail(3);

            if idx % 1000 == 0 && self.spill_helper.is_spill_needed() {
                return Ok(true);
            }
        }
    }

    /// We call this function when sorted rows are in disk.
    // go: SortExec.generateResultFromDisk L2 (a method of the generator task's state; the spilled chunk is shared by its rows [M-CH, M-ROW])
    async fn generate_result_from_disk(&mut self) -> Result<(), Error> {
        let sorted_rows_in_disk = self.spill_helper.sorted_rows_in_disk();
        let in_disk_num = sorted_rows_in_disk.len();
        if in_disk_num == 0 {
            return Ok(());
        }

        // Spill is triggered only once
        if in_disk_num == 1 {
            let in_disk = &sorted_rows_in_disk[0];
            let chunk_num = in_disk.num_chunks();
            for i in 0..chunk_num {
                let chk = Arc::new(in_disk.get_chunk(i)?);

                inject_parallel_sort_random_fail(1);

                let row_num = chk.num_rows();
                for j in 0..row_num {
                    let row = RowHandle::new(Arc::clone(&chk), chk.get_row(j).idx());
                    let row = RowWithError {
                        row: Some(row),
                        err: None,
                    };
                    let result_channel = self
                        .result_channel
                        .as_ref()
                        .expect("the result channel is open while the generator runs");
                    if !send_or_finish(&mut self.finish_ch, result_channel, row).await {
                        return Ok(());
                    }
                }
            }
            return Ok(());
        }
        self.generate_result_with_multi_way_merge(sorted_rows_in_disk)
            .await
    }

    // go: SortExec.generateResultWithMultiWayMerge L2 (a method of the generator task's state [M-CH])
    async fn generate_result_with_multi_way_merge(
        &mut self,
        sorted_rows_in_disk: Vec<Arc<super::sort_util::DataInDiskByChunks>>,
    ) -> Result<(), Error> {
        let mut multi_way_merge = new_multi_way_merger(
            DiskSource {
                sorted_rows_in_disk,
                cursors: Vec::new(),
            },
            self.less_row.clone(),
        );

        multi_way_merge.init()?;

        loop {
            let row = multi_way_merge.next()?;

            let Some(row) = row else {
                return Ok(());
            };

            let row = RowWithError {
                row: Some(row),
                err: None,
            };
            let result_channel = self
                .result_channel
                .as_ref()
                .expect("the result channel is open while the generator runs");
            if !send_or_finish(&mut self.finish_ch, result_channel, row).await {
                return Ok(());
            }
            inject_parallel_sort_random_fail(1);
        }
    }

    /// Spill rows that are in memory.
    // go: SortExec.spillSortedRowsInMemory L2 (a method of the generator task's state)
    fn spill_sorted_rows_in_memory(&mut self) -> Result<(), Error> {
        let merger = self
            .merger
            .as_mut()
            .expect("invalid memory address or nil pointer dereference");
        self.spill_helper.spill_impl(merger)
    }
}
