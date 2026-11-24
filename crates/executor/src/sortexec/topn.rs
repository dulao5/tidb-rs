//! go-file: pkg/executor/sortexec/topn.go
//!
//! `TopNExec`: keeps the top `Offset + Count` rows of its child in a heap instead of sorting
//! all of them.
//!
//! - Go loads the first `Offset + Count` rows in `Next`, then runs `executeTopN` on a
//!   goroutine that sends the result rows to `resultChannel`, which `Next` reads. Here
//!   `executeTopN` runs on the executor's task right after the load (mapping rules M-GO,
//!   M-CH), and its result rows go to a queue that `Next` reads; the rows, their order and
//!   the errors are the same.
//! - Spill: the spill action is not registered (the session memory tracker of the memory exception A1), so a
//!   spill is never needed; the spill branches are kept and their functions are boundary
//!   stubs (the disk container, D01.10).
//! - The rank top-n (`RankInfo`, a top-n over rows already ordered by a prefix) is not in this
//!   part: it stops with `Blocked` (D12.5).

// Go identifiers for package constants (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_base::gostd::sort::sort;
use tidb_chunk::chunk::Chunk;
use tidb_chunk::row::Row;
use tidb_expression::ExprRef;
use tidb_memory::tracker::new_tracker;
use tidb_planner::core::physical_plans::PhysicalLimit;
use tidb_sessionctx::variable::tidb_vars::EnableTmpStorageOnOOM;

use super::heap;
use super::sort::{SortExec, SortKeys};
use super::sort_util::RowWithError;
use super::topn_chunk_heap::TopNChunkHeap;
use super::topn_spill::{TopNSpillHelper, new_top_n_spiller_helper};
use crate::exec::ExecCtx;
use crate::internal::exec::executor::{
    self as exec, BaseExecutorV2, Executor, try_new_cache_chunk,
};

/// `topNCompactionFactor`.
const topNCompactionFactor: usize = 4;

/// `rankInfo`: the prefix keys of a rank top-n (not in this part; only the fields the
/// builder sets are kept).
#[derive(Default)]
pub struct RankInfo {
    pub truncate_key_exprs: Vec<ExprRef>,
    pub truncate_key_prefix_char_counts: Vec<isize>,
}

/// TopNExec implements a Top-N algorithm and it is built from a SELECT statement with ORDER
/// BY and LIMIT. Instead of sorting all the rows fetched from the table, it keeps the Top-N
/// elements only in a heap to reduce memory usage.
pub struct TopNExec {
    pub sort: SortExec,
    pub limit: PhysicalLimit,

    /// `resultChannel`: the result rows `executeTopN` produced, read by `Next`.
    result_channel: VecDeque<RowWithError>,
    /// Whether `resultChannel` has been closed (made in `Open`, closed when the result is
    /// complete).
    result_channel_closed: bool,

    /// Whether `finishCh` has been made (`Open`; `nil` means that `Open` is not called).
    finish_ch: bool,

    chk_heap: Option<TopNChunkHeap>,

    spill_helper: Option<TopNSpillHelper>,

    /// Normally, heap will be stored in memory after it has been built. However, other
    /// executors may trigger topn spill after the heap is built and inMemoryThenSpillFlag
    /// will be set to true at this time.
    in_memory_then_spill_flag: bool,

    /// Topn executor has two stage: 1. Building heap, in this stage all received rows will be
    /// inserted into heap. 2. Updating heap, in this stage only rows that is smaller than the
    /// heap top could be inserted and we will drop the heap top.
    ///
    /// This variable is only used for test.
    is_spill_triggered_in_stage1_for_test: bool,
    is_spill_triggered_in_stage2_for_test: bool,

    pub concurrency: isize,

    /// ColumnIdxsUsedByChild keep column indexes of child executor used for inline projection.
    pub column_idxs_used_by_child: Vec<usize>,

