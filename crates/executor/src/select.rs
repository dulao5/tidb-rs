//! go-file: pkg/executor/select.go
//!
//! `ResetContextOfStmt` and `isWeakConsistencyRead` (block D12.2), ported ahead of VS1-35
//! by VS1-14: the session calls them before `Compile` (spec `specs/D05.3-session/spec.md`
//! §2.4). The executors of the file come with VS1-35.
//!
//! Not ported here, each the business of another block (spec §2.4): the session memory and
//! disk trackers and the OOM action (`SessionVars.MemTracker`, A1), the ANALYZE tracker
//! (ANALYZE is delegated to Go TiDB), the SQL digest of an `EXECUTE`'s statement (not
//! computed, `parser.NormalizeDigest`), Top SQL and pprof labels (D14.2), the index usage collector (D11.2), the CTE / union-scan maps (their
//! executors are not ported: nothing to clear), and the plan cache switches (D13.1). Statements outside the
//! Rust AST (`Stmt::Unported`: INSERT, UPDATE, DELETE, EXPLAIN, ...) are a boundary stub
//! (D02.2): the new stack only runs SELECT (VS1 router).

// Constants keep their Go names (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use tidb_base::dbterror::exeerrors::ErrMaxExecTimeExceeded;
use tidb_chunk::chunk::{self, Chunk};
use tidb_chunk::chunk_util::ColumnSwapHelper;
use tidb_expression::ExprRef;
use tidb_expression::chunk_executor::{vectorizable, vectorized_filter};
use tidb_expression::eval::RpnState;
use tidb_expression::expression::{EvalState, eval_bool};
use tidb_kv::kv::Snapshot;
use tidb_memory::action::{ActionOnExceed, ActionRef, BaseOOMAction, DefPanicPriority};
use tidb_memory::tracker::{
    LabelForGlobalAnalyzeMemory, LabelForGlobalMemory, LabelForGlobalStorage, Tracker, new_tracker,
};
use tidb_sessionctx::exprctx::EvalContextRef;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_base::mysql::{NoPriority, PriorityEnum, SQLMode, ServerStatusCursorExists};
use tidb_execdetails::execdetails::RuntimeStatsColl;
use tidb_memory::tracker::LabelForSQLText;
use tidb_parser::ast;
use tidb_planner::core::plan_cache_utils::get_prepared_stmt;
use tidb_sessionctx::errctx;
use tidb_sessionctx::stmtctx::{StatementContext, allocate_task_id};
use tidb_sessionctx::variable::session::SessionVars;

use crate::exec::ExecCtx;
use crate::glue::SessionContext;
use crate::internal::exec::executor::{self as exec, BaseExecutor, BaseExecutorV2, Executor};

// Panic messages of the global trackers' action (Go `const` block of select.go).
/// globalPanicStorageExceed represents the panic message when out of storage quota.
const globalPanicStorageExceed: &str = "Out Of Quota For Local Temporary Space!";
/// globalPanicMemoryExceed represents the panic message when out of memory limit.
const globalPanicMemoryExceed: &str = "Out Of Global Memory Limit!";
/// globalPanicAnalyzeMemoryExceed represents the panic message when out of analyze memory limit.
const globalPanicAnalyzeMemoryExceed: &str = "Out Of Global Analyze Memory Limit!";

/// globalPanicOnExceed panics when GlobalDisTracker storage usage exceeds storage quota.
///
/// Go installs it on the process-wide trackers in `init()` (`GlobalMemoryUsageTracker`,
/// `GlobalDiskUsageTracker`, `GlobalAnalyzeMemoryTracker`), which are not part of this port
/// yet (memory control, A1): the action exists, nothing installs it.
#[derive(Default)]
pub struct GlobalPanicOnExceed {
    base: BaseOOMAction,
    /// For synchronization.
    mutex: parking_lot::Mutex<()>,
}

