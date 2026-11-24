//! go-file: pkg/executor/sortexec/sort.go
//!
//! `SortExec`, the sorting executor.
//!
//! Go has two paths: the unparallel one (all rows of the child in sort partitions, sorted in
//! memory, merged when more than one partition was spilled) and the parallel one (a fetcher,
//! workers and a result generator, `fetchChunksParallel`). Go's `Open` always picks the
//! parallel one (`e.IsUnparallel = false`, sort.go:170); its unit tests switch to the
//! unparallel one after `Open`.
//!
//! This part (VS1-36) ports the unparallel path in memory:
//! - `Open` follows Go: it picks the parallel path, which is not in this part, and stops
//!   there with `Blocked` (D12.5), before it opens the child.
//!   [`SortExec::set_unparallel_for_test`] makes it take the unparallel path, as Go's tests
//!   do.
//! - More than one partition only comes from a spill (`externalSorting`, the multi-way merge
//!   over spilled partitions): a boundary stub (D01.10). Spill itself is not triggered here
//!   (its action is not registered: the session memory tracker of the memory exception A1).
//! - The functions of the parallel path that this part lists (`fetchChunksFromChild`,
//!   `generateResult`, `generateResultFromMemory`) are boundary stubs (D12.5).

// Go identifiers for package constants (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use std::sync::Arc;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_chunk::chunk::Chunk;
use tidb_chunk::compare::{CompareFunc, get_compare_func};
use tidb_chunk::row::Row;
use tidb_expression::{EvalContext, Expr, Schema};
use tidb_memory::action::ActionOnExceed;
use tidb_memory::tracker::{LabelForRowChunks, Tracker, new_tracker};
use tidb_parser::types::FieldType;
use tidb_parser::types::type_str;
use tidb_planner::util::byitem::ByItems;
use tidb_sessionctx::exprctx::ExprContextRef;
use tidb_sessionctx::variable::tidb_vars::EnableTmpStorageOnOOM;

use super::sort_partition::{SortPartition, new_sort_partition};
use super::sort_spill::SortPartitionSpillDiskAction;
use super::sort_util::err_fail_to_add_chunk;
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

/// SortExec represents sorting executor.
///
/// Go's `Parallel` fields (channels, workers, the merger, the spill helper of the parallel
/// sort) are not kept: the parallel path is not in this part, and `Open` stops before it
/// would build them.
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

    /// Whether `finishCh` has been made (`Open`); nothing waits on it here (no worker).
    finish_ch: bool,

    pub file_name_prefix_for_test: String,

    pub unparallel: Unparallel,

    enable_tmp_storage_on_oom: bool,

    /// The session's expression context: Go reads `e.Ctx().GetExprCtx()` when `Next` starts.
    expr_ctx: ExprContextRef,
    /// The byte limit of the session memory tracker when the executor was built: Go reads
    /// `e.Ctx().GetSessionVars().MemTracker.GetBytesLimit()` in `Open`. The Rust session has
    /// no memory tracker (the memory exception A1); its limit is `tidb_mem_quota_query`, the value the
    /// statement set it to.
    session_mem_bytes_limit: i64,
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
        finish_ch: false,
        file_name_prefix_for_test: String::new(),
        unparallel: Unparallel::default(),
        enable_tmp_storage_on_oom: false,
        expr_ctx,
        session_mem_bytes_limit,
        unparallel_for_test: false,
    }
}

impl SortExec {
    /// Makes `Open` take the unparallel path. Go's tests set `IsUnparallel = true` and call
    /// `InitUnparallelModeForTest` after `Open`; here `Open` stops at the parallel path, so
    /// the switch is made before it.
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
    /// channels.
    ///
    /// The channels of the parallel sort are never made here (`Open` stops before the
    /// parallel path), so there is nothing to close. In Go, `Open` always makes them (also
    /// in the unparallel mode of its tests, switched after `Open`), and this closes them
    /// when the sort keys are rejected; no later step reads them in the unparallel mode.
    // go: SortExec.closeChannels L2 (the parallel channels are never made: nothing to close)
    fn close_channels(&mut self) {}