    pub rank_info: RankInfo,
}

/// A top-n executor over `sort` (Go's composite literal in `buildTopN`).
// glue: Go composite literal sortexec.TopNExec{...} (buildTopN)
pub fn new_top_n_exec(sort: SortExec, limit: PhysicalLimit, concurrency: isize) -> TopNExec {
    TopNExec {
        sort,
        limit,
        result_channel: VecDeque::new(),
        result_channel_closed: false,
        finish_ch: false,
        chk_heap: None,
        spill_helper: None,
        in_memory_then_spill_flag: false,
        is_spill_triggered_in_stage1_for_test: false,
        is_spill_triggered_in_stage2_for_test: false,
        concurrency,
        column_idxs_used_by_child: Vec::new(),
        rank_info: RankInfo::default(),
    }
}

impl SortKeys {
    // go: TopNExec.greaterRow L2 (a method of the sort keys, see SortKeys)
    pub fn greater_row(&self, row_i: Row<'_>, row_j: Row<'_>) -> bool {
        for (i, &col_idx) in self.key_columns.iter().enumerate() {
            let cmp_func = &self.key_cmp_funcs[i];
            let mut cmp = cmp_func(row_i, col_idx, row_j, col_idx);
            if self.descs[i] {
                cmp = cmp.wrapping_neg();
            }
            if cmp > 0 {
                return true;
            } else if cmp < 0 {
                return false;
            }
        }
        false
    }
}

impl TopNExec {
    /// `close(e.resultChannel)`.
    // glue: Go close(chan) on the result queue
    fn close_result_channel(&mut self) {
        assert!(!self.result_channel_closed, "close of closed channel");
        self.result_channel_closed = true;
    }

    // glue: e.chkHeap (a nil pointer dereference panics in Go)
    fn heap(&self) -> &TopNChunkHeap {
        self.chk_heap
            .as_ref()
            .expect("invalid memory address or nil pointer dereference")
    }

    // glue: e.chkHeap (a nil pointer dereference panics in Go)
    fn heap_mut(&mut self) -> &mut TopNChunkHeap {
        self.chk_heap
            .as_mut()
            .expect("invalid memory address or nil pointer dereference")
    }

    // glue: e.spillHelper (a nil pointer dereference panics in Go)
    fn spill_helper(&self) -> &TopNSpillHelper {
        self.spill_helper
            .as_ref()
            .expect("invalid memory address or nil pointer dereference")
    }

    /// Go recovers a panic of the load and sends it to the result channel; no panic is
    /// recovered here (mapping rule M-PANIC). `executeTopN` runs before this returns (Go starts
    /// it on a goroutine).
    // go: TopNExec.fetchChunks L2 (executeTopN runs inline, see the file comment [M-GO, M-PANIC])
    async fn fetch_chunks(&mut self, ctx: &ExecCtx) -> Result<(), Error> {
        if self.limit.count == 0 {
            self.close_result_channel();
            return Ok(());
        }

        if !self.rank_info.truncate_key_exprs.is_empty() {
            if let Err(err) = self.load_chunks_until_total_limit_for_rank_top_n(ctx) {
                self.close_result_channel();
                return Err(err);
            }

            self.execute_rank_top_n();
        } else {
            if let Err(err) = self.load_chunks_until_total_limit(ctx).await {
                self.close_result_channel();
                return Err(err);
            }
            self.execute_top_n(ctx).await;
        }

        Ok(())
    }