impl ActionOnExceed for GlobalPanicOnExceed {
    /// Action panics when storage usage exceeds storage quota.
    ///
    /// Go's `panic(msg)` is the error its recovery (`util.GetRecoverError`) makes of it: the
    /// message as a plain error (M-PANIC, as `PanicOnExceed.Action` of the memory crate).
    // go: globalPanicOnExceed.Action L1 [M-PANIC, M-LOCK]
    fn action(&self, t: &Arc<Tracker>) -> Result<(), Error> {
        let _guard = self.mutex.lock();
        let msg = match t.label() {
            LabelForGlobalStorage => globalPanicStorageExceed,
            LabelForGlobalMemory => globalPanicMemoryExceed,
            LabelForGlobalAnalyzeMemory => globalPanicAnalyzeMemoryExceed,
            _ => "Out of Unknown Resource Quota!",
        };
        // TODO(hawkingrei): should return error instead.
        Err(Error::plain(msg))
    }
    // glue: Go embeds BaseOOMAction; forwarded to its SetFallback
    fn set_fallback(&self, a: Option<ActionRef>) {
        self.base.set_fallback(a)
    }
    // glue: Go embeds BaseOOMAction; forwarded to its GetFallback
    fn get_fallback(&self) -> Option<ActionRef> {
        self.base.get_fallback()
    }
    /// GetPriority get the priority of the Action
    // go: globalPanicOnExceed.GetPriority L1
    fn get_priority(&self) -> i64 {
        DefPanicPriority
    }
    // glue: Go embeds BaseOOMAction; forwarded to its SetFinished
    fn set_finished(&self) {
        self.base.set_finished()
    }
    // glue: Go embeds BaseOOMAction; forwarded to its IsFinished
    fn is_finished(&self) -> bool {
        self.base.is_finished()
    }
}

/// checkMaxExecutionTimeExceeded validates whether the current statement already hit the
/// max_execution_time limit. Centralized here so different executors share the same behaviour.
///
/// Go reads the session (`sctx`, which may be nil) and its process info
/// (`sctx.ShowProcess()`); the executors do not hold the session (N2), so the caller passes
/// the session variables and the process info's start time (`None`: no process info, or a
/// zero time). Its callers are the locking reads (`SELECT ... FOR UPDATE`, the write path X),
/// none of which is ported yet.
// go: checkMaxExecutionTimeExceeded L2 (the session variables and the process start time are passed in: executors do not hold the session, N2)
pub fn check_max_execution_time_exceeded(
    sess_vars: Option<&SessionVars>,
    process_time: Option<SystemTime>,
) -> Result<(), Error> {
    let Some(sess_vars) = sess_vars else {
        return Ok(());
    };

    if !sess_vars.stmt_ctx().in_select_stmt {
        return Ok(());
    }

    let max_exec_time_ms = sess_vars.get_max_execution_time();
    if max_exec_time_ms == 0 {
        return Ok(());
    }

    let Some(process_time) = process_time else {
        return Ok(());
    };

    // time.Since: a start time in the future gives a negative duration (never exceeded).
    let elapsed = SystemTime::now()
        .duration_since(process_time)
        .unwrap_or(Duration::ZERO);
    if elapsed >= Duration::from_millis(max_exec_time_ms) {
        return Err(ErrMaxExecTimeExceeded.gen_with_stack_by_args(Vec::new()));
    }

    Ok(())
}

/// LimitExec represents limit executor
/// It ignores 'Offset' rows from src, then returns 'Count' rows at maximum.
pub struct LimitExec {
    pub(crate) base: BaseExecutor,

    pub(crate) begin: u64,
    pub(crate) end: u64,
    pub(crate) cursor: u64,

    /// meetFirstBatch represents whether we have met the first valid Chunk from child.
    pub(crate) meet_first_batch: bool,

    /// Go's `nil` (before `Open`, after `Close`) is `None`.
    pub(crate) child_result: Option<Chunk>,

    /// columnIdxsUsedByChild keep column indexes of child executor used for inline projection
    pub(crate) column_idxs_used_by_child: Option<Vec<usize>>,
    pub(crate) column_swap_helper: Option<ColumnSwapHelper>,
    // Go's `span` (opentracing) is not kept: tidb-rs has no opentracing spans.
}

#[async_trait]
impl Executor for LimitExec {
    // glue: M-SELF
    fn base(&self) -> &BaseExecutorV2 {
        &self.base.base
    }
    // glue: M-SELF
    fn base_mut(&mut self) -> &mut BaseExecutorV2 {
        &mut self.base.base
    }

    /// Next implements the Executor Next interface.
    ///
    /// The child's chunk is taken out of the executor while `Next` runs, so that
    /// `adjustRequiredRows` (a method of the executor) can be given it; it is put back
    /// whatever `Next` returns.
    // go: LimitExec.Next L1
    async fn next(&mut self, ctx: &ExecCtx, req: &mut Chunk) -> Result<(), Error> {
        req.reset();
        if self.cursor >= self.end {
            return Ok(());
        }
        let mut child_result = self
            .child_result
            .take()
            .expect("LimitExec.Next before Open or after Close (Go: nil chunk)");
        let r = self.next_with(ctx, req, &mut child_result).await;
        self.child_result = Some(child_result);
        r
    }

