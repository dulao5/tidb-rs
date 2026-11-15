//! go-file: pkg/executor/adapter.go
//!
//! `ExecStmt` (a compiled statement: plan + info schema) and `recordSet` (its result), the
//! part of block D12.0 in VS1-13.
//!
//! Carriers (N2, M-GOCTX):
//! - Go's `ExecStmt.Ctx` (`sessionctx.Context`) and `GoCtx` are not fields: the methods take
//!   the session ([`SessionContext`]), which owns the statement and its record set on its
//!   task. Go's `recordSet.stmt` pointer is the same: the record set's methods take the
//!   statement.
//! - The executors run with an [`ExecCtx`] made from the statement context when the
//!   executor is opened (N3).
//!
//! Not on the read-only node's path, so not ported here (no function of VS1-13): the
//! pessimistic DML / SELECT FOR UPDATE retry loop, foreign key cascades, the slow log and the
//! statement summary (D14.2), Top SQL (D14.2), plan replayer capture, audit plugins, the
//! runaway checker (N10).

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime};

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_base::mysql::{HighPriority, LowPriority, MaxAliasIdentifierLen, NoPriority};
use tidb_chunk::alloc::Allocator;
use tidb_chunk::chunk::Chunk;
use tidb_expression::Schema;
use tidb_parser::ast;
use tidb_parser::model::CIStr;
use tidb_planner::PhysicalKind;
use tidb_sessionctx::sessionstates::QueryInfo;
use tidb_sessionctx::stmtctx::StatementContext;
use tidb_types::field_name::FieldName;

use crate::builder::new_executor_builder;
use crate::detach::detach;
use crate::exec::ExecCtx;
use crate::glue::{InfoSchemaRef, Plan, PlanCacheStmtRef, ResultField, SessionContext};
use crate::internal::exec::executor::{self as exec, Executor};
use crate::metrics::{self as executor_metrics, METRICS, Observer};

/// recordSet wraps an executor, implements sqlexec.RecordSet interface
///
/// **No `Drop`.** `finish` / `close` are async and need the statement and the session, so
/// dropping a record set without `close` (the statement's future cancelled, KILL, the client
/// disconnecting) closes nothing on the session side: `ExecStmt::close_record_set` /
/// `FinishExecuteStmt` (last query info, phase metrics, the statement context resets) does
/// not run and `last_errs` is lost; the executor tree is just dropped. The session (VS1-14)
/// must notice a statement that did not finish and do the missing finish before it runs the
/// next statement (Go always runs `Close` through `defer` in the protocol layer).
pub struct RecordSet {
    fields: Vec<ResultField>,
    executor: Option<Box<dyn Executor>>,
    /// The `Fields` method may be called after `Close`, and the executor is cleared in the
    /// `Close` function. Therefore, we need to store the schema in `recordSet` to avoid a null
    /// pointer exception when calling `executor.Schema()`.
    schema: Schema,
    last_errs: Vec<Error>,
    txn_start_ts: u64,
    /// `once` of `Finish`.
    finished: bool,
    /// The context the executor runs with (N3); dropped by `Finish` with the executor.
    exec_ctx: Option<ExecCtx>,
    /// The prepared statement whose cache gets the point get executor back when the record
    /// set finishes (Go caches the executor pointer while it runs, `ExecStmt.PointGet`).
    point_get_cache: Option<PlanCacheStmtRef>,
}

impl RecordSet {
    // go: recordSet.Fields L2 (the statement and the session are passed in)
    pub fn fields(&mut self, stmt: &ExecStmt, sctx: &dyn SessionContext) -> &[ResultField] {
        if self.fields.is_empty() {
            self.fields = col_names2_result_fields(
                &self.schema,
                &stmt.output_names,
                &sctx.get_session_vars().current_db,
            );
        }
        &self.fields
    }

    /// Next use uses recordSet's executor to get next available chunk for later usage.
    /// If chunk does not contain any rows, then we update last query found rows in session
    /// variable as current found rows. The reason we need update is that chunk with 0 rows
    /// indicating we already finished current query, we need prepare for next query.
    /// If stmt is not nil and chunk with some rows inside, we simply update last query found
    /// rows by the number of row in chunk.
    // go: recordSet.Next L2 (panics are caught at the task boundary, M-PANIC)
    pub async fn next(
        &mut self,
        stmt: &mut ExecStmt,
        sctx: &mut dyn SessionContext,
        req: &mut Chunk,
    ) -> Result<(), Error> {
        sctx.sql_killer().handle_signal()?;

        let (Some(e), Some(ctx)) = (self.executor.as_deref_mut(), self.exec_ctx.as_ref()) else {
            return Err(Error::plain("the record set is closed"));
        };
        if let Err(err) = stmt.next(ctx, e, req).await {
            self.last_errs.push(err.clone());
            return Err(err);
        }
        let num_rows = req.num_rows();
        let vars = sctx.get_session_vars_mut();
        if num_rows == 0 {
            vars.last_found_rows = vars.stmt_ctx().found_rows();
            return Ok(());
        }
        vars.stmt_ctx().add_found_rows(num_rows as u64);
        Ok(())
    }

    /// NewChunk create a chunk base on top-level executor's exec.NewFirstChunk().
    // go: recordSet.NewChunk L1
    pub fn new_chunk(&self, alloc: Option<&mut dyn Allocator>) -> Chunk {
        let e = self.executor.as_deref().expect("record set is open");
        let Some(alloc) = alloc else {
            return exec::new_first_chunk(e);
        };

        alloc.alloc(
            e.ret_field_types(),
            e.init_cap().max(0) as usize,
            e.max_chunk_size().max(0) as usize,
        )
    }