    // go: TopNExec.initBeforeLoadingChunks L2 (the rank top-n stops with Blocked, see the comment in the body)
    fn init_before_loading_chunks(&mut self) -> Result<(), Error> {
        let eval_ctx = self.sort_expr_eval_ctx();
        self.sort.init_compare_funcs(eval_ctx.as_ref())?;

        self.sort.build_key_columns()?;

        if !self.rank_info.truncate_key_exprs.is_empty() {
            // glue: stub, blocked-by:D12.5 (the rank top-n: the collators and column indexes of the prefix keys)
            return Err(blocked("D12.5", "the rank top-n"));
        }

        let ret_types = self.ret_field_types().to_vec();
        let init_cap = self.init_cap();
        let max_chunk_size = self.max_chunk_size();
        let keys = self.sort.sort_keys();
        let mem_tracker = Arc::clone(
            self.sort
                .mem_tracker
                .as_ref()
                .expect("Open made the tracker"),
        );
        let total_limit = self.limit.offset.wrapping_add(self.limit.count);
        let idx = self.limit.offset as isize;
        self.heap_mut().init(
            &ret_types,
            init_cap,
            max_chunk_size,
            keys,
            &mem_tracker,
            total_limit,
            idx,
            &ret_types,
        )
    }

    /// The evaluation context of the session (Go's `e.Ctx().GetExprCtx().GetEvalCtx()`).
    // glue: e.Ctx().GetExprCtx().GetEvalCtx() (kept by the sort executor)
    fn sort_expr_eval_ctx(&self) -> tidb_sessionctx::exprctx::EvalContextRef {
        self.sort.expr_ctx().get_eval_ctx()
    }

    // go: TopNExec.loadChunksUntilTotalLimit L2 (failpoint TopNRandomFail not ported: no failpoints; the list's error is returned [M-INT, M-MEM])
    async fn load_chunks_until_total_limit(&mut self, ctx: &ExecCtx) -> Result<(), Error> {
        self.init_before_loading_chunks()?;

        while (self.heap().chunks().len() as u64) < self.heap().total_limit {
            let mut src_chk = try_new_cache_chunk(self.child());
            // adjust required rows by total limit
            let required =
                self.heap()
                    .total_limit
                    .wrapping_sub(self.heap().chunks().len() as u64) as isize;
            src_chk.set_required_rows(required, self.max_chunk_size() as usize);
            exec::next(ctx, self.child_mut(), &mut src_chk).await?;
            if src_chk.num_rows() == 0 {
                break;
            }
            self.heap_mut().chunks_mut().add(src_chk)?;
            if self.spill_helper().is_spill_needed() {
                self.is_spill_triggered_in_stage1_for_test = true;
                break;
            }
        }

        self.heap_mut().init_ptrs()
    }

    // glue: stub of TopNExec.loadChunksUntilTotalLimitForRankTopN, blocked-by:D12.5 (the rank top-n)
    fn load_chunks_until_total_limit_for_rank_top_n(
        &mut self,
        _ctx: &ExecCtx,
    ) -> Result<(), Error> {
        Err(blocked("D12.5", "the rank top-n"))
    }

    // go: TopNExec.executeTopNWhenNoSpillTriggered L2 (failpoint TopNRandomFail not ported: no failpoints; the heap's errors are returned [M-MEM])
    async fn execute_top_n_when_no_spill_triggered(&mut self, ctx: &ExecCtx) -> Result<(), Error> {
        if self.spill_helper().is_spill_needed() {
            self.is_spill_triggered_in_stage2_for_test = true;
            return Ok(());
        }

        let mut child_row_chk = try_new_cache_chunk(self.child());
        loop {
            if self.spill_helper().is_spill_needed() {
                self.is_spill_triggered_in_stage2_for_test = true;
                return Ok(());
            }

            exec::next(ctx, self.child_mut(), &mut child_row_chk).await?;

            if child_row_chk.num_rows() == 0 {
                break;
            }

            self.heap_mut().process_chk(&child_row_chk)?;

            if self.heap().chunks().len() > self.heap().row_ptrs.len() * topNCompactionFactor {
                let ret_types = self.ret_field_types().to_vec();
                let init_cap = self.init_cap();
                let max_chunk_size = self.max_chunk_size();
                self.heap_mut()
                    .do_compaction(&ret_types, init_cap, max_chunk_size)?;
            }
        }

        // slices.SortFunc(e.chkHeap.rowPtrs, e.chkHeap.keyColumnsCompare): the same pattern-
        // defeating quicksort as sort.Sort, with less(a, b) = cmp(a, b) < 0.
        let chk_heap = self.heap_mut();
        let mut row_ptrs = std::mem::take(&mut chk_heap.row_ptrs);
        sort(&mut row_ptrs, |&a, &b| {
            chk_heap.key_columns_compare(a, b) < 0
        });
        chk_heap.row_ptrs = row_ptrs;
        Ok(())
    }