    /// Open implements the Executor Open interface.
    // go: LimitExec.Open L2 (no opentracing span)
    async fn open(&mut self, ctx: &ExecCtx) -> Result<(), Error> {
        self.base.base.open(ctx).await?;
        self.child_result = Some(exec::try_new_cache_chunk(
            self.base.base.meta.children(0).as_ref(),
        ));
        self.cursor = 0;
        self.meet_first_batch = self.begin == 0;
        Ok(())
    }

    /// Close implements the Executor Close interface.
    // go: LimitExec.Close L2 (no opentracing span: only the log)
    async fn close(&mut self) -> Result<(), Error> {
        let start = Instant::now();

        self.child_result = None;
        let err = self.base.base.close().await;

        let elapsed = start.elapsed();
        if elapsed > Duration::from_millis(1) {
            log::info!("limit executor close takes a long time, elapsed={elapsed:?}");
        }
        err
    }
}

impl LimitExec {
    /// The body of `Next` after its first check, with the child's chunk (see `Next`).
    // go-part: LimitExec.Next
    async fn next_with(
        &mut self,
        ctx: &ExecCtx,
        req: &mut Chunk,
        child_result: &mut Chunk,
    ) -> Result<(), Error> {
        let max_chunk_size = self.max_chunk_size().max(0) as usize;
        while !self.meet_first_batch {
            // transfer req's requiredRows to childResult and then adjust it in childResult
            child_result.set_required_rows(req.required_rows() as isize, max_chunk_size);
            self.adjust_required_rows(child_result);
            exec::next(ctx, self.base.base.meta.children(0).as_mut(), child_result).await?;
            let batch_size = child_result.num_rows() as u64;
            // no more data.
            if batch_size == 0 {
                return Ok(());
            }
            let new_cursor = self.cursor.wrapping_add(batch_size);
            if new_cursor >= self.begin {
                self.meet_first_batch = true;
                let (begin, mut end) = (self.begin.wrapping_sub(self.cursor), batch_size);
                if new_cursor > self.end {
                    end = self.end.wrapping_sub(self.cursor);
                }
                self.cursor = self.cursor.wrapping_add(end);
                if begin == end {
                    break;
                }
                match &self.column_idxs_used_by_child {
                    Some(cols) => {
                        req.append(&child_result.prune(cols), begin as usize, end as usize)
                    }
                    None => req.append(child_result, begin as usize, end as usize),
                }
                return Ok(());
            }
            self.cursor = self.cursor.wrapping_add(batch_size);
        }
        child_result.reset();
        child_result.set_required_rows(req.required_rows() as isize, max_chunk_size);
        self.adjust_required_rows(child_result);
        exec::next(ctx, self.base.base.meta.children(0).as_mut(), child_result).await?;
        let mut batch_size = child_result.num_rows() as u64;
        // no more data.
        if batch_size == 0 {
            return Ok(());
        }
        if self.cursor.wrapping_add(batch_size) > self.end {
            child_result.truncate_to(self.end.wrapping_sub(self.cursor) as usize);
            batch_size = self.end.wrapping_sub(self.cursor);
        }
        self.cursor = self.cursor.wrapping_add(batch_size);

        match &self.column_swap_helper {
            Some(helper) if self.column_idxs_used_by_child.is_some() => {
                helper.swap_columns(child_result, req)?;
            }
            _ => req.swap_columns(child_result),
        }
        Ok(())
    }

    /// Go converts the unsigned cursor to `int` (M-INT: wrapping, as Go).
    // go: LimitExec.adjustRequiredRows L1 [M-INT]
    pub(crate) fn adjust_required_rows<'a>(&self, chk: &'a mut Chunk) -> &'a mut Chunk {
        // the limit of maximum number of rows the LimitExec should read
        let limit_total = self.end.wrapping_sub(self.cursor) as isize;

        let limit_required = if self.cursor < self.begin {
            // if cursor is less than begin, it have to read (begin-cursor) rows to ignore
            // and then read chk.RequiredRows() rows to return,
            // so the limit is (begin-cursor)+chk.RequiredRows().
            (self.begin as isize)
                .wrapping_sub(self.cursor as isize)
                .wrapping_add(chk.required_rows() as isize)
        } else {
            // if cursor is equal or larger than begin, just read chk.RequiredRows() rows to return.
            chk.required_rows() as isize
        };

        let max_chunk_size = self.max_chunk_size().max(0) as usize;
        chk.set_required_rows(limit_total.min(limit_required), max_chunk_size)
    }
}

