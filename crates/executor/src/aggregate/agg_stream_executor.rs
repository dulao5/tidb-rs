//! go-file: pkg/executor/aggregate/agg_stream_executor.go
//!
//! The stream aggregation executor (the aggregation executors D12.3, part A; spec
//! `specs/D12.3-agg-executors/spec.md` §2.1, §4).
//!
//! Differences in shape from Go (spec §4, §6, §7.2):
//! - Memory (the memory exception A1, spec §4.6): the aggregate functions allocate their
//!   partial results' growing state through a `TrackedAlloc` on the executor's tracker, which
//!   records the real bytes. Go's estimates are not kept: `memUsageOfInitialPartialResult`
//!   (the sum of `AllocPartialResult`'s `memDelta`), `pendingMemDelta` with its 1 KiB flush
//!   threshold (`streamAggMemDeltaFlushThreshold`), and the `ReplaceBytesUsed` of
//!   `appendResult2Chunk` that resets the tracker to "child chunk + initial partial
//!   results". Where Go flushes a positive delta (`consumeGroupRows`), the tracker's
//!   `checkpoint` runs the actions. The child chunk's memory is consumed as in Go.
//! - Go's `inputIter` / `inputRow` are written (`OpenSelf`, `consumeCurGroupRowsAndFetchChild`)
//!   and never read: not kept. `groupRows` holds row indices into `childResult` (M-ROW: no
//!   row borrowed across calls); the rows are looked up when the group is consumed.
//! - The session values the executor reads are read when it is built (N2): the evaluation
//!   context of the aggregate functions and `tidb_track_aggregate_memory_usage`.
//! - An aggregate function that `aggfuncs.Build` returned as nil is `None`; Go panics with a
//!   nil pointer dereference when it is used, Rust returns that message as the error
//!   (M-PANIC).
//! - The failpoints are not ported (spec §6 item 14: agg_stream_executor.go:64-68, 102, 190,
//!   194, 212, 245).

use std::sync::Arc;

use async_trait::async_trait;
use tidb_base::Error;
use tidb_chunk::ColAlloc;
use tidb_chunk::chunk::Chunk;
use tidb_chunk::row::Row;
use tidb_expression::expression::EvalState;
use tidb_memory::TrackedAlloc;
use tidb_memory::tracker::{Tracker, new_tracker};
use tidb_sessionctx::exprctx::EvalContextRef;

use crate::aggfuncs::{AggFunc, PartialResult};
use crate::exec::ExecCtx;
use crate::internal::exec::executor::{self as exec, BaseExecutor, BaseExecutorV2, Executor};
use crate::internal::vecgroupchecker::VecGroupChecker;

use super::agg_util::close_base_executor;

/// StreamAggExec deals with all the aggregate functions.
/// It assumes all the input data is sorted by group by key.
/// When Next() is called, it will return a result for the same group.
pub struct StreamAggExec {
    pub(crate) base: BaseExecutor,

    pub(crate) executed: bool,
    /// IsChildReturnEmpty indicates whether the child executor only returns an empty input.
    pub is_child_return_empty: bool,
    pub default_val: Option<Chunk>,
    pub group_checker: VecGroupChecker,
    /// `None` is a function `aggfuncs.Build` returned as nil (Go appends the nil `AggFunc`).
    pub agg_funcs: Vec<Option<Box<dyn AggFunc>>>,
    pub(crate) partial_results: Vec<PartialResult>,
    /// The rows of the current group: indices into `child_result` (M-ROW).
    pub(crate) group_rows: Vec<usize>,
    pub(crate) child_result: Option<Chunk>,

    /// track memory usage.
    pub(crate) mem_tracker: Option<Arc<Tracker>>,
    /// The allocator of the partial results, accounting in `mem_tracker` (A1).
    pub(crate) alloc: Option<TrackedAlloc>,

    /// `e.Ctx().GetExprCtx().GetEvalCtx()`, read when the executor is built (N2).
    pub(crate) eval_ctx: EvalContextRef,
    /// `e.Ctx().GetSessionVars().TrackAggregateMemoryUsage`, read when the executor is built.
    pub(crate) track_aggregate_memory_usage: bool,
    /// The evaluation state of the group-by items and the aggregate arguments (N4 §3.4).
    pub(crate) eval_state: EvalState,
}

/// The fields of Go's `&aggregate.StreamAggExec{...}` literal (buildStreamAggFromChildExec)
/// and the session values the executor reads.
// glue: the StreamAggExec composite literal
pub fn new_stream_agg_exec(
    base: BaseExecutor,
    group_checker: VecGroupChecker,
    eval_ctx: EvalContextRef,
    track_aggregate_memory_usage: bool,
) -> StreamAggExec {
    StreamAggExec {
        base,
        executed: false,
        is_child_return_empty: false,
        default_val: None,
        group_checker,
        agg_funcs: Vec::new(),
        partial_results: Vec::new(),
        group_rows: Vec::new(),
        child_result: None,
        mem_tracker: None,
        alloc: None,
        eval_ctx,
        track_aggregate_memory_usage,
        eval_state: EvalState::default(),
    }
}