    /// The fetcher of the spill stage.
    // glue: stub of TopNExec.fetchChunksFromChild, blocked-by:D01.10 (the chunk fetcher of the top-n spill)
    pub fn fetch_chunks_from_child(&mut self, _ctx: &ExecCtx) -> Result<(), Error> {
        Err(blocked("D01.10", "the chunk fetcher of the top-n spill"))
    }

    /// Spill the heap which is in TopN executor.
    // glue: stub of TopNExec.spillTopNExecHeap, blocked-by:D01.10 (spilling the top-n heap to disk)
    fn spill_top_n_exec_heap(&mut self) -> Result<(), Error> {
        Err(blocked("D01.10", "spilling the top-n heap to disk"))
    }

    // glue: stub of TopNExec.executeTopNWhenSpillTriggered, blocked-by:D01.10 (the spill stage of the top-n)
    async fn execute_top_n_when_spill_triggered(&mut self, _ctx: &ExecCtx) -> Result<(), Error> {
        Err(blocked("D01.10", "the spill stage of the top-n"))
    }

    /// Runs on the executor's task (Go: a goroutine started by `fetchChunks`); the result
    /// queue is closed when it returns, as Go's deferred `close`.
    // go: TopNExec.executeTopN L2 (runs inline, no recover; see the file comment [M-GO, M-CH, M-PANIC])
    async fn execute_top_n(&mut self, ctx: &ExecCtx) {
        'body: {
            heap::init(self.heap_mut());
            while self.heap().row_ptrs.len() as u64 > self.heap().total_limit {
                // The number of rows we loaded may exceeds total limit, remove greatest rows by Pop.
                heap::pop(self.heap_mut());
            }

            if let Err(err) = self.execute_top_n_when_no_spill_triggered(ctx).await {
                self.result_channel.push_back(RowWithError {
                    row: None,
                    err: Some(err),
                });
                break 'body;
            }

            if self.spill_helper().is_spill_needed()
                && let Err(err) = self.execute_top_n_when_spill_triggered(ctx).await
            {
                self.result_channel.push_back(RowWithError {
                    row: None,
                    err: Some(err),
                });
                break 'body;
            }

            self.generate_top_n_results();
        }
        // defer: close(e.resultChannel)
        self.close_result_channel();
    }

    // glue: stub of TopNExec.executeRankTopN, blocked-by:D12.5 (the rank top-n)
    fn execute_rank_top_n(&mut self) {
        self.result_channel.push_back(RowWithError {
            row: None,
            err: Some(blocked("D12.5", "the rank top-n")),
        });
        self.close_result_channel();
    }

    /// Return true when spill is triggered.
    // go: TopNExec.generateTopNResultsWhenNoSpillTriggered L1 [M-CH]
    fn generate_top_n_results_when_no_spill_triggered(&mut self) -> bool {
        let row_ptr_num = self.heap().row_ptrs.len() as isize;
        while self.heap().idx < row_ptr_num {
            let idx = self.heap().idx;
            if idx % 10 == 0 && self.spill_helper().is_spill_needed() {
                return true;
            }
            let ptr = self.heap().row_ptrs[idx as usize];
            self.result_channel.push_back(RowWithError {
                row: Some(ptr),
                err: None,
            });
            self.heap_mut().idx += 1;
        }
        false
    }

    // glue: stub of TopNExec.generateTopNResultsWhenSpillTriggered, blocked-by:D01.10 (reading the spilled top-n rows back)
    fn generate_top_n_results_when_spill_triggered(&mut self) -> Result<(), Error> {
        Err(blocked("D01.10", "reading the spilled top-n rows back"))
    }

    // go: TopNExec.generateTopNResults L1 [M-CH]
    fn generate_top_n_results(&mut self) {
        if !self.spill_helper().is_spill_triggered() {
            if !self.generate_top_n_results_when_no_spill_triggered() {
                return;
            }

            if let Err(err) = self.spill_top_n_exec_heap() {
                self.result_channel.push_back(RowWithError {
                    row: None,
                    err: Some(err),
                });
            }

            self.in_memory_then_spill_flag = true;
        }

        if let Err(err) = self.generate_top_n_results_when_spill_triggered() {
            self.result_channel.push_back(RowWithError {
                row: None,
                err: Some(err),
            });
        }
    }

    /// IsSpillTriggeredForTest shows if spill is triggered, used for test.
    // go: TopNExec.IsSpillTriggeredForTest L1
    pub fn is_spill_triggered_for_test(&self) -> bool {
        self.spill_helper().is_spill_triggered()
    }

    /// GetIsSpillTriggeredInStage1ForTest shows if spill is triggered in stage 1, only used
    /// for test.
    // go: TopNExec.GetIsSpillTriggeredInStage1ForTest L1
    pub fn get_is_spill_triggered_in_stage1_for_test(&self) -> bool {
        self.is_spill_triggered_in_stage1_for_test
    }

    /// GetIsSpillTriggeredInStage2ForTest shows if spill is triggered in stage 2, only used
    /// for test.
    // go: TopNExec.GetIsSpillTriggeredInStage2ForTest L1
    pub fn get_is_spill_triggered_in_stage2_for_test(&self) -> bool {
        self.is_spill_triggered_in_stage2_for_test
    }

    /// GetInMemoryThenSpillFlagForTest shows if results are in memory before they are
    /// spilled, only used for test.
    // go: TopNExec.GetInMemoryThenSpillFlagForTest L1
    pub fn get_in_memory_then_spill_flag_for_test(&self) -> bool {
        self.in_memory_then_spill_flag
    }

    // glue: e.Children(0)
    fn child(&self) -> &dyn Executor {
        self.sort.base.base.meta.all_children()[0].as_ref()
    }

    // glue: e.Children(0)
    fn child_mut(&mut self) -> &mut dyn Executor {
        self.sort.base.base.meta.children(0).as_mut()
    }
}