/// TableDualExec represents a dual table executor.
pub struct TableDualExec {
    pub(crate) base: BaseExecutorV2,

    /// numDualRows can only be 0 or 1.
    pub(crate) num_dual_rows: isize,
    pub(crate) num_returned: isize,
}

#[async_trait]
impl Executor for TableDualExec {
    // glue: M-SELF
    fn base(&self) -> &BaseExecutorV2 {
        &self.base
    }
    // glue: M-SELF
    fn base_mut(&mut self) -> &mut BaseExecutorV2 {
        &mut self.base
    }

    /// Open implements the Executor Open interface.
    // go: TableDualExec.Open L1
    async fn open(&mut self, _ctx: &ExecCtx) -> Result<(), Error> {
        self.num_returned = 0;
        Ok(())
    }

    /// Next implements the Executor Next interface.
    // go: TableDualExec.Next L1
    async fn next(&mut self, _ctx: &ExecCtx, req: &mut Chunk) -> Result<(), Error> {
        req.reset();
        if self.num_returned >= self.num_dual_rows {
            return Ok(());
        }
        let schema_len = self.base.meta.get_schema().map_or(0, |s| s.len());
        if schema_len == 0 {
            req.set_num_virtual_rows(1);
        } else {
            for i in 0..schema_len {
                req.append_null(i);
            }
        }
        self.num_returned = self.num_dual_rows;
        Ok(())
    }
}

/// selectionExecutorContext is the execution context for the `SelectionExec`
pub struct SelectionExecutorContext {
    /// `nil` (no statement tracker: contexts that did not run `ResetContextOfStmt`) is `None`.
    pub(crate) stmt_mem_tracker: Option<Arc<Tracker>>,
    pub(crate) eval_ctx: EvalContextRef,
    pub(crate) enable_vectorized_expression: bool,
}

// go: newSelectionExecutorContext L1 [M-CTX]
pub fn new_selection_executor_context(sctx: &dyn SessionContext) -> SelectionExecutorContext {
    SelectionExecutorContext {
        stmt_mem_tracker: sctx.get_session_vars().stmt_ctx().mem_tracker.clone(),
        eval_ctx: sctx.get_expr_ctx().get_eval_ctx(),
        enable_vectorized_expression: sctx.get_session_vars().enable_vectorized_expression,
    }
}

/// SelectionExec represents a filter executor.
///
/// Go keeps an iterator over `childResult` and the current row (`inputIter`, `inputRow`)
/// across `Next` calls; here the current row is its index in `childResult` (`None` is the
/// iterator's `End()`), the iterator is not kept (M-ROW: no row borrowed across calls).
/// The evaluation state of the filters (no correlated values: Apply is not ported, and the
/// vector evaluation's scratch columns) belongs to the executor (N4 §3.4).
pub struct SelectionExec {
    pub(crate) ctx: SelectionExecutorContext,
    pub(crate) base: BaseExecutorV2,

    pub(crate) batched: bool,
    pub(crate) filters: Vec<ExprRef>,
    pub(crate) selected: Vec<bool>,
    pub(crate) input_row: Option<usize>,
    pub(crate) child_result: Option<Chunk>,

    pub(crate) mem_tracker: Option<Arc<Tracker>>,

    pub(crate) eval_state: EvalState,
    pub(crate) rpn: RpnState,
}

#[async_trait]
impl Executor for SelectionExec {
    // glue: M-SELF
    fn base(&self) -> &BaseExecutorV2 {
        &self.base
    }
    // glue: M-SELF
    fn base_mut(&mut self) -> &mut BaseExecutorV2 {
        &mut self.base
    }

    /// Open implements the Executor Open interface.
    // go: SelectionExec.Open L2 (no failpoint)
    async fn open(&mut self, ctx: &ExecCtx) -> Result<(), Error> {
        self.base.open(ctx).await?;
        self.open_inner(ctx)
    }

    /// Close implements plannercore.Plan Close interface.
    // go: SelectionExec.Close L1
    async fn close(&mut self) -> Result<(), Error> {
        if let Some(child_result) = self.child_result.take()
            && let Some(t) = &self.mem_tracker
        {
            // Releasing memory never runs an action.
            let _ = t.consume(-child_result.memory_usage());
        }
        self.selected = Vec::new();
        self.base.close().await
    }