    // go: recordSet.Finish L1
    pub async fn finish(&mut self, sctx: &mut dyn SessionContext) -> Result<(), Error> {
        let mut result = Ok(());
        if !self.finished {
            self.finished = true;
            if let Some(e) = self.executor.as_deref_mut() {
                result = exec::close(e).await;
            }
            // resetCTEStorageMap: the CTE storages are created by the CTE executors only,
            // which are not ported: there is nothing to reset.
            let executor = self.executor.take();
            if let (Some(cache), Some(e)) = (self.point_get_cache.take(), executor) {
                cache.lock().point_get_executor = Some(e);
            }
            self.exec_ctx = None;
            let killer = sctx.sql_killer();
            let status = killer.get_kill_signal();
            let in_write_result_set = killer.in_write_result_set.load(Ordering::SeqCst);
            if status > 0 && in_write_result_set {
                log::warn!(
                    "kill, this SQL might be stuck in the network stack while writing packets \
                     to the client. connection ID={}",
                    sctx.get_session_vars().connection_id
                );
            }
        }
        if let Err(err) = &result {
            self.last_errs.push(err.clone());
        }
        result
    }

    // go: recordSet.Close L2 (the statement and the session are passed in)
    pub async fn close(
        &mut self,
        stmt: &mut ExecStmt,
        sctx: &mut dyn SessionContext,
    ) -> Result<(), Error> {
        let err = self.finish(sctx).await;
        if let Err(e) = &err {
            log::error!("close recordSet error: {e}");
        }
        stmt.close_record_set(sctx, self.txn_start_ts, join_errors(&self.last_errs));
        err
    }

    /// OnFetchReturned implements commandLifeCycle#OnFetchReturned
    ///
    /// `LogSlowQuery` is the slow log (D14.2): nothing is logged.
    // go: recordSet.OnFetchReturned L1
    pub fn on_fetch_returned(&self) {
        // a.stmt.LogSlowQuery(a.txnStartTS, len(a.lastErrs) == 0, true)
        // glue: stub, blocked-by:D14.2 (slow log)
    }

    /// TryDetach creates a new `RecordSet` which doesn't depend on the current session
    /// context. `Ok(None)` is Go's `(nil, false, nil)`.
    // go: recordSet.TryDetach L1
    pub fn try_detach(&self) -> Result<Option<RecordSet>, Error> {
        let Some(e) = self.executor.as_deref() else {
            return Ok(None);
        };
        let Some(_e) = detach(e) else {
            return Ok(None);
        };
        // staticrecordset.New(a.Fields(), e, a.stmt.GetTextToLog(false)): the record set of a
        // cursor (D12.12).
        Err(blocked("D12.12", "detached record sets (cursor fetch)"))
    }

    /// GetExecutor4Test exports the internal executor for test purpose.
    // go: recordSet.GetExecutor4Test L1
    pub fn get_executor4_test(&self) -> Option<&dyn Executor> {
        self.executor.as_deref()
    }

    /// The start ts of the statement's transaction (Go's `txnStartTS` field).
    // glue: accessor
    pub fn txn_start_ts(&self) -> u64 {
        self.txn_start_ts
    }
}

/// `errors.Join(errs...)`: `None` for none, the messages joined by newlines otherwise.
// go-std: errors.Join L1
fn join_errors(errs: &[Error]) -> Option<Error> {
    match errs {
        [] => None,
        [e] => Some(e.clone()),
        _ => Some(Error::plain(
            errs.iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        )),
    }
}

// go: colNames2ResultFields L1
#[allow(clippy::needless_range_loop)] // Go's index loop over two slices
fn col_names2_result_fields(
    schema: &Schema,
    names: &[FieldName],
    default_db: &str,
) -> Vec<ResultField> {
    let mut rfs = Vec::with_capacity(schema.len());
    let default_db_ci_str = CIStr::new(default_db);
    for i in 0..schema.len() {
        let mut db_name = names[i].db_name.clone();
        if db_name.l.is_empty() && !names[i].tbl_name.l.is_empty() {
            db_name = default_db_ci_str.clone();
        }
        let mut orig_col_name = names[i].orig_col_name.clone();
        let mut empty_org_name = false;
        if orig_col_name.l.is_empty() {
            orig_col_name = names[i].col_name.clone();
            empty_org_name = true;
        }
        let mut rf = ResultField {
            column_name: orig_col_name,
            column_type: tidb_parser::types::FieldType::clone(&schema.columns[i].ret_type),
            column_as_name: names[i].col_name.clone(),
            empty_org_name,
            table_name: names[i].orig_tbl_name.clone(),
            table_as_name: names[i].tbl_name.clone(),
            db_name,
        };
        // This is for compatibility.
        // See issue https://github.com/pingcap/tidb/issues/10513 .
        if rf.column_as_name.o.len() > MaxAliasIdentifierLen as usize {
            rf.column_as_name.o =
                truncate_bytes(&rf.column_as_name.o, MaxAliasIdentifierLen as usize);
        }
        // Usually the length of O equals the length of L.
        // Add this len judgement to avoid panic.
        if rf.column_as_name.l.len() > MaxAliasIdentifierLen as usize {
            rf.column_as_name.l =
                truncate_bytes(&rf.column_as_name.l, MaxAliasIdentifierLen as usize);
        }
        rfs.push(rf);
    }
    rfs
}

/// Go's `s[:n]` on a string: the first `n` bytes (`String` holds UTF-8, so a cut inside a
/// character is replaced as Go's bytes would print, M-BYTES).
// glue: Go string slicing on a Rust String
fn truncate_bytes(s: &str, n: usize) -> String {
    String::from_utf8_lossy(&s.as_bytes()[..n]).into_owned()
}