#[async_trait]
impl Executor for TopNExec {
    // glue: M-SELF (Go struct embedding)
    fn base(&self) -> &BaseExecutorV2 {
        &self.sort.base.base
    }
    // glue: M-SELF (Go struct embedding)
    fn base_mut(&mut self) -> &mut BaseExecutorV2 {
        &mut self.sort.base.base
    }

    /// Open implements the Executor Open interface.
    ///
    /// The workers and the spill action Go builds when `tidb_enable_tmp_storage_on_oom` is on
    /// serve the spill only; they are not built, and the action is not registered (see the
    /// comment in the body). The spill helper keeps the spill state.
    // go: TopNExec.Open L2 (the spill workers and action are not built, and a rank top-n stops with Blocked; see the comments in the body)
    async fn open(&mut self, ctx: &ExecCtx) -> Result<(), Error> {
        let mem_tracker = new_tracker(self.sort.base.base.meta.id(), -1);
        if let Some(stmt_tracker) = &ctx.stmt.mem_tracker {
            mem_tracker.attach_to(stmt_tracker)?;
        }
        self.sort.mem_tracker = Some(Arc::clone(&mem_tracker));

        self.sort.fetched = false;
        let mut chk_heap = TopNChunkHeap::default();
        chk_heap.mem_tracker = Some(mem_tracker);
        self.chk_heap = Some(chk_heap);
        self.heap_mut().idx = 0;

        self.finish_ch = true;
        self.result_channel = VecDeque::new();
        self.result_channel_closed = false;
        self.in_memory_then_spill_flag = false;
        self.is_spill_triggered_in_stage1_for_test = false;
        self.is_spill_triggered_in_stage2_for_test = false;

        if !self.rank_info.truncate_key_prefix_char_counts.is_empty() {
            // `e.typeCtx` of the rank top-n. It stops here, before the child opens, so the
            // statement has read nothing yet (a boundary after Open would end it on this
            // node instead of handing it to Go TiDB).
            // glue: stub, blocked-by:D12.5 (the rank top-n: e.typeCtx and the prefix keys)
            return Err(blocked("D12.5", "the rank top-n"));
        }

        if EnableTmpStorageOnOOM.load(Ordering::SeqCst) {
            let disk_tracker = new_tracker(self.sort.base.base.meta.id(), -1);
            if let Some(stmt_tracker) = &ctx.stmt.disk_tracker {
                disk_tracker.attach_to(stmt_tracker)?;
            }
            self.sort.disk_tracker = Some(disk_tracker);

            // The `Concurrency` workers (topNWorker, each with its own heap) and the spill
            // action serve the spill only, which starts when the action runs.
            // glue: blocked-by:D01.10 (e.Ctx().GetSessionVars().MemTracker.FallbackOldAndSetNewAction(e.spillAction): the spill to disk is not ported, and the session memory tracker it hangs on is not in the Rust session yet (exception A1; the missing session memory tracker is issue #344), so a top-n never spills)
            self.spill_helper = Some(new_top_n_spiller_helper());
        } else {
            self.spill_helper = Some(new_top_n_spiller_helper());
        }

        exec::open(ctx, self.child_mut()).await
    }