    /// Next implements the Executor Next interface.
    // go: SelectionExec.Next L1 [M-ROW]
    async fn next(&mut self, ctx: &ExecCtx, req: &mut Chunk) -> Result<(), Error> {
        req.grow_and_reset(self.max_chunk_size().max(0) as usize);

        if !self.batched {
            return self.un_batched_next(ctx, req).await;
        }

        let child_result = self
            .child_result
            .as_mut()
            .expect("SelectionExec.Next before Open or after Close (Go: nil chunk)");
        loop {
            while let Some(i) = self.input_row {
                if req.is_full() {
                    return Ok(());
                }

                if self.selected[i] {
                    req.append_row(child_result.get_row(i));
                }
                self.input_row = iter_next(i, child_result);
            }
            let m_size = child_result.memory_usage();
            let r = exec::next(ctx, self.base.meta.children(0).as_mut(), child_result).await;
            consume(&self.mem_tracker, child_result.memory_usage() - m_size)?;
            r?;
            // no more data.
            if child_result.num_rows() == 0 {
                return Ok(());
            }
            self.selected = vectorized_filter(
                &*self.ctx.eval_ctx,
                &self.eval_state,
                &mut self.rpn,
                self.ctx.enable_vectorized_expression,
                &self.filters,
                child_result,
                std::mem::take(&mut self.selected),
            )?;
            self.input_row = iter_begin(child_result);
        }
    }
}

impl SelectionExec {
    // go: SelectionExec.open L1
    fn open_inner(&mut self, _ctx: &ExecCtx) -> Result<(), Error> {
        let mem_tracker = match &self.mem_tracker {
            Some(t) => {
                t.reset()?;
                Arc::clone(t)
            }
            None => {
                let t = new_tracker(self.base.meta.id(), -1);
                self.mem_tracker = Some(Arc::clone(&t));
                t
            }
        };
        if let Some(parent) = &self.ctx.stmt_mem_tracker {
            mem_tracker.attach_to(parent)?;
        }
        let child_result = exec::try_new_cache_chunk(self.base.meta.children(0).as_ref());
        mem_tracker.consume(child_result.memory_usage())?;
        self.batched = vectorizable(&self.filters);
        if self.batched {
            self.selected = Vec::with_capacity(chunk::InitialCapacity);
        }
        self.input_row = None;
        self.child_result = Some(child_result);
        Ok(())
    }

    /// unBatchedNext filters input rows one by one and returns once an input row is selected.
    /// For sql with "SETVAR" in filter and "GETVAR" in projection, for example: "SELECT @a FROM t WHERE (@a := 2) > 0",
    /// we have to set batch size to 1 to do the evaluation of filter and projection.
    // go: SelectionExec.unBatchedNext L1 [M-ROW]
    async fn un_batched_next(&mut self, ctx: &ExecCtx, chk: &mut Chunk) -> Result<(), Error> {
        let eval_ctx = &*self.ctx.eval_ctx;
        let child_result = self
            .child_result
            .as_mut()
            .expect("SelectionExec.Next before Open or after Close (Go: nil chunk)");
        loop {
            while let Some(i) = self.input_row {
                let (selected, _) = eval_bool(
                    eval_ctx,
                    &self.eval_state,
                    &self.filters,
                    child_result.get_row(i),
                )?;
                if selected {
                    chk.append_row(child_result.get_row(i));
                    self.input_row = iter_next(i, child_result);
                    return Ok(());
                }
                self.input_row = iter_next(i, child_result);
            }
            let m_size = child_result.memory_usage();
            let r = exec::next(ctx, self.base.meta.children(0).as_mut(), child_result).await;
            consume(&self.mem_tracker, child_result.memory_usage() - m_size)?;
            r?;
            self.input_row = iter_begin(child_result);
            // no more data.
            if child_result.num_rows() == 0 {
                return Ok(());
            }
        }
    }
}

/// `inputIter.Begin()` of an `Iterator4Chunk` over `chk`, as a row index (`None`: `End()`).
// glue: Iterator4Chunk.Begin as a row index (M-ROW)
fn iter_begin(chk: &Chunk) -> Option<usize> {
    (chk.num_rows() > 0).then_some(0)
}

/// `inputIter.Next()` after row `i`, as a row index (`None`: `End()`).
// glue: Iterator4Chunk.Next as a row index (M-ROW)
fn iter_next(i: usize, chk: &Chunk) -> Option<usize> {
    (i + 1 < chk.num_rows()).then_some(i + 1)
}