/// TelemetryInfo records some telemetry information during execution.
///
/// The partition and account-lock telemetry are the DDL / user statements' (not ported).
#[derive(Debug, Default)]
pub struct TelemetryInfo {
    pub use_non_recursive: bool,
    pub use_recursive: bool,
    pub use_multi_schema_change: bool,
    pub use_exchange_partition: bool,
    pub use_flashback_to_cluster: bool,
    pub use_index_merge: bool,
    pub use_table_look_up: std::sync::atomic::AtomicBool,
}

/// ExecStmt implements the sqlexec.Statement interface, it builds a planner.Plan to an
/// sqlexec.Statement.
pub struct ExecStmt {
    /// InfoSchema stores a reference to the schema information.
    pub info_schema: Option<InfoSchemaRef>,
    /// Plan stores a reference to the final physical plan.
    pub plan: Option<Plan>,
    /// Text represents the origin query text.
    pub text: String,

    pub stmt_node: Option<Arc<ast::Stmt>>,

    /// LowerPriority represents whether to lower the execution priority of a query.
    pub lower_priority: bool,
    pub is_prepared_stmt: bool,
    pub is_select_for_update: bool,
    pub retry_count: u32,
    pub retry_start_time: Option<Instant>,

    /// Phase durations are splited into two parts: 1. trying to lock keys (but failed); 2.
    /// the final iteration of the retry loop. Here we use [2]time.Duration to record such
    /// info for each phase. The first duration is increased only within the current
    /// iteration. When we meet a pessimistic lock error and decide to retry, we add the first
    /// duration to the second and reset the first to 0 by calling `resetPhaseDurations`.
    pub phase_build_durations: [Duration; 2],
    pub phase_open_durations: [Duration; 2],
    pub phase_next_durations: [Duration; 2],
    pub phase_lock_durations: [Duration; 2],

    /// OutputNames will be set if using cached plan
    pub output_names: Vec<FieldName>,
    pub ps_stmt: Option<PlanCacheStmtRef>,
    pub ti: Arc<TelemetryInfo>,
}

impl ExecStmt {
    /// A statement for a plan (the fields Go's `Compiler.Compile` sets).
    // glue: Go struct literal &ExecStmt{...}
    pub fn new(text: String, plan: Option<Plan>, info_schema: Option<InfoSchemaRef>) -> ExecStmt {
        ExecStmt {
            info_schema,
            plan,
            text,
            stmt_node: None,
            lower_priority: false,
            is_prepared_stmt: false,
            is_select_for_update: false,
            retry_count: 0,
            retry_start_time: None,
            phase_build_durations: [Duration::ZERO; 2],
            phase_open_durations: [Duration::ZERO; 2],
            phase_next_durations: [Duration::ZERO; 2],
            phase_lock_durations: [Duration::ZERO; 2],
            output_names: Vec::new(),
            ps_stmt: None,
            ti: Arc::new(TelemetryInfo::default()),
        }
    }

    /// GetStmtNode returns the stmtNode inside Statement
    // go: ExecStmt.GetStmtNode L1
    pub fn get_stmt_node(&self) -> Option<&Arc<ast::Stmt>> {
        self.stmt_node.as_ref()
    }

    /// The statement context the executors share (N2), for `ExecCtx`.
    // glue: N3 ExecCtx of the statement
    fn exec_ctx(sctx: &dyn SessionContext) -> ExecCtx {
        ExecCtx::new(sctx.get_session_vars().stmt_ctx_arc())
    }

    /// The statement context for writing its plain fields: only while no executor shares it
    /// (Go writes through the pointer; N2).
    // glue: N2 field writes through s.StmtCtx
    fn stmt_ctx_mut(sctx: &mut dyn SessionContext) -> &mut StatementContext {
        sctx.get_session_vars_mut().stmt_ctx_mut()
    }

    /// PointGet short path for point exec directly from plan, keep only necessary steps
    // go: ExecStmt.PointGet L2 (no tracing region / Top SQL, D14.2; executor type checks through Executor::as_point_get)
    pub async fn point_get(&mut self, sctx: &mut dyn SessionContext) -> Result<RecordSet, Error> {
        let start_ts = sctx.get_stmt_read_ts().await?;
        Self::stmt_ctx_mut(sctx).priority = HighPriority;

        let mut executor: Option<Box<dyn Executor>> = None;
        let use_max_ts = start_ts == u64::MAX;
        let mut point_get_cache = None;

        // try to reuse point get executor
        // We should only use the cached the executor when the startTS is MaxUint64
        let ps_stmt = self
            .ps_stmt
            .clone()
            .expect("point plan short path has a prepared statement");
        let cached = if use_max_ts {
            ps_stmt.lock().point_get_executor.take()
        } else {
            None
        };
        if let Some(mut cached) = cached {
            match cached.as_point_get() {
                None => {
                    log::error!("invalid executor type, not PointGetExecutor for point get path");
                }
                Some(pg) => {
                    // CachedPlan type is already checked in last step
                    pg.init(self.plan.as_ref().expect("point get plan"))?;
                    executor = Some(cached);
                    point_get_cache = Some(Arc::clone(&ps_stmt));
                }
            }
        }

        let mut executor = match executor {
            Some(e) => e,
            None => {
                let mut b =
                    new_executor_builder(sctx, self.info_schema.clone(), Arc::clone(&self.ti));
                let e = b.build(sctx, self.plan.as_ref());
                if let Some(err) = b.err {
                    return Err(err);
                }
                let mut e = e.expect("built");
                // Don't cache the executor for non point-get (table dual) or partitioned tables
                let cache = match e.as_point_get() {
                    Some(pg) => use_max_ts && !pg.is_partitioned(),
                    None => false,
                };
                if cache {
                    // Go keeps the executor in the cache and uses it for this execution too
                    // (one pointer); here the cache gets it back when the record set
                    // finishes.
                    point_get_cache = Some(Arc::clone(&ps_stmt));
                }
                e
            }
        };

        let ctx = Self::exec_ctx(sctx);
        if let Err(err) = exec::open(&ctx, executor.as_mut()).await {
            if let Err(e) = exec::close(executor.as_mut()).await {
                log::warn!("close executor: {e}");
            }
            return Err(err);
        }

        let cmd = sctx.get_session_vars().command_value as u8;
        let sql = self.origin_text().to_owned();
        let max_execution_time = sctx.get_session_vars().get_max_execution_time();
        // Update processinfo, ShowProcess() will use it.
        sctx.set_process_info(&sql, SystemTime::now(), cmd, max_execution_time);
        // StmtCtx.StmtType = stmtctx.GetStmtLabel(ctx, a.StmtNode): GetStmtLabel is D00.1,
        // not in tidb-sessionctx yet; the statement type stays as the session set it.

        let schema = executor.schema();
        Ok(RecordSet {
            fields: Vec::new(),
            executor: Some(executor),
            schema,
            last_errs: Vec::new(),
            txn_start_ts: start_ts,
            finished: false,
            exec_ctx: Some(ctx),
            point_get_cache,
        })
    }