    // go: SortExec.InitUnparallelModeForTest L1
    pub fn init_unparallel_mode_for_test(&mut self) {
        self.unparallel.idx = 0;
        self.unparallel.sort_partitions.clear();
    }

    // glue: stub of SortExec.appendResultToChunkInParallelMode, blocked-by:D12.5 (the parallel sort)
    fn append_result_to_chunk_in_parallel_mode(&mut self, _req: &mut Chunk) -> Result<(), Error> {
        Err(blocked("D12.5", "the parallel sort"))
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

    /// We call this function to generate result when sorted rows are in memory. Return true
    /// when spill is triggered.
    // glue: stub of SortExec.generateResultFromMemory, blocked-by:D12.5 (the result generator of the parallel sort)
    pub fn generate_result_from_memory(&mut self) -> Result<bool, Error> {
        Err(blocked(
            "D12.5",
            "the result generator of the parallel sort",
        ))
    }

    // glue: stub of SortExec.generateResult, blocked-by:D12.5 (the result generator of the parallel sort)
    pub fn generate_result(&mut self) -> Result<(), Error> {
        Err(blocked(
            "D12.5",
            "the result generator of the parallel sort",
        ))
    }

    // go: SortExec.onePartitionSorting L1
    fn one_partition_sorting(&mut self, req: &mut Chunk) -> Result<(), Error> {
        let partition = &mut self.unparallel.sort_partitions[0];
        partition.check_error()?;

        while !req.is_full() {
            let row = partition.get_next_sorted_row()?;

            if row.is_empty() {
                return Ok(());
            }

            req.append_row(row);
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

    // glue: stub of SortExec.fetchChunksParallel, blocked-by:D12.5 (the parallel sort)
    fn fetch_chunks_parallel(&mut self, _ctx: &ExecCtx) -> Result<(), Error> {
        Err(blocked("D12.5", "the parallel sort"))
    }

    /// Fetch chunks from child and put chunks into chunkChannel.
    // glue: stub of SortExec.fetchChunksFromChild, blocked-by:D12.5 (the chunk fetcher of the parallel sort)
    pub fn fetch_chunks_from_child(&mut self, _ctx: &ExecCtx) -> Result<(), Error> {
        Err(blocked("D12.5", "the chunk fetcher of the parallel sort"))
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
    /// statement context); the spill limit is computed from the session limit kept when the
    /// executor was built.
    // go: SortExec.Open L2 (the session values are read from the build; the parallel path stops with Blocked, see the comment in the body)
    async fn open(&mut self, ctx: &ExecCtx) -> Result<(), Error> {
        self.fetched = false;
        self.enable_tmp_storage_on_oom = EnableTmpStorageOnOOM.load(Ordering::SeqCst);
        self.finish_ch = true;

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
            // glue: stub, blocked-by:D12.5 (the parallel sort: Go builds its workers, channels, merger and spill helper here and always takes this path, sort.go:170)
            return Err(blocked("D12.5", "the parallel sort"));
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
        self.append_result_to_chunk_in_parallel_mode(req)
    }

    /// Close implements the Executor Close interface.
    ///
    /// The parallel branch has nothing to release: `Open` stops before it builds the parallel
    /// sort's channels and workers.
    // go: SortExec.Close L2 (the parallel fields are never built, see the comment)
    async fn close(&mut self) -> Result<(), Error> {
        // close(e.finishCh): nothing waits on it here.
        if let Some(spill_action) = &self.unparallel.spill_action {
            spill_action.set_finished();
        }

        if self.is_unparallel {
            for partition in &mut self.unparallel.sort_partitions {
                partition.close()?;
            }
        } else if self.finish_ch && !self.fetched {
            self.fetched = true;
            self.close_channels();
        }

        if let Some(mem_tracker) = &self.mem_tracker {
            mem_tracker.replace_bytes_used(0)?;
        }

        exec::close(self.base.base.meta.children(0).as_mut()).await
    }
}