    /// Next implements the Executor Next interface. See Go's comment for the procedure of
    /// topn when spill is triggered.
    ///
    /// The result queue is complete when `fetchChunks` returns (see the file comment), so an
    /// empty queue is a closed channel.
    // go: TopNExec.Next L1 [M-CH]
    async fn next(&mut self, ctx: &ExecCtx, req: &mut Chunk) -> Result<(), Error> {
        req.reset();
        if !self.sort.fetched {
            self.sort.fetched = true;
            self.fetch_chunks(ctx).await?;
        }

        if !req.is_full() {
            let num_to_append = req.required_rows() - req.num_rows();
            for _ in 0..num_to_append {
                let Some(row) = self.result_channel.pop_front() else {
                    debug_assert!(self.result_channel_closed);
                    return Ok(());
                };
                if let Some(err) = row.err {
                    return Err(err);
                }
                let ptr = row.row.expect("a result row without an error has a row");
                req.append_row(self.heap().chunks().get_row(ptr));
            }
        }
        Ok(())
    }

    /// Close implements the Executor Close interface.
    // go: TopNExec.Close L1 [M-CH]
    async fn close(&mut self) -> Result<(), Error> {
        // `e.finishCh == nil` means that `Open` is not called.
        if !self.finish_ch {
            return exec::close(self.child_mut()).await;
        }

        // close(e.finishCh): nothing waits on it here.
        if !self.sort.fetched {
            self.sort.fetched = true;
            self.close_result_channel();
            return exec::close(self.child_mut()).await;
        }

        // Wait for the finish of all tasks: channel.Clear(e.resultChannel).
        self.result_channel.clear();

        self.chk_heap = None;

        if let Some(spill_helper) = self.spill_helper.take() {
            spill_helper.close();
        }

        if let Some(mem_tracker) = &self.sort.mem_tracker {
            mem_tracker.replace_bytes_used(0)?;
        }

        exec::close(self.child_mut()).await
    }
}