/// Go's panic when a nil `AggFunc` (from `aggfuncs.Build`) is called, as an error (M-PANIC).
// glue: a method call on Go's nil AggFunc interface
fn nil_agg_func(f: &Option<Box<dyn AggFunc>>) -> Result<&dyn AggFunc, Error> {
    f.as_deref().ok_or_else(|| {
        Error::plain("runtime error: invalid memory address or nil pointer dereference")
    })
}

#[async_trait]
impl Executor for StreamAggExec {
    // glue: M-SELF
    fn base(&self) -> &BaseExecutorV2 {
        &self.base.base
    }
    // glue: M-SELF
    fn base_mut(&mut self) -> &mut BaseExecutorV2 {
        &mut self.base.base
    }

    /// Open implements the Executor Open interface.
    ///
    /// Go's `defer closeBaseExecutor(&e.BaseExecutor)` recovers a panic of `OpenSelf`; here
    /// `OpenSelf` returns that panic as its error (M-PANIC), and the children are closed
    /// before it is returned (spec §4.5).
    // go: StreamAggExec.Open L2 (no failpoint; the deferred recover is the error path of OpenSelf, M-PANIC)
    async fn open(&mut self, ctx: &ExecCtx) -> Result<(), Error> {
        self.base.base.open(ctx).await?;
        // If panic in Open, the children executor should be closed because they are open.
        if let Err(err) = self.open_self(ctx) {
            return Err(close_base_executor(&mut self.base, err).await);
        }
        Ok(())
    }

    /// Close implements the Executor Close interface.
    ///
    /// The partial results are dropped here, which gives their memory back to the tracker
    /// (A1; Go releases the initial partial results' estimate with the child chunk's).
    // go: StreamAggExec.Close L2 (A1: the partial results' memory is released by dropping them)
    async fn close(&mut self) -> Result<(), Error> {
        if let Some(child_result) = self.child_result.take()
            && let Some(t) = &self.mem_tracker
        {
            // Releasing memory never runs an action.
            let _ = t.consume(-child_result.memory_usage());
        }
        self.drop_partial_results();
        self.group_checker.reset();
        self.base.base.close().await
    }

    /// Next implements the Executor Next interface.
    // go: StreamAggExec.Next L1
    async fn next(&mut self, ctx: &ExecCtx, req: &mut Chunk) -> Result<(), Error> {
        req.reset();
        while !self.executed && !req.is_full() {
            if let Err(err) = self.consume_one_group(ctx, req).await {
                self.executed = true;
                return Err(err);
            }
        }
        Ok(())
    }
}

impl StreamAggExec {
    /// OpenSelf just opens the StreamAggExec.
    ///
    /// The tracker is reset (or created) before the partial results are allocated, because
    /// they are accounted in it as they are allocated (A1); Go allocates them first and
    /// consumes their estimated size after attaching the tracker. Partial results left by an
    /// earlier `Open` without `Close` are dropped first, so that the reset does not forget
    /// bytes they still hold.
    // go: StreamAggExec.OpenSelf L2 (A1: the tracker comes before the partial results, whose estimated memDelta is not kept; no failpoint)
    pub fn open_self(&mut self, ctx: &ExecCtx) -> Result<(), Error> {
        let child_result = exec::try_new_cache_chunk(self.base.base.meta.children(0).as_ref());
        self.executed = false;
        self.is_child_return_empty = true;

        self.drop_partial_results();
        let mem_tracker = match &self.mem_tracker {
            Some(t) => {
                t.reset()?;
                Arc::clone(t)
            }
            None => {
                // bytesLimit <= 0 means no limit, for now we just track the memory footprint
                let t = new_tracker(self.base.base.meta.id(), -1);
                self.mem_tracker = Some(Arc::clone(&t));
                let alloc = TrackedAlloc::new(Arc::clone(&t));
                for f in self.agg_funcs.iter_mut().flatten() {
                    f.set_alloc(ColAlloc::Tracked(alloc.clone()));
                }
                self.alloc = Some(alloc);
                t
            }
        };
        if self.track_aggregate_memory_usage
            && let Some(parent) = &ctx.stmt.mem_tracker
        {
            mem_tracker.attach_to(parent)?;
        }

        self.partial_results = Vec::with_capacity(self.agg_funcs.len());
        for agg_func in &self.agg_funcs {
            let partial_result = nil_agg_func(agg_func)?.alloc_partial_result();
            self.partial_results.push(partial_result);
        }

        mem_tracker.consume(child_result.memory_usage())?;
        self.child_result = Some(child_result);
        Ok(())
    }

    /// Drops the partial results and records the bytes they gave back (A1).
    // glue: A1 release of the partial results (Go's negative Consume / ReplaceBytesUsed)
    fn drop_partial_results(&mut self) {
        self.partial_results = Vec::new();
        if let Some(alloc) = &self.alloc {
            alloc.flush();
        }
    }