    /// OriginText returns original statement as a string.
    // go: ExecStmt.OriginText L1
    pub fn origin_text(&self) -> &str {
        &self.text
    }

    /// IsPrepared returns true if stmt is a prepare statement.
    // go: ExecStmt.IsPrepared L1
    pub fn is_prepared(&self) -> bool {
        self.is_prepared_stmt
    }

    /// IsReadOnly returns true if a statement is read only.
    /// If current StmtNode is an ExecuteStmt, we can get its prepared stmt,
    /// then using ast.IsReadOnly function to determine a statement is read only or not.
    // go: ExecStmt.IsReadOnly L2 (boundary stub, blocked-by:D09.13)
    pub fn is_read_only(&self) -> Result<bool, Error> {
        // planner.IsReadOnly(a.StmtNode, vars)
        Err(blocked("D09.13", "planner.IsReadOnly"))
    }

    /// RebuildPlan rebuilds current execute statement plan.
    /// It returns the current information schema version that 'a' is using.
    ///
    /// Needs the preprocessor and `planner.Optimize` (VS1-10).
    // go: ExecStmt.RebuildPlan L2 (boundary stub, blocked-by:D09.0)
    pub fn rebuild_plan(&mut self) -> Result<i64, Error> {
        // glue: stub, blocked-by:D09.0 (preprocess and optimize, VS1-10)
        Err(blocked(
            "D09.0",
            "rebuilding the plan of a retried statement",
        ))
    }

    /// Exec builds an Executor from a plan. If the Executor doesn't return result, like the
    /// INSERT, UPDATE statements, it executes in this function. If the Executor returns
    /// result, execution is done after this function returns, in the returned
    /// sqlexec.RecordSet Next method.
    ///
    /// `Ok(None)` is Go's nil record set (a statement without a result).
    // go: ExecStmt.Exec L2 (no Analyze / runaway / Top SQL branches: delegated, N10, D14.2; panics at the task boundary)
    pub async fn exec(
        &mut self,
        sctx: &mut dyn SessionContext,
    ) -> Result<Option<RecordSet>, Error> {
        // The Analyze plan is delegated to Go TiDB (constitution §4.3); no plan of the new
        // stack is one.
        // HasMemQuotaHint / MemQuotaQuery: the MEMORY_QUOTA hint is D13.3 (A1 sets the limit).

        // must set plan according to the `Execute` plan before getting planDigest
        self.inherit_context_from_execute_stmt();
        // The runaway checker is resource control (N10); Top SQL is D14.2.

        let mut e = self.build_executor(sctx)?;

        let cmd = sctx.get_session_vars().command_value as u8;
        let sql = self.get_sql_for_process_info();
        let mut max_execution_time = sctx.get_session_vars().get_max_execution_time();
        // StmtType: stmtctx.GetStmtLabel (D00.1, not in tidb-sessionctx yet).
        // Since maxExecutionTime is used only for SELECT statements, here we limit its scope.
        if !sctx.get_session_vars().stmt_ctx().in_select_stmt {
            max_execution_time = 0;
        }
        sctx.set_process_info(&sql, SystemTime::now(), cmd, max_execution_time);

        let ctx = Self::exec_ctx(sctx);
        if let Err(err) = self.open_executor(&ctx, e.as_mut()).await {
            if let Err(e2) = exec::close(e.as_mut()).await {
                log::warn!("close executor: {e2}");
            }
            return Err(err);
        }

        let is_pessimistic = sctx.get_session_vars().state.txn_ctx.is_pessimistic;

        if self.is_select_for_update {
            if sctx.get_session_vars().use_low_resolution_tso() {
                if let Err(e2) = exec::close(e.as_mut()).await {
                    log::warn!("close executor: {e2}");
                }
                return Err(Error::plain(
                    "can not execute select for update statement when 'tidb_low_resolution_tso' is set",
                ));
            }
            // Special handle for "select for update statement" in pessimistic transaction.
            if is_pessimistic {
                // a.handlePessimisticSelectForUpdate(ctx, e): locks keys (the write path).
                if let Err(e2) = exec::close(e.as_mut()).await {
                    log::warn!("close executor: {e2}");
                }
                // glue: stub, blocked-by:X (pessimistic locks: the write path)
                return Err(blocked(
                    "X",
                    "SELECT ... FOR UPDATE in a pessimistic transaction",
                ));
            }
        }

        // a.prepareFKCascadeContext(e): foreign key cascades are the write path.
        let (handled, result) = self
            .handle_no_delay(sctx, &ctx, e.as_mut(), is_pessimistic)
            .await;
        match result {
            Err(err) => return Err(err),
            Ok(()) if handled => return Ok(None),
            Ok(()) => {}
        }

        let txn_start_ts = match sctx.txn_start_ts() {
            Ok(ts) => ts.unwrap_or(0),
            Err(err) => {
                if let Err(e2) = exec::close(e.as_mut()).await {
                    log::warn!("close executor: {e2}");
                }
                return Err(err);
            }
        };

        let schema = e.schema();
        Ok(Some(RecordSet {
            fields: Vec::new(),
            executor: Some(e),
            schema,
            last_errs: Vec::new(),
            txn_start_ts,
            finished: false,
            exec_ctx: Some(ctx),
            point_get_cache: None,
        }))
    }