/// `e.memTracker.Consume(bs)`: an action of the tracker (Go: a panic, recovered into the
/// statement's error) is an error here (M-PANIC).
// glue: memory.Tracker.Consume of an executor's tracker (M-PANIC)
pub(crate) fn consume(t: &Option<Arc<Tracker>>, bs: i64) -> Result<(), Error> {
    match t {
        Some(t) => t.consume(bs),
        None => Ok(()),
    }
}

/// ResetContextOfStmt resets the StmtContext and session variables.
/// Before every execution, we must clear statement context.
///
/// `text` is `s.Text()` (the parser AST does not keep the text). Go recovers a panic here;
/// panics are caught at the task boundary (M-PANIC). The old statement context is not
/// written (`SetVarHintRestore = nil` on it): it may be shared with the statement
/// checkpoint of VS1 plan §4.6 (N2), and the new context starts without restores anyway.
// go: ResetContextOfStmt L2 (see the module doc for the parts of other blocks; the old context is not written)
pub fn reset_context_of_stmt(
    ctx: &mut dyn SessionContext,
    s: &ast::Stmt,
    text: &str,
) -> Result<(), Error> {
    let killer = ctx.sql_killer();
    let vars = ctx.get_session_vars_mut();
    let restore: Vec<(String, String)> = vars
        .stmt_ctx()
        .set_var_hint_restore
        .iter()
        .flatten()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    for (name, val) in restore {
        if vars.set_system_var(&name, &val).is_err() {
            log::warn!(
                "Failed to restore the variable after SET_VAR hint, variable name={name}, expected value={val}"
            );
        }
    }
    let mut sc_arc: Arc<StatementContext> =
        if vars.state.txn_ctx.could_retry || vars.has_status_flag(ServerStatusCursorExists) {
            // Must construct new statement context object, the retry history need context for
            // every statement.
            // TODO: Maybe one day we can get rid of transaction retry, then this logic can be
            // deleted.
            Arc::new(tidb_sessionctx::stmtctx::new_stmt_ctx())
        } else {
            vars.init_statement_context()
        };
    let sc = Arc::get_mut(&mut sc_arc).expect("a new statement context");
    sc.set_time_zone(vars.location());
    sc.task_id = allocate_task_id();
    // CTEStorageMap / TblInfo2UnionScan: not ported (their executors), nothing to clear.
    sc.lock_table_ids.clear();
    sc.table_stats.clear();
    sc.related_table_ids.clear();
    sc.is_staleness = false;
    sc.enable_optimize_trace = false;
    // sc.OptimizeTracer = nil; sc.OptimizerCETrace = nil: the optimizer traces (D09.13).
    sc.is_sync_stats_failed = false;
    sc.is_explain_analyze_dml = false;
    sc.resource_group_name = vars.resource_group_name.clone();
    // Firstly we assume that UseDynamicPruneMode can be enabled according session variable,
    // then we will check other conditions in PlanBuilder.buildDataSource
    sc.use_dynamic_prune_mode = vars.is_dynamic_partition_prune_enabled();

    sc.stats_load_timeout = std::time::Duration::ZERO;
    // sc.StatsLoad.NeededItems / ResultCh: the sync stats load (D11.2).

    sc.sysdate_is_now = vars.sysdate_is_now;

    // vars.MemTracker / vars.DiskTracker: the session trackers are A1's (not in SessionVars).
    killer.reset();
    killer.conn_id.store(vars.connection_id, Ordering::SeqCst);

    // isAnalyze: ANALYZE is delegated to Go TiDB (not in the Rust AST), also as the
    // statement of an EXECUTE: the SQL text tracker.
    let prepared = match s {
        ast::Stmt::Execute(exec_stmt) => Some(get_prepared_stmt(exec_stmt, vars)?),
        _ => None,
    };
    sc.init_mem_tracker(LabelForSQLText, -1);
    // The OOM action (variable.OOMAction) and sc.MemTracker.AttachTo(vars.MemTracker): A1.
    sc.init_disk_tracker(LabelForSQLText, -1);
    // EnableTmpStorageOnOOM: the disk tracker is attached to the session's (A1).

    // EXECUTE: Go's second GetPreparedStmt finds the same statement. `isWeakConsistencyRead`
    // of the EXECUTE node reads the statement again (planner IsReadOnly), so it is evaluated
    // before the statement is held for the rest of the function (the lock is not reentrant).
    if let Some(prepare_stmt) = &prepared {
        let select_without_lock = matches!(
            &*prepare_stmt.lock().prepared_ast.stmt,
            ast::Stmt::Select(sel) if sel.lock_info.is_none()
        );
        if select_without_lock {
            sc.weak_consistency = is_weak_consistency_read(vars, s)?;
        }
    }
    let prepared = prepared.as_ref().map(|p| p.lock());
    let (s, text) = match &prepared {
        Some(prepare_stmt) => {
            // s = prepareStmt.PreparedAst.Stmt. sc.InitSQLDigest(prepareStmt.NormalizedSQL,
            // prepareStmt.SQLDigest): both are empty (parser.NormalizeDigest is a boundary
            // stub, plan_cache_utils), nothing to set; pprof labels and Top SQL: D14.2.
            // `s.Text()` of the prepared statement is its SQL (`StmtText`).
            (
                &*prepare_stmt.prepared_ast.stmt,
                prepare_stmt.stmt_text.as_str(),
            )
        }
        None => (s, text),
    };
    // execute missed stmtID uses empty sql
    sc.original_sql = text.to_owned();
    // EXPLAIN / EXPLAIN FOR are not in the Rust AST (D02.2).
    sc.explain_format = String::new();

    // TODO: Many same bool variables here.
    // We should set only two variables (
    // IgnoreErr and StrictSQLMode) to avoid setting the same bool variables and
    // pushing them down to TiKV as flags.

    sc.in_restricted_sql = vars.in_restricted_sql;
    let sql_mode = SQLMode(vars.sql_mode);

    let mut err_levels = sc.err_levels();
    err_levels[errctx::ErrGroupDividedByZero.0 as usize] = errctx::LevelWarn;
    match s {
        ast::Stmt::Select(stmt) => {
            sc.in_select_stmt = true;

            // Return warning for truncate error in selection.
            sc.set_type_flags(
                sc.type_flags()
                    .with_truncate_as_warning(true)
                    .with_ignore_zero_in_date(true)
                    .with_ignore_invalid_date_err(sql_mode.has_allow_invalid_dates_mode()),
            );
            let opts = &stmt.select_stmt_opts;
            sc.priority = PriorityEnum(opts.priority as isize);
            sc.not_fill_cache = !opts.sql_cache;
            sc.weak_consistency = is_weak_consistency_read(vars, s)?;
        }
        ast::Stmt::SetOpr(_) => {
            sc.in_select_stmt = true;
            sc.set_type_flags(
                sc.type_flags()
                    .with_truncate_as_warning(true)
                    .with_ignore_zero_in_date(true)
                    .with_ignore_invalid_date_err(sql_mode.has_allow_invalid_dates_mode()),
            );
        }
        ast::Stmt::Show(stmt) => {
            sc.set_type_flags(
                sc.type_flags()
                    .with_ignore_truncate_err(true)
                    .with_ignore_zero_in_date(true)
                    .with_ignore_invalid_date_err(sql_mode.has_allow_invalid_dates_mode()),
            );
            if matches!(
                stmt.tp,
                ast::ShowStmtType::Warnings
                    | ast::ShowStmtType::Errors
                    | ast::ShowStmtType::SessionStates
            ) {
                sc.in_show_warning = true;
                sc.set_warnings(vars.stmt_ctx().get_warnings());
            }
        }
        // UPDATE, DELETE, INSERT, CREATE / ALTER TABLE, LOAD DATA, IMPORT INTO, SPLIT
        // REGION, SET SESSION_STATES and the other statements Go distinguishes here are not
        // in the Rust AST. ADMIN statements (also outside it) take Go's `default` arm.
        // glue: stub, blocked-by:D02.2 (the statements outside the Rust AST)
        ast::Stmt::Unported(n) if !n.kind.starts_with("AdminStmt#") => {
            return Err(blocked(
                "D02.2",
                "ResetContextOfStmt of a statement outside the Rust AST",
            ));
        }
        _ => {
            sc.set_type_flags(
                sc.type_flags()
                    .with_ignore_truncate_err(true)
                    .with_ignore_zero_in_date(true)
                    .with_ignore_invalid_date_err(sql_mode.has_allow_invalid_dates_mode()),
            );
        }
    }

    if err_levels != sc.err_levels() {
        sc.set_err_levels(err_levels);
    }

    let global_config = tidb_base::config::get_global_config();
    // WithAllowNegativeToUnsigned with false value indicates values less than 0 should be
    // clipped to 0 for unsigned integer types. This is the case for `insert`, `update`,
    // `alter table`, `create table` and `load data infile` statements, when not in strict
    // SQL mode. see https://dev.mysql.com/doc/refman/5.7/en/out-of-range-and-overflow.html
    // (inImportInto: IMPORT INTO is not in the Rust AST, false.)
    let allow_negative = !sc.in_insert_stmt
        && !sc.in_load_data_stmt
        && !sc.in_update_stmt
        && !sc.in_create_or_alter_stmt;
    sc.set_type_flags(
        sc.type_flags()
            .with_skip_utf8_check(vars.skip_utf8_check)
            .with_skip_sacii_check(vars.skip_ascii_check)
            .with_skip_utf8mb4_check(!global_config.instance.check_mb4_value_in_utf8.load())
            .with_allow_negative_to_unsigned(allow_negative),
    );

    vars.state.plan_cache_params.lock().reset();
    let priority = PriorityEnum(
        tidb_sessionctx::variable::tidb_vars::ForcePriority.load(Ordering::SeqCst) as isize,
    );
    if priority != NoPriority {
        sc.priority = priority;
    }
    let old = vars.stmt_ctx();
    if old.last_insert_id() > 0 {
        sc.prev_last_insert_id = old.last_insert_id();
    } else {
        sc.prev_last_insert_id = old.prev_last_insert_id;
    }
    sc.prev_affected_rows = 0;
    if old.in_update_stmt
        || old.in_delete_stmt
        || old.in_insert_stmt
        || old.in_set_session_states_stmt
    {
        sc.prev_affected_rows = old.affected_rows() as i64;
    } else if old.in_select_stmt {
        sc.prev_affected_rows = -1;
    }
    if global_config.instance.enable_collect_execution_info.load() {
        // In ExplainFor case, RuntimeStatsColl should not be reset for reuse,
        // because ExplainFor need to display the last statement information.
        // (The old collector is not reused: it may be shared with the checkpoint, M-POOL.)
        sc.runtime_stats_coll = Some(RuntimeStatsColl::new(None));
        // also enable index usage collector: D11.2.
    } else {
        // turn off the index usage collector: D11.2.
    }

    // sc.SetForcePlanCache(fixcontrol Fix49736) / sc.SetAlwaysWarnSkipCache: D13.1.
    let (err_count, warn_count) = old.num_error_warnings();
    vars.sys_error_count = err_count;
    vars.sys_warning_count = warn_count;
    vars.exchange_chunk_status();
    vars.set_stmt_ctx(sc_arc);
    vars.prev_found_in_plan_cache = vars.found_in_plan_cache;
    vars.found_in_plan_cache = false;
    vars.prev_found_in_binding = vars.found_in_binding;
    vars.found_in_binding = false;
    vars.duration_wait_ts = 0;
    // vars.CurrInsertBatchExtraCols / CurrInsertValues: the write path.
    ctx.reset_plan_ctx();

    Ok(())
}