    // go: StreamAggExec.consumeOneGroup L1 [M-ROW]
    async fn consume_one_group(&mut self, ctx: &ExecCtx, chk: &mut Chunk) -> Result<(), Error> {
        if self.group_checker.is_exhausted() {
            self.consume_cur_group_rows_and_fetch_child(ctx, chk)
                .await?;
            if self.executed {
                return Ok(());
            }
            let child_result = self.child_result.as_ref().expect(CHILD_RESULT);
            self.group_checker
                .split_into_groups(&self.eval_state, child_result)?;
        }
        let (begin, end) = self.group_checker.get_next_group();
        for i in begin..end {
            self.group_rows.push(i);
        }

        let mut meet_last_group = end == self.child_num_rows();
        while meet_last_group {
            meet_last_group = false;
            self.consume_cur_group_rows_and_fetch_child(ctx, chk)
                .await?;
            if self.executed {
                return Ok(());
            }

            let child_result = self.child_result.as_ref().expect(CHILD_RESULT);
            let is_first_group_same_as_prev = self
                .group_checker
                .split_into_groups(&self.eval_state, child_result)?;

            if is_first_group_same_as_prev {
                let (begin, end) = self.group_checker.get_next_group();
                for i in begin..end {
                    self.group_rows.push(i);
                }
                meet_last_group = end == self.child_num_rows();
            }
        }

        self.consume_group_rows()?;

        self.append_result2_chunk(chk)
    }

    /// The number of rows of `childResult`.
    // glue: e.childResult.NumRows()
    fn child_num_rows(&self) -> usize {
        self.child_result.as_ref().expect(CHILD_RESULT).num_rows()
    }

    /// Go consumes the functions' summed `memDelta` once 1 KiB of it is pending; here the
    /// functions record their real growth through the allocator and the tracker's
    /// checkpoint runs the actions at that place, after every group (A1, spec §4.6).
    // go: StreamAggExec.consumeGroupRows L2 (A1: no memDelta, Go's flush is a checkpoint; no failpoint [M-ROW])
    fn consume_group_rows(&mut self) -> Result<(), Error> {
        if self.group_rows.is_empty() {
            return Ok(());
        }

        let child_result = self.child_result.as_ref().expect(CHILD_RESULT);
        let rows: Vec<Row<'_>> = self
            .group_rows
            .iter()
            .map(|&i| child_result.get_row(i))
            .collect();
        let eval_ctx = &*self.eval_ctx;
        for (i, agg_func) in self.agg_funcs.iter().enumerate() {
            nil_agg_func(agg_func)?.update_partial_result(
                eval_ctx,
                &self.eval_state,
                &rows,
                &mut self.partial_results[i],
            )?;
        }
        if let Some(t) = &self.mem_tracker {
            t.checkpoint()?;
        }
        self.group_rows.clear();
        Ok(())
    }

    /// Go consumes the child chunk's growth before it looks at `exec.Next`'s error (its
    /// `Consume` panics first); the order is kept.
    // go: StreamAggExec.consumeCurGroupRowsAndFetchChild L2 (inputIter / inputRow not kept; no failpoint)
    async fn consume_cur_group_rows_and_fetch_child(
        &mut self,
        ctx: &ExecCtx,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        // Before fetching a new batch of input, we should consume the last group.
        self.consume_group_rows()?;

        let child_result = self.child_result.as_mut().expect(CHILD_RESULT);
        let m_size = child_result.memory_usage();
        let r = exec::next(ctx, self.base.base.meta.children(0).as_mut(), child_result).await;
        if let Some(t) = &self.mem_tracker {
            t.consume(child_result.memory_usage() - m_size)?;
        }
        r?;

        // No more data.
        if child_result.num_rows() == 0 {
            let mut r = Ok(());
            if !self.is_child_return_empty {
                r = self.append_result2_chunk(chk);
            } else if let Some(default_val) = &self.default_val {
                chk.append(default_val, 0, 1);
            }
            self.executed = true;
            return r;
        }
        // Reach here, "e.childrenResults[0].NumRows() > 0" is guaranteed.
        self.is_child_return_empty = false;
        Ok(())
    }

    /// appendResult2Chunk appends result of all the aggregation functions to the
    /// result chunk, and reset the evaluation context for each aggregation.
    ///
    /// Go then resets the tracker to "child chunk + initial partial results"
    /// (`ReplaceBytesUsed`); the reset partial results give their bytes back through the
    /// allocator instead (A1).
    // go: StreamAggExec.appendResult2Chunk L2 (A1: no ReplaceBytesUsed, no pendingMemDelta; no failpoint)
    fn append_result2_chunk(&mut self, chk: &mut Chunk) -> Result<(), Error> {
        let eval_ctx = &*self.eval_ctx;
        for (i, agg_func) in self.agg_funcs.iter().enumerate() {
            let agg_func = nil_agg_func(agg_func)?;
            agg_func.append_final_result2_chunk(eval_ctx, &mut self.partial_results[i], chk)?;
            agg_func.reset_partial_result(&mut self.partial_results[i]);
        }
        if self.agg_funcs.is_empty() {
            chk.set_num_virtual_rows(chk.num_rows() + 1);
        }
        Ok(())
    }
}

/// The message of Go's nil `childResult` (a `Next` before `Open` or after `Close`).
const CHILD_RESULT: &str = "StreamAggExec.Next before Open or after Close (Go: nil chunk)";