    /// The `Execute` plan (a prepared statement's execution) is D13.1 (VS1-15): no plan of
    /// the new stack is one yet, so there is nothing to inherit.
    // go: ExecStmt.inheritContextFromExecuteStmt L2 (the Execute plan is D13.1; none is built yet)
    fn inherit_context_from_execute_stmt(&mut self) {}

    /// The text shown in the process list. `SecureText` of sensitive statements (user and
    /// password statements, IMPORT INTO) does not apply: none reaches the new stack (VS1
    /// router: SELECT only).
    // go: ExecStmt.getSQLForProcessInfo L2 (no sensitive statement reaches the new stack)
    fn get_sql_for_process_info(&self) -> String {
        self.origin_text().to_owned()
    }

    /// `(handled, err)`: Go's `(handled, rs, err)` where `rs` is always nil on the read path
    /// (`handleNoDelayExecutor` returns no record set).
    // go: ExecStmt.handleNoDelay L2 (no EXPLAIN ANALYZE / DO / pessimistic DML: D12.12, D12.2, X)
    async fn handle_no_delay(
        &mut self,
        sctx: &mut dyn SessionContext,
        ctx: &ExecCtx,
        e: &mut dyn Executor,
        is_pessimistic: bool,
    ) -> (bool, Result<(), Error>) {
        let to_check = e;
        // ExplainExec with an analyze executor (EXPLAIN ANALYZE of DML) is D12.12.
        let is_explain_analyze = false;

        // If the executor doesn't return any result to the client, we execute it without
        // delay.
        if to_check.schema().is_empty() {
            let handled = !is_explain_analyze;
            if is_pessimistic {
                // a.handlePessimisticDML(ctx, toCheck): the write path.
                return (
                    handled,
                    Err(blocked("X", "DML in a pessimistic transaction")),
                );
            }
            let r = self.handle_no_delay_executor(sctx, ctx, to_check).await;
            if handled {
                // sc.DetachMemDiskTracker() and resetCTEStorageMap (deferred in Go): the
                // trackers are detached with the statement context (A1); no CTE storages.
            }
            return (handled, r);
        }
        // `*ProjectionExec` with calculateNoDelay (DO) is D12.2 (VS1-35).

        (false, Ok(()))
    }

    // go: ExecStmt.handleNoDelayExecutor L2 (no write executors: the write path)
    async fn handle_no_delay_executor(
        &mut self,
        sctx: &mut dyn SessionContext,
        ctx: &ExecCtx,
        e: &mut dyn Executor,
    ) -> Result<(), Error> {
        let _ = &sctx;
        // The "tidb_snapshot" / low-resolution TSO check is for the write executors (Delete,
        // Insert, Update, Replace, LoadData, DDL, ImportInto): none exists on the read path.
        let mut chk = exec::try_new_cache_chunk(e);
        let err = self.next(ctx, e, &mut chk).await;
        // defer: exec.Close(e), a.logAudit() (audit plugins are not ported).
        if let Err(e2) = exec::close(e).await {
            log::warn!("close executor: {e2}");
        }
        err?;
        // a.handleStmtForeignKeyTrigger(ctx, e): foreign keys are the write path.
        Ok(())
    }

    // go: ExecStmt.buildExecutor L2 (no Execute / Analyze plans: D13.1, delegated)
    fn build_executor(
        &mut self,
        sctx: &mut dyn SessionContext,
    ) -> Result<Box<dyn Executor>, Error> {
        let start = Instant::now();
        let r = (|| {
            // `*plannercore.Execute` (D13.1) is not a plan of the new stack.
            {
                let stmt_ctx = Self::stmt_ctx_mut(sctx);
                if stmt_ctx.priority == NoPriority && self.lower_priority {
                    stmt_ctx.priority = LowPriority;
                }
            }
            let mut b = new_executor_builder(sctx, self.info_schema.clone(), Arc::clone(&self.ti));
            let e = b.build(sctx, self.plan.as_ref());
            if let Some(err) = b.err {
                return Err(err);
            }
            // ExecuteExec (D13.1) is not built by the new stack.
            let stmt_ctx = sctx.get_session_vars().stmt_ctx();
            self.is_select_for_update = b.has_lock
                && (!stmt_ctx.in_delete_stmt
                    && !stmt_ctx.in_update_stmt
                    && !stmt_ctx.in_insert_stmt);
            e.ok_or_else(|| Error::plain("no executor was built"))
        })();
        self.phase_build_durations[0] += start.elapsed();
        r
    }

    // go: ExecStmt.openExecutor L2 (panics are caught at the task boundary, M-PANIC)
    async fn open_executor(&mut self, ctx: &ExecCtx, e: &mut dyn Executor) -> Result<(), Error> {
        let start = Instant::now();
        let err = exec::open(ctx, e).await;
        self.phase_open_durations[0] += start.elapsed();
        err
    }