/// setOptionForTopSQL: the Top SQL options of a snapshot (or transaction).
///
/// The resource group tagger (`sc.GetResourceGroupTagger()`) and the KV exec counter's RPC
/// interceptor (`sc.KvExecCounter`) are Top SQL's (D14.2), which tidb-rs does not record
/// (ROADMAP §3; D12.1 spec §3, the `Open` row): the statement context has neither, and the
/// store ignores both options, so nothing is set. A pipelined transaction (Go returns early)
/// does not exist: tidb-rs transactions never write.
// go: setOptionForTopSQL L2 (no Top SQL options: D14.2 is not recorded; no pipelined transaction)
pub fn set_option_for_top_sql(sc: &StatementContext, snapshot: Option<&dyn Snapshot>) {
    let Some(_snapshot) = snapshot else {
        return;
    };
    // snapshot.SetOption(kv.ResourceGroupTagger, sc.GetResourceGroupTagger()) and, with
    // sc.KvExecCounter, snapshot.SetOption(kv.RPCInterceptor, ...): Top SQL (D14.2).
    let _ = sc;
}

/// `plannercore.IsReadOnly(node, vars)` is a boundary stub (`blocked-by:D02.2`), reached only
/// with `tidb_read_consistency = weak` (Go's `&&` order is kept); its error is returned
/// (M-ERRVAL).
// go: isWeakConsistencyRead L1 [M-ERRVAL]
fn is_weak_consistency_read(session_vars: &SessionVars, node: &ast::Stmt) -> Result<bool, Error> {
    Ok(session_vars.connection_id > 0
        && session_vars.read_consistency == "weak"
        && tidb_planner::core::common_plans::is_auto_commit_txn(session_vars)
        && tidb_planner::optimize::is_read_only(node, session_vars)?)
}