    // go: ExecStmt.next L1
    async fn next(
        &mut self,
        ctx: &ExecCtx,
        e: &mut dyn Executor,
        req: &mut Chunk,
    ) -> Result<(), Error> {
        let start = Instant::now();
        let err = exec::next(ctx, e, req).await;
        self.phase_next_durations[0] += start.elapsed();
        err
    }

    // go: ExecStmt.resetPhaseDurations L1
    pub fn reset_phase_durations(&mut self) {
        self.phase_build_durations[1] += self.phase_build_durations[0];
        self.phase_build_durations[0] = Duration::ZERO;
        self.phase_open_durations[1] += self.phase_open_durations[0];
        self.phase_open_durations[0] = Duration::ZERO;
        self.phase_next_durations[1] += self.phase_next_durations[0];
        self.phase_next_durations[0] = Duration::ZERO;
        self.phase_lock_durations[1] += self.phase_lock_durations[0];
        self.phase_lock_durations[0] = Duration::ZERO;
    }

    /// `commit_details`: Go's `*util.CommitDetails`, durations of the commit phases (the write
    /// path; `None` on the read-only node). The write-response phase is the protocol layer's
    /// (`StmtExecDetails.WriteSQLRespDuration`, passed in).
    // go: ExecStmt.observePhaseDurations L2 (the GoCtx value is a parameter, M-GOCTX)
    fn observe_phase_durations(
        &self,
        internal: bool,
        commit_details: Option<&[(Duration, &'static str)]>,
        write_sql_resp: Duration,
    ) {
        for (duration, phase) in [
            (
                self.phase_build_durations[0],
                executor_metrics::PhaseBuildFinal,
            ),
            (
                self.phase_build_durations[1],
                executor_metrics::PhaseBuildLocking,
            ),
            (
                self.phase_open_durations[0],
                executor_metrics::PhaseOpenFinal,
            ),
            (
                self.phase_open_durations[1],
                executor_metrics::PhaseOpenLocking,
            ),
            (
                self.phase_next_durations[0],
                executor_metrics::PhaseNextFinal,
            ),
            (
                self.phase_next_durations[1],
                executor_metrics::PhaseNextLocking,
            ),
            (
                self.phase_lock_durations[0],
                executor_metrics::PhaseLockFinal,
            ),
            (
                self.phase_lock_durations[1],
                executor_metrics::PhaseLockLocking,
            ),
        ] {
            if duration > Duration::ZERO {
                get_phase_duration_observer(phase, internal).observe(duration.as_secs_f64());
            }
        }
        if let Some(commit) = commit_details {
            for (duration, phase) in commit {
                if *duration > Duration::ZERO {
                    get_phase_duration_observer(phase, internal).observe(duration.as_secs_f64());
                }
            }
        }
        if write_sql_resp > Duration::ZERO {
            get_phase_duration_observer(executor_metrics::PhaseWriteResponse, internal)
                .observe(write_sql_resp.as_secs_f64());
        }
    }

    /// FinishExecuteStmt is used to record some information after `ExecStmt` execution
    /// finished:
    /// 1. record slow log if needed.
    /// 2. record summary statement.
    /// 3. record execute duration metric.
    /// 4. update the `PrevStmt` in session variable.
    /// 5. reset `DurationParse` in session variable.
    // go: ExecStmt.FinishExecuteStmt L2 (no slow log / summary / Top SQL / plan replayer: D14.2; no commit details: read-only)
    pub fn finish_execute_stmt(
        &mut self,
        sctx: &mut dyn SessionContext,
        txn_ts: u64,
        err: Option<&Error>,
        has_more_results: bool,
    ) {
        let _ = (txn_ts, has_more_results);
        // a.checkPlanReplayerCapture(txnTS): plan replayer (D13.x).
        // Commit / lock-keys runtime stats and the write-throughput SLI: the write path.
        if let Some(plan) = &self.plan {
            // If this statement has a Plan, the StmtCtx.plan should have been set when it
            // comes here, but we set it again in case we missed some code paths.
            let plan: Arc<dyn std::any::Any + Send + Sync> = Arc::new(Arc::clone(plan));
            Self::stmt_ctx_mut(sctx).set_plan(Some(plan));
        }
        // `LowSlowQuery` and `SummaryStmt` must be called before recording `PrevStmt`.
        // a.LogSlowQuery / a.SummaryStmt / a.observeStmtFinishedForTopSQL: D14.2.
        // TiFlash query counters: TiFlash is DEF.MPP.
        self.update_prev_stmt(sctx);
        self.record_last_query_info(sctx, err);
        // a.recordAffectedRows2Metrics(): DML affected rows (write path).
        let internal = sctx.get_session_vars().in_restricted_sql;
        self.observe_phase_durations(internal, None, Duration::ZERO);
        // SessionExecuteRunDuration{Internal,General}.Observe(sessVars.GetExecuteDuration()):
        // GetExecuteDuration is D05.2's, not in tidb-sessionctx yet.
        let (vars, _) = &*METRICS;
        let _: &Observer = if internal {
            &vars.session_execute_run_duration_internal
        } else {
            &vars.session_execute_run_duration_general
        };
        let sess_vars = sctx.get_session_vars_mut();
        // Reset DurationParse due to the next statement may not need to be parsed (not a text
        // protocol query).
        sess_vars.duration_parse = 0;
        let sc = sess_vars.stmt_ctx_mut();
        // Clean the stale read flag when statement execution finish
        sc.is_staleness = false;
        // Clean the MPP query info
        sc.mpp_query_id.store(0, Ordering::SeqCst);
        sc.mpp_query_ts.store(0, Ordering::SeqCst);
        sc.mpp_allocated_task_id.store(0, Ordering::SeqCst);
        sc.mpp_allocated_gather_id.store(0, Ordering::SeqCst);
        // ReadFromTableCacheCounter: cached tables are D06.4's (not on the new stack).
        // Fair locking counters: the write path. a.Ctx.ReportUsageStats(): telemetry.
    }

    /// CloseRecordSet will finish the execution of current statement and do some record work
    // go: ExecStmt.CloseRecordSet L2 (no audit plugins; trackers detach with the statement context, A1)
    pub fn close_record_set(
        &mut self,
        sctx: &mut dyn SessionContext,
        txn_start_ts: u64,
        last_err: Option<Error>,
    ) {
        self.finish_execute_stmt(sctx, txn_start_ts, last_err.as_ref(), false);
        // a.logAudit(): audit plugins are not ported.
        // StmtCtx.DetachMemDiskTracker(): the A1 trackers detach when the statement context
        // is dropped.
    }

    // go: ExecStmt.recordLastQueryInfo L2 (no RU details: resource control, N10)
    fn record_last_query_info(&self, sctx: &mut dyn SessionContext, err: Option<&Error>) {
        // Record diagnostic information for DML statements
        let record_last_query = match self.stmt_node.as_deref() {
            Some(ast::Stmt::Select(_)) => true,
            // ShowStmt (not SHOW SESSION_STATES), ExecuteStmt and the DML nodes: the new
            // stack runs SELECT only (VS1 router).
            _ => false,
        };
        if record_last_query {
            // RU consumption (util.RUDetailsCtxKey): resource control (N10), 0.
            let last_ru_consumption = 0.0;
            let sess_vars = sctx.get_session_vars_mut();
            // Keep the previous queryInfo for `show session_states` because the statement
            // needs to encode it.
            sess_vars.last_query_info = QueryInfo {
                txn_scope: sess_vars.check_and_get_txn_scope(),
                start_ts: sess_vars.state.txn_ctx.start_ts,
                for_update_ts: sess_vars.state.txn_ctx.get_for_update_ts(),
                ru_consumption: last_ru_consumption,
                err_msg: String::new(),
            };
            if let Some(err) = err {
                sess_vars.last_query_info.err_msg = err.to_string();
            }
        }
    }

    /// GetOriginalSQL returns the original SQL text (`getLazyStmtText().String()`).
    // go: ExecStmt.GetOriginalSQL L1
    pub fn get_original_sql(&self, sctx: &dyn SessionContext) -> Result<String, Error> {
        let mut stmt = self.get_lazy_stmt_text(sctx)?;
        let s = tidb_sessionctx::variable::lazy_text::LazyStmtText::string(Some(&mut stmt));
        Ok(String::from_utf8_lossy(&s).into_owned())
    }

    /// GetTextToLog return the query text to log.
    ///
    /// With `tidb_redact_log = ON` Go logs the normalized SQL (`parser.NormalizeKeepHint` /
    /// `StmtCtx.SQLDigest`), not ported yet (D02.1).
    // go: ExecStmt.GetTextToLog L2 (redact ON is a boundary stub, blocked-by:D02.1)
    pub fn get_text_to_log(
        &self,
        sctx: &dyn SessionContext,
        keep_hint: bool,
    ) -> Result<String, Error> {
        let _ = keep_hint;
        let sess_vars = sctx.get_session_vars();
        let rmode = sess_vars.enable_redact_log.as_str();
        if rmode == tidb_base::errors::RedactLogEnable {
            return Err(blocked("D02.1", "the normalized SQL of tidb_redact_log=ON"));
        }
        // SensitiveStmtNode: none reaches the new stack (VS1 router: SELECT only).
        let mut sql = sess_vars.stmt_ctx().original_sql.clone();
        sql.push_str(&String::from_utf8_lossy(
            &sess_vars.state.plan_cache_params.string(),
        ));
        Ok(tidb_base::redact::string(rmode, &sql))
    }

    /// getLazyText is equivalent to `a.GetTextToLog(false)`. Note that the s.Params is a
    /// shallow copy of `sessVars.PlanCacheParams`, so you can only use the lazy text within the
    /// current stmt context.
    // go: ExecStmt.getLazyStmtText L2 (redact ON is a boundary stub, blocked-by:D02.1)
    fn get_lazy_stmt_text(
        &self,
        sctx: &dyn SessionContext,
    ) -> Result<tidb_sessionctx::variable::lazy_text::LazyStmtText, Error> {
        let sess_vars = sctx.get_session_vars();
        let rmode = sess_vars.enable_redact_log.clone();
        if rmode == tidb_base::errors::RedactLogEnable {
            // sessVars.StmtCtx.SQLDigest(): the normalized SQL (parser.NormalizeDigest, D02.1).
            return Err(blocked("D02.1", "the normalized SQL of tidb_redact_log=ON"));
        }
        // SensitiveStmtNode: none reaches the new stack.
        let mut s = tidb_sessionctx::variable::lazy_text::LazyStmtText::default();
        s.redact = rmode;
        s.sql = sess_vars.stmt_ctx().original_sql.as_bytes().to_vec();
        s.params = sess_vars.state.plan_cache_params.clone();
        Ok(s)
    }

    /// updatePrevStmt is equivalent to `sessVars.PrevStmt = FormatSQL(a.GetTextToLog(false))`
    ///
    /// `SessionVars.PrevStmt` (read by the slow log and `@@tidb_last_query_info`'s neighbours)
    /// is not in tidb-sessionctx yet (D05.2 kept it with the slow log, D14.2).
    // go: ExecStmt.updatePrevStmt L2 (boundary stub, blocked-by:D14.2)
    fn update_prev_stmt(&self, sctx: &mut dyn SessionContext) {
        let _ = sctx;
        // glue: stub, blocked-by:D14.2 (SessionVars.PrevStmt)
    }
}

/// IsFastPlan exports for testing.
///
/// Go's `*plannercore.Set` (SET statements) is also a fast plan; it is not a plan of the new
/// stack (SET runs on the PoC, VS1 plan §4.1).
// go: IsFastPlan L1
pub fn is_fast_plan(p: &Plan) -> bool {
    let mut p = p;
    if p.kind() == PhysicalKind::Projection {
        p = &p.children()[0];
    }
    match p.kind() {
        PhysicalKind::PointGet => true,
        // Plan of following SQL is PhysicalTableDual:
        // select 1;
        // select @@autocommit;
        PhysicalKind::TableDual => true,
        _ => false,
    }
}

/// `isNoResultPlan`. Go also returns true for a projection with `CalculateNoDelay` (DO),
/// whose field comes with the projection operator (D10.1 / D09.x).
// go: isNoResultPlan L2 (CalculateNoDelay comes with the projection operators)
pub fn is_no_result_plan(p: &Plan) -> bool {
    if p.schema().is_empty() {
        return true;
    }
    false
}

/// chunkRowRecordSet: rows already read, returned as a record set (SELECT FOR UPDATE in a
/// pessimistic transaction). Go keeps `[]chunk.Row`; here the rows are copied into one chunk
/// (`M-ROW`).
pub struct ChunkRowRecordSet {
    rows: Chunk,
    idx: usize,
    fields: Option<Vec<ResultField>>,
    e: Box<dyn Executor>,
}

impl ChunkRowRecordSet {
    // go: chunkRowRecordSet.Fields L2 (the statement and the session are passed in)
    pub fn fields(&mut self, stmt: &ExecStmt, sctx: &dyn SessionContext) -> &[ResultField] {
        if self.fields.is_none() {
            self.fields = Some(col_names2_result_fields(
                &self.e.schema(),
                &stmt.output_names,
                &sctx.get_session_vars().current_db,
            ));
        }
        self.fields.as_deref().expect("set")
    }

    // go: chunkRowRecordSet.Next L1 [M-ROW]
    pub fn next(&mut self, chk: &mut Chunk) -> Result<(), Error> {
        chk.reset();
        let len = self.rows.num_rows();
        if !chk.is_full() && self.idx < len {
            let num_to_append = (len - self.idx).min(chk.required_rows() - chk.num_rows());
            for i in self.idx..self.idx + num_to_append {
                chk.append_row(self.rows.get_row(i));
            }
            self.idx += num_to_append;
        }
        Ok(())
    }

    // go: chunkRowRecordSet.NewChunk L1
    pub fn new_chunk(&self, alloc: Option<&mut dyn Allocator>) -> Chunk {
        let Some(alloc) = alloc else {
            return exec::new_first_chunk(self.e.as_ref());
        };

        alloc.alloc(
            self.e.ret_field_types(),
            self.e.init_cap().max(0) as usize,
            self.e.max_chunk_size().max(0) as usize,
        )
    }

    // go: chunkRowRecordSet.Close L2 (the statement and the session are passed in)
    pub fn close(
        &mut self,
        stmt: &mut ExecStmt,
        sctx: &mut dyn SessionContext,
    ) -> Result<(), Error> {
        let start_ts = sctx.get_session_vars().state.txn_ctx.start_ts;
        stmt.close_record_set(sctx, start_ts, None);
        Ok(())
    }
}

/// FormatSQL is used to format the original SQL, e.g. truncating long SQL, appending prepared
/// arguments. Go returns a lazy `stringutil.StringerFunc`; this is the closure.
// go: FormatSQL L1
pub fn format_sql_lazy(sql: String) -> impl Fn() -> String {
    move || format_sql(&sql)
}

/// `QueryReplacer`: replaces new line and tab for grep result including query string.
// glue: strings.NewReplacer("\r", " ", "\n", " ", "\t", " ").Replace
fn query_replacer_replace(s: &str) -> String {
    s.chars()
        .map(|c| {
            if matches!(c, '\r' | '\n' | '\t') {
                ' '
            } else {
                c
            }
        })
        .collect()
}

// go: formatSQL L1 [M-BYTES]
pub fn format_sql(sql: &str) -> String {
    let length = sql.len();
    let max_query_len =
        tidb_sessionctx::variable::tidb_vars::QueryLogMaxLen.load(Ordering::SeqCst) as i64;
    if max_query_len <= 0 {
        return query_replacer_replace(sql); // no limit
    }
    if length as i64 > max_query_len {
        let mut result = String::with_capacity(max_query_len as usize);
        result.push_str(&truncate_bytes(sql, max_query_len as usize));
        result.push_str(&format!("(len:{length})"));
        return query_replacer_replace(&result);
    }
    query_replacer_replace(sql)
}

// go: getPhaseDurationObserver L1
fn get_phase_duration_observer(phase: &str, internal: bool) -> Observer {
    let (vars, maps) = &*METRICS;
    if internal {
        if let Some(ob) = maps.internal.get(phase) {
            return ob.clone();
        }
        return vars.exec_unknown_internal.clone();
    }
    if let Some(ob) = maps.general.get(phase) {
        return ob.clone();
    }
    vars.exec_unknown.clone()
}

/// GetResultRowsCount gets the count of the statement result rows.
// go: GetResultRowsCount L1
pub fn get_result_rows_count(stmt_ctx: &StatementContext, p: &Plan) -> i64 {
    let Some(runtime_stats_coll) = &stmt_ctx.runtime_stats_coll else {
        return 0;
    };
    runtime_stats_coll.get_plan_act_rows(p.id() as i64)
}

#[cfg(test)]
pub(crate) mod tests;
