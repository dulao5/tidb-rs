//! go-file: pkg/session/session.go
//!
//! The session (Go `session`): statement execution of the new stack (VS1-14 part 1; spec
//! `specs/D05.3-session/spec.md`). It takes statements as AST (ADR 0010 §4.2: parsing is
//! the protocol adapter's), runs them with the executor framework (D12.0) and the
//! transaction context providers (D05.4), and answers questions about its state.
//!
//! Layout (N2, spec §2.1): [`Session`] = [`SessionCore`] (what `ExecStmt` and the compiler
//! see: [`SessCtx`], the part the providers see, plus the transaction manager) + the
//! statement whose rows are being read ([`crate::exec_result`]).
//!
//! Prepared statements (part 2, VS1-14b, spec §6): `PrepareStmt` runs Go's transaction
//! steps and stops at the prepare executor; `ExecutePreparedStmt` stops after the lookup:
//! both need the plan-cache statement of prepared-statement planning (VS1-15).

use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use futures::FutureExt;
use futures::future::BoxFuture;
use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_base::dbterror::plannererrors::ErrStmtNotFound;
use tidb_executor::adapter::ExecStmt;
use tidb_executor::compiler::Compiler;
use tidb_executor::glue::{Plan, SessionContext};
use tidb_executor::select::reset_context_of_stmt;
use tidb_kv::kv::{Client, Storage};
use tidb_memory::sqlkiller::SqlKiller;
use tidb_parser::ast;
use tidb_sessionctx::variable::session::{GlobalVarAccessor, SessionVars};
use tidb_sessionctx::variable::session_methods::prepare_dedup_cache_key;
use tidb_sessionctx::variable::session_state::TxnInfoSchema;
use tidb_sessiontxn::sctx::{
    InfoSchemaRef, LazyTxn as SctxLazyTxn, Oracle, PlanInfo, SessionCtx, TxnTsFuture,
};
use tidb_sessiontxn::{EnterNewTxnRequest, EnterNewTxnType};
use tidb_types::datum::Datum;

use crate::exec_result::{ExecStmtResult, RunningStmt};
use crate::glue::{PhysicalPlanInfo, UnwiredStore};
use crate::tidb::finish_stmt;
use crate::txn::{LazyTxn, TxnFuture};
use crate::txnmanager::{Optimistic, Pessimistic, TxnManager, new_txn_manager};

/// The SQL dialect of a connection (ADR 0010 §4.4): which syntax front end parses its
/// statements, part of the plan cache key. Set when the connection is opened, fixed for the
/// session's life; not a system variable.
// glue: ADR 0010 §4.4
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Dialect {
    /// MySQL syntax (`tidb-parser`), the only front end.
    #[default]
    MySQL,
}

/// What `PrepareStmt` returns (Go: `stmtID, paramCount, fields`).
///
/// TODO(VS1-15): the result fields (`[]*resolve.ResultField`), built by the prepare executor
/// that comes with prepared-statement planning.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedStmtInfo {
    /// The session's id of the statement (Go `SessionVars.GetNextPreparedStmtID`), not the
    /// protocol's: the adapter maps between them (ADR 0010 §4.1).
    pub stmt_id: u32,
    /// Number of `?` placeholders.
    pub param_count: usize,
}

/// `sessionctx.QueryString`: the key of the current statement's text in the session values.
#[allow(non_upper_case_globals)] // M-GONAME
pub const QueryString: &str = "query_string";
/// `sessionctx.LastExecuteDDL`: whether the last statement was a DDL.
#[allow(non_upper_case_globals)] // M-GONAME
pub const LastExecuteDDL: &str = "last_execute_ddl";

/// What `SetProcessInfo` was last called with (`util.ProcessInfo` is not ported, spec §1.2).
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessArgs {
    pub sql: String,
    pub time: SystemTime,
    pub command: u8,
    pub max_execution_time: u64,
}

/// The part of the session the transaction context providers see (sessiontxn's
/// `sessionctx.Context`): variables, the transaction, the store.
pub struct SessCtx {
    /// The session variables: the only source of truth for session state (VS1 plan §4.1).
    pub vars: SessionVars,
    pub(crate) txn: LazyTxn,
    /// `SessionVars.SQLKiller`: the shared part of N2 (`SessionShared`).
    pub(crate) killer: Arc<SqlKiller>,
    store: Arc<dyn Storage>,
    oracle: Arc<dyn Oracle>,
    /// `domain.GetDomain(s).InfoSchema()`: the domain is D06.5 (VS1-17 feeds it); until then
    /// an empty v2 InfoSchema.
    domain_is: InfoSchemaRef,
    /// `mu.values`.
    values: HashMap<String, Box<dyn Any + Send>>,
    /// The transaction ended inside a provider call (`CommitBeforeEnterNewTxn`): the
    /// manager's `OnTxnEnd` is still to run (txnmanager.rs module doc).
    txn_end: bool,
    /// The expression context the session gives its executors (`GetExprCtx`): a static
    /// context with the defaults until the session's own (`sessionexpr`, D05.3 tasks P1-1).
    expr_ctx: tidb_sessionctx::exprctx::ExprContextRef,
    /// `currentPlan`.
    pub(crate) current_plan: Option<Plan>,
    /// `SetProcessInfo` (spec §1.2).
    pub(crate) process: Option<ProcessArgs>,
    /// `sessionVars.StartTime`.
    pub(crate) start_time: Option<Instant>,
    /// `sessionVars.CommonGlobalLoaded`.
    common_global_loaded: bool,
}

/// The session as `ExecStmt`, the compiler and the transaction manager's helpers see it.
pub struct SessionCore {
    pub(crate) sctx: SessCtx,
    pub(crate) txn_manager: TxnManager,
}

/// One client session (N2 `Session`; Go `session`). Owned by the connection's task; `Send`.
pub struct Session {
    pub(crate) core: SessionCore,
    /// The statement whose rows are being read (spec §2.3).
    pub(crate) running: Option<RunningStmt>,
    /// The sequence number of the last statement that returned a result: a result only acts
    /// on its own statement (spec §2.3).
    next_seq: u64,
    /// Statements whose executors were built and opened (`ExecStmt.Exec` / `PointGet`
    /// called): the router checks that no fallback happens after that (VS1 plan §4.3).
    executed_stmts: u64,
    dialect: Dialect,
}

// N2 §5: the session moves with its connection task between worker threads (VS1-04).
const _: () = {
    // glue: compile-time check
    const fn send<T: Send>() {}
    send::<Session>();
    send::<SessionCore>();
    send::<ExecStmtResult>();
    send::<ast::Stmt>();
};

impl SessCtx {
    // go: session.GetSessionVars L1
    pub fn get_session_vars(&self) -> &SessionVars {
        &self.vars
    }

    // go: session.GetStore L1
    pub fn get_store(&self) -> Arc<dyn Storage> {
        Arc::clone(&self.store)
    }

    // go: session.GetClient L1
    pub fn get_client(&self) -> Arc<dyn Client> {
        self.store.get_client()
    }

    /// `GetDomainInfoSchema()`: Go wraps it in a `SessionExtendedInfoSchema` with the local
    /// temporary tables (D06.3 T9, D06.4).
    // go: session.GetDomainInfoSchema L2 (no SessionExtendedInfoSchema / temporary tables: D06.3 T9)
    pub fn get_domain_info_schema(&self) -> InfoSchemaRef {
        Arc::clone(&self.domain_is)
    }

    /// `GetInfoSchema()`: the snapshot schema (`SnapshotInfoschema`, D06.5; `tidb_snapshot`
    /// statements run on the PoC), else the transaction's, else the domain's.
    // go: session.GetInfoSchema L2 (no SnapshotInfoschema / temporary tables: D06.5, D06.4)
    pub fn get_info_schema(&self) -> InfoSchemaRef {
        if let Some(is) = &self.vars.state.txn_ctx.info_schema
            && let Ok(is) = Arc::clone(&is.0).downcast::<tidb_infoschema::InfoSchemaV2>()
        {
            return is;
        }
        self.get_domain_info_schema()
    }

    // go: session.isInternal L1
    pub fn is_internal(&self) -> bool {
        self.vars.in_restricted_sql
    }

    // go: session.GetSQLLabel L1
    pub fn get_sql_label(&self) -> &'static str {
        if self.vars.in_restricted_sql {
            return "internal"; // metrics.LblInternal
        }
        "general" // metrics.LblGeneral
    }

    // go: session.AffectedRows L1
    pub fn affected_rows(&self) -> u64 {
        self.vars.stmt_ctx().affected_rows()
    }

    // go: session.LastMessage L1
    pub fn last_message(&self) -> String {
        self.vars.stmt_ctx().get_message()
    }

    // go: session.Status L1
    pub fn status(&self) -> u16 {
        self.vars.status()
    }

    // go: session.SetValue L1
    pub fn set_value(&mut self, key: &str, value: Box<dyn Any + Send>) {
        self.values.insert(key.to_owned(), value);
    }

    // go: session.Value L1
    pub fn value(&self, key: &str) -> Option<&(dyn Any + Send)> {
        self.values.get(key).map(|v| &**v)
    }

    // go: session.ClearValue L1
    pub fn clear_value(&mut self, key: &str) {
        self.values.remove(key);
    }

    // go: session.SetConnectionID L1
    pub fn set_connection_id(&mut self, conn_id: u64) {
        self.vars.connection_id = conn_id;
    }

    // go: session.SetClientCapability L1
    pub fn set_client_capability(&mut self, capability: u32) {
        self.vars.client_capability = capability;
    }

    // go: session.SetCommandValue L1
    pub fn set_command_value(&mut self, command: u8) {
        self.vars.command_value = command as u32;
    }

    /// The transaction (`Txn(false)`).
    // glue: accessor (Go &s.txn)
    pub fn lazy_txn(&self) -> &LazyTxn {
        &self.txn
    }

    /// `GetPreparedTxnFuture() != nil`.
    // go: session.GetPreparedTxnFuture L1
    pub fn get_prepared_txn_future(&self) -> bool {
        self.txn.valid_or_pending()
    }

    /// PrepareTSFuture uses to try to get ts future.
    ///
    /// `usePipelinedDmlOrWarn` (pipelined DML) is the write path: false.
    // go: session.PrepareTSFuture L1
    pub fn prepare_ts_future(&mut self, future: TxnTsFuture, scope: &str) -> Result<(), Error> {
        if self.txn.valid() {
            return Err(tidb_base::errors::new(
                "cannot prepare ts future when txn is valid",
            ));
        }

        // failpoints assertTSONotRequest / mockGetTSFail: not ported.

        self.txn.change_to_pending(TxnFuture::new(
            future,
            Arc::clone(&self.store),
            scope.to_owned(),
            false,
        ));
        Ok(())
    }

    /// `doCommit`: a read-only transaction (the only kind on this node) is just invalidated.
    // go: session.doCommit L2 (the commit of writes is the write path, blocked-by:X)
    async fn do_commit(&mut self) -> Result<(), Error> {
        if !self.txn.valid() {
            return Ok(());
        }

        let r = if self.txn.is_read_only() {
            // check if the transaction is read-only
            Ok(())
        } else {
            // The read-only / placement checks, cached-table leases, SetOptionsBeforeCommit
            // and the commit with temporary data: the write path (N11 D8).
            // glue: stub, blocked-by:X (committing writes)
            Err(blocked("X", "committing a transaction with writes"))
        };
        self.txn.change_to_invalid();
        self.vars.set_in_txn(false);
        // ClearDiskFullOpt: the disk-full option is a write option.
        r
    }

    /// `doCommitWithRetry` without the manager's `OnTxnEnd` (the caller runs it). A
    /// read-only commit does not fail, so nothing is retried (`CouldRetry` is always false,
    /// D05.4 spec §4).
    // go: session.doCommitWithRetry L2 (no retry and no commit metrics: read-only; OnTxnEnd is the caller's)
    async fn do_commit_with_retry(&mut self) -> Result<(), Error> {
        let r = if !self.txn.valid() {
            // If the transaction is invalid, maybe it has already been rolled back by the
            // client.
            Ok(())
        } else {
            let err = self.do_commit().await;
            // tryReplaceWriteConflictError / retry: write conflicts (write path).
            // recordOnTransactionExecution(err, TxnCtx.StatementCount, ...): D14.1.
            if let Err(e) = &err
                && !err_is_noisy(e)
            {
                log::warn!("commit failed: {e}");
            }
            // updateStatsDeltaToCollector: table deltas (write path).
            err
        };
        self.vars
            .set_txn_isolation_level_one_shot_state_for_next_txn();
        self.txn.change_to_invalid();
        // cleanRetryInfo: the retry of optimistic writes.
        r
    }

    /// `CommitTxn` without the manager's `OnTxnEnd`.
    // go-part: session.CommitTxn
    async fn commit_txn_state(&mut self) -> Result<(), Error> {
        let err = self.do_commit_with_retry().await;
        // commit details / TTL metrics: the write path.
        self.vars.state.txn_ctx.cleanup();
        self.vars.cleanup_txn_read_ts_if_used();
        err
    }

    /// `RollbackTxn` without the manager's `OnTxnEnd`.
    // go-part: session.RollbackTxn
    pub(crate) fn rollback_txn_state(&mut self) {
        if self.txn.valid()
            && let Err(e) = self.txn.rollback()
        {
            log::error!("{e}");
        }
        // cleanRetryInfo: the retry of optimistic writes.
        self.txn.change_to_invalid();
        self.vars.state.txn_ctx.cleanup();
        self.vars.cleanup_txn_read_ts_if_used();
        self.vars.set_in_txn(false);
        // metrics.DDLRollbackTempIndexWrite: DDL (delegated).
    }

    /// Whether a transaction ended inside a provider call, and clears it.
    // glue: txnmanager.rs module doc
    pub(crate) fn take_txn_end(&mut self) -> Option<()> {
        std::mem::take(&mut self.txn_end).then_some(())
    }

    /// `loadCommonGlobalVariablesIfNeeded`: the PoC router syncs the session variables
    /// (VS1 plan §4.1) and the global values are the compiled defaults until the domain's
    /// sysvar cache (D06.5, VS1-17).
    // glue: stub of session.loadCommonGlobalVariablesIfNeeded, blocked-by:D06.5 (domain.GetSessionCache)
    fn load_common_global_variables_if_needed(&mut self) -> Result<(), Error> {
        self.common_global_loaded = true;
        Ok(())
    }

    /// The coprocessor client's per-session values (`tikv.Variables`, Go's
    /// `SessionVars.KVVars` = `NewVariables(&SQLKiller.Signal)`): the worker's `finished()`
    /// reads the kill signal (spec §2.5).
    // glue: SessionVars.KVVars (tikvstore.NewVariables(&vars.SQLKiller.Signal))
    pub fn copr_vars(&self) -> tidb_store::copr::tikv::Variables {
        tidb_store::copr::tikv::Variables {
            backoff_weight: self.vars.state.kv_vars.back_off_weight as i64,
            killed: Some(self.killer.signal_handle()),
        }
    }
}

impl SessionCtx for SessCtx {
    // glue: interface method sessionctx.Context.GetSessionVars
    fn get_session_vars(&self) -> &SessionVars {
        &self.vars
    }
    // glue: interface method sessionctx.Context.GetSessionVars (written through the pointer)
    fn get_session_vars_mut(&mut self) -> &mut SessionVars {
        &mut self.vars
    }
    // glue: interface method sessionctx.Context.Txn(false)
    fn txn(&mut self) -> &mut dyn SctxLazyTxn {
        &mut self.txn
    }
    // glue: interface method sessionctx.Context.GetPreparedTxnFuture
    fn has_prepared_txn_future(&self) -> bool {
        self.get_prepared_txn_future()
    }
    // glue: interface method sessionctx.TxnFuture.Wait (LazyTxn.Wait)
    fn wait_prepared_txn_future(&mut self) -> BoxFuture<'_, Result<(), Error>> {
        async move { self.txn.wait(&mut self.vars).await }.boxed()
    }
    // glue: interface method sessionctx.Context.PrepareTSFuture
    fn prepare_ts_future(&mut self, future: TxnTsFuture, scope: &str) -> Result<(), Error> {
        SessCtx::prepare_ts_future(self, future, scope)
    }
    /// The manager's `OnTxnEnd` runs when the provider returns (txnmanager.rs module doc).
    // glue: interface method sessionctx.Context.CommitTxn
    fn commit_txn(&mut self) -> BoxFuture<'_, Result<(), Error>> {
        async move {
            let r = self.commit_txn_state().await;
            self.txn_end = true;
            r
        }
        .boxed()
    }
    // glue: interface method sessionctx.Context.GetDomainInfoSchema
    fn get_domain_info_schema(&self) -> InfoSchemaRef {
        SessCtx::get_domain_info_schema(self)
    }
    // glue: interface method sessionctx.Context.GetStore
    fn get_store(&self) -> Arc<dyn Storage> {
        SessCtx::get_store(self)
    }
    // glue: interface method kv.Storage.GetOracle (D07.1)
    fn get_oracle(&self) -> Arc<dyn Oracle> {
        Arc::clone(&self.oracle)
    }
}

impl SessionCore {
    /// The session variables.
    // glue: accessor (Go session.GetSessionVars)
    pub fn vars(&self) -> &SessionVars {
        &self.sctx.vars
    }

    /// `Txn(true)`: activates the transaction through the manager.
    // go: session.Txn L2 (returns the session's LazyTxn after activation; no memory footprint hook)
    pub async fn txn(&mut self, active: bool) -> Result<&LazyTxn, Error> {
        if !active {
            return Ok(&self.sctx.txn);
        }
        self.txn_manager.activate_txn(&mut self.sctx).await?;
        // SetMemoryFootprintChangeHook: the memory of writes (write path).
        Ok(&self.sctx.txn)
    }

    // go: session.CommitTxn L2 (the manager's OnTxnEnd after the cleanup; no commit details: read-only)
    pub async fn commit_txn(&mut self) -> Result<(), Error> {
        let r = self.sctx.commit_txn_state().await;
        self.txn_manager.on_txn_end(&self.sctx);
        r
    }

    // go: session.RollbackTxn L1
    pub fn rollback_txn(&mut self) {
        self.sctx.rollback_txn_state();
        self.txn_manager.on_txn_end(&self.sctx);
    }

    /// RefreshTxnCtx implements context.RefreshTxnCtx interface.
    // go: session.RefreshTxnCtx L2 (no commit details / stats delta: read-only)
    pub async fn refresh_txn_ctx(&mut self) -> Result<(), Error> {
        self.sctx.do_commit().await?;

        // updateStatsDeltaToCollector: the write path.

        tidb_sessiontxn::new_txn(self).await
    }

    /// rollbackOnError makes sure the next statement starts a new transaction with the latest
    /// InfoSchema.
    // go: session.rollbackOnError L1
    pub fn rollback_on_error(&mut self) {
        if !self.sctx.vars.in_txn() {
            self.rollback_txn();
        }
    }

    /// PrepareTxnCtx begins a transaction, and creates a new transaction context.
    // go: session.PrepareTxnCtx L2 (RetryInfo.Retrying is the write path, always false)
    pub async fn prepare_txn_ctx(&mut self) -> Result<(), Error> {
        if self.sctx.txn.valid_or_pending() {
            return Ok(());
        }

        let vars = &self.sctx.vars;
        let mut txn_mode = Optimistic;
        let pessimistic_auto_commit = tidb_base::config::get_global_config()
            .pessimistic_txn
            .pessimistic_auto_commit
            .load();
        if (!vars.is_autocommit() || pessimistic_auto_commit) && vars.txn_mode == Pessimistic {
            // (... && !BulkDMLEnabled: pipelined DML is the write path, never enabled.)
            txn_mode = Pessimistic;
        }

        // sessionVars.RetryInfo.Retrying: the retry of optimistic writes, always false.

        self.txn_manager
            .enter_new_txn(
                &mut self.sctx,
                EnterNewTxnRequest {
                    tp: EnterNewTxnType::EnterNewTxnBeforeStmt,
                    provider: None,
                    txn_mode: txn_mode.to_owned(),
                    causal_consistency_only: false,
                    stale_read_ts: 0,
                },
            )
            .await
    }

    // go: session.onTxnManagerStmtStartOrRetry L2 (RetryInfo.Retrying is the write path)
    fn on_txn_manager_stmt_start_or_retry(&mut self, node: Arc<ast::Stmt>) -> Result<(), Error> {
        // if s.sessionVars.RetryInfo.Retrying { return OnStmtRetry(ctx) }: never.
        self.txn_manager.on_stmt_start(&mut self.sctx, Some(node))
    }

    /// Internal statements read the request source from the context (`kv.RequestSourceKey`):
    /// the new stack runs no internal statement.
    // go: session.setRequestSource L2 (no internal statements on the new stack)
    fn set_request_source(&mut self, stmt_label: &str) {
        if !self.sctx.is_internal() {
            let mut label = stmt_label.to_owned();
            if self.sctx.txn.valid() {
                if SctxLazyTxn::is_pipelined(&self.sctx.txn) {
                    label = "pdml".to_owned();
                }
                SctxLazyTxn::set_option(
                    &mut self.sctx.txn,
                    tidb_kv::option::RequestSourceType,
                    Box::new(label.clone()),
                );
            }
            self.sctx.vars.request_source_type = label;
            return;
        }
        log::warn!(
            "unexpected no source type context, if you see this warning, the `RequestSourceTypeKey` is missing in the context"
        );
    }

    /// SetProcessInfo sets the process info of the session (SHOW PROCESSLIST, KILL, the
    /// expensive query checker). `util.ProcessInfo` is not ported (D00.3): the arguments are
    /// kept for it.
    // glue: stub of session.SetProcessInfo, blocked-by:D00.3 (util.ProcessInfo; SHOW PROCESSLIST runs on the PoC)
    pub fn set_process_info(
        &mut self,
        sql: &str,
        t: SystemTime,
        command: u8,
        max_execution_time: u64,
    ) {
        self.sctx.process = Some(ProcessArgs {
            sql: sql.to_owned(),
            time: t,
            command,
            max_execution_time,
        });
    }
}

/// The getters of the session's contexts and of the server's objects that blocks not ported
/// yet provide (spec §1.2). They are not called: the planner gets a `PlanCtx` (N2) from
/// `Compile`, which is still a boundary stub.
impl SessionCore {
    /// `GetPlanCtx()`.
    // glue: stub of session.GetPlanCtx, blocked-by:D05.3 (P1-1: the PlanCtx is built over the session's ExprContext)
    pub fn get_plan_ctx(&self) -> Result<(), Error> {
        Err(blocked("D05.3", "the session's PlanContext"))
    }

    /// `GetRangerCtx()`.
    // glue: stub of session.GetRangerCtx, blocked-by:D05.3 (P1-1: the RangerContext holds the session's ExprContext)
    pub fn get_ranger_ctx(&self) -> Result<(), Error> {
        Err(blocked("D05.3", "the session's RangerContext"))
    }

    /// `GetDistSQLCtx()`: the `DistSQLContext` of the coprocessor reads (its KV variables are
    /// [`SessCtx::copr_vars`]).
    // glue: stub of session.GetDistSQLCtx, blocked-by:D07.3 (VS1-33: distsql DistSQLContext)
    pub fn get_dist_sql_ctx(&self) -> Result<(), Error> {
        Err(blocked("D07.3", "the session's DistSQLContext"))
    }

    /// `GetBuildPBCtx()`.
    // glue: stub of session.GetBuildPBCtx, blocked-by:D10.9 (planctx.BuildPBContext of ToPB)
    pub fn get_build_pb_ctx(&self) -> Result<(), Error> {
        Err(blocked("D10.9", "the session's BuildPBContext"))
    }

    /// `GetTableCtx()`.
    // glue: stub of session.GetTableCtx, blocked-by:D06.4 (tblctx: the table context of writes)
    pub fn get_table_ctx(&self) -> Result<(), Error> {
        Err(blocked("D06.4", "the session's table context"))
    }

    /// `GetDomain()`.
    // glue: stub of session.GetDomain, blocked-by:D06.5 (the domain)
    pub fn get_domain(&self) -> Result<(), Error> {
        Err(blocked("D06.5", "the domain of the new session"))
    }

    /// `GetSessionManager()`.
    // glue: stub of session.GetSessionManager, blocked-by:D03.1 (the server's session manager)
    pub fn get_session_manager(&self) -> Result<(), Error> {
        Err(blocked("D03.1", "the session manager"))
    }

    /// `SetSessionManager(sm)`.
    // glue: stub of session.SetSessionManager, blocked-by:D03.1 (the server's session manager)
    pub fn set_session_manager(&mut self) -> Result<(), Error> {
        Err(blocked("D03.1", "the session manager"))
    }

    /// `SetCollation(coID)`: the handshake's collation.
    // glue: stub of session.SetCollation, blocked-by:D02.2 (charset.GetCharsetInfoByID; the router syncs SET NAMES)
    pub fn set_collation(&mut self, co_id: i32) -> Result<(), Error> {
        let _ = co_id;
        Err(blocked("D02.2", "charset.GetCharsetInfoByID"))
    }

    /// `ShowProcess()`.
    // glue: stub of session.ShowProcess, blocked-by:D00.3 (util.ProcessInfo; SHOW PROCESSLIST runs on the PoC)
    pub fn show_process(&self) -> Option<&ProcessArgs> {
        self.sctx.process.as_ref()
    }

    /// `UpdateProcessInfo()`.
    // glue: stub of session.UpdateProcessInfo, blocked-by:D00.3 (util.ProcessInfo)
    pub fn update_process_info(&mut self) {}
}

/// `getSnapshotInfoSchema(s, snapshotTS)`: the InfoSchema at `tidb_snapshot`.
// glue: stub of getSnapshotInfoSchema, blocked-by:D06.5 (domain.GetSnapshotInfoSchema; tidb_snapshot runs on the PoC)
pub fn get_snapshot_info_schema(snapshot_ts: u64) -> Result<InfoSchemaRef, Error> {
    let _ = snapshot_ts;
    Err(blocked("D06.5", "the InfoSchema at tidb_snapshot"))
}

impl SessionContext for SessionCore {
    // glue: interface method sessionctx.Context.GetSessionVars
    fn get_session_vars(&self) -> &SessionVars {
        &self.sctx.vars
    }
    // glue: interface method sessionctx.Context.GetSessionVars
    fn get_session_vars_mut(&mut self) -> &mut SessionVars {
        &mut self.sctx.vars
    }
    // glue: SessionVars.SQLKiller (N2 SessionShared)
    fn sql_killer(&self) -> Arc<SqlKiller> {
        Arc::clone(&self.sctx.killer)
    }
    // glue: interface method sessiontxn.TxnManager.GetStmtReadTS
    fn get_stmt_read_ts(&mut self) -> BoxFuture<'_, Result<u64, Error>> {
        async move { self.txn_manager.get_stmt_read_ts(&mut self.sctx).await }.boxed()
    }
    // glue: interface method sessiontxn.TxnManager.GetTxnInfoSchema
    fn get_txn_info_schema(&mut self) -> Option<InfoSchemaRef> {
        self.txn_manager.get_txn_info_schema(&mut self.sctx)
    }
    // glue: interface method sessiontxn.TxnManager.GetTxnScope
    fn get_txn_scope(&self) -> String {
        self.txn_manager.get_txn_scope(&self.sctx)
    }
    // glue: interface method sessiontxn.TxnManager.GetReadReplicaScope
    fn get_read_replica_scope(&self) -> String {
        self.txn_manager.get_read_replica_scope(&self.sctx)
    }
    // glue: sessiontxn/staleread.IsStmtStaleness (D05.4)
    fn is_stmt_staleness(&self) -> bool {
        tidb_sessiontxn::staleread::is_stmt_staleness(&self.sctx)
    }
    // glue: interface method sessionctx.Context.Txn (Txn(false).StartTS() when valid)
    fn txn_start_ts(&mut self) -> Result<Option<u64>, Error> {
        if self.sctx.txn.valid() {
            return Ok(Some(SctxLazyTxn::start_ts(&self.sctx.txn)));
        }
        Ok(None)
    }
    // glue: interface method processinfoSetter.SetProcessInfo (D05.3)
    fn set_process_info(&mut self, sql: &str, t: SystemTime, command: u8, max_execution_time: u64) {
        SessionCore::set_process_info(self, sql, t, command, max_execution_time);
    }
    // glue: sessiontxn.AdviseOptimizeWithPlanAndThenWarmUp (D05.4)
    fn advise_optimize_with_plan_and_then_warm_up(&mut self, plan: &Plan) -> Result<(), Error> {
        let info = PhysicalPlanInfo::new(plan.as_dyn());
        tidb_sessiontxn::advise_optimize_with_plan_and_then_warm_up(self, Some(&info))
    }
    /// `GetExprCtx()`: the session's `ExprContext` is `sessionexpr` (D05.3 tasks P1-1, not
    /// ported): until then a static context with Go's defaults, not the session's variables
    /// (spec §1.2). No statement reaches the executors yet (`Compile` is a boundary stub).
    // glue: stub of session.GetExprCtx, blocked-by:D05.3 (P1-1: sessionexpr; a static default context until then)
    fn get_expr_ctx(&self) -> tidb_sessionctx::exprctx::ExprContextRef {
        Arc::clone(&self.sctx.expr_ctx) as tidb_sessionctx::exprctx::ExprContextRef
    }
    // glue: interface method sessiontxn.TxnManager.GetSnapshotWithStmtReadTS
    fn get_snapshot_with_stmt_read_ts(
        &mut self,
    ) -> BoxFuture<'_, Result<Arc<dyn tidb_kv::kv::Snapshot>, Error>> {
        async move {
            self.txn_manager
                .get_snapshot_with_stmt_read_ts(&mut self.sctx)
                .await
        }
        .boxed()
    }
}

impl tidb_sessiontxn::TxnManager for SessionCore {
    // glue: interface method TxnManager.AdviseWarmup
    fn advise_warmup(&mut self) -> Result<(), Error> {
        self.txn_manager.advise_warmup(&mut self.sctx)
    }
    // glue: interface method TxnManager.AdviseOptimizeWithPlan
    fn advise_optimize_with_plan(&mut self, plan: Option<&dyn PlanInfo>) -> Result<(), Error> {
        self.txn_manager
            .advise_optimize_with_plan(&mut self.sctx, plan)
    }
    // glue: interface method TxnManager.EnterNewTxn
    fn enter_new_txn(&mut self, req: EnterNewTxnRequest) -> BoxFuture<'_, Result<(), Error>> {
        async move { self.txn_manager.enter_new_txn(&mut self.sctx, req).await }.boxed()
    }
    // glue: interface methods TxnManager.OnStmtStart and TxnManager.GetCurrentStmt
    fn on_stmt_start_with_current_stmt(&mut self) -> Result<(), Error> {
        let node = self.txn_manager.get_current_stmt().cloned();
        self.txn_manager.on_stmt_start(&mut self.sctx, node)
    }
}

/// `errIsNoisy`: errors not worth a warning log.
// go: errIsNoisy L1
pub fn err_is_noisy(err: &Error) -> bool {
    if tidb_kv::error::ErrKeyExists.equal(err) {
        return true;
    }
    if tidb_store::driver::error::ErrLockAcquireFailAndNoWaitSet.equal(err) {
        return true;
    }
    false
}

/// `EXECUTE` (Go reads the prepared statement, D13.1) is not in the Rust AST.
// go: isEndTxnStmt L1 [M-ERRVAL]
pub fn is_end_txn_stmt(stmt: &ast::Stmt, vars: &SessionVars) -> Result<bool, Error> {
    let _ = vars;
    match stmt {
        ast::Stmt::Rollback(_) | ast::Stmt::Commit(_) => Ok(true),
        // *ast.ExecuteStmt: plannercore.GetPreparedStmt (D13.1), not in the Rust AST.
        _ => Ok(false),
    }
}

/// The crucial operations Go logs (DDL, user and privilege statements) are not in the Rust
/// AST and never reach the new stack: every statement is a general query.
// go: logStmt L2 (the crucial statements are not in the Rust AST)
fn log_stmt(exec_stmt: &ExecStmt, s: &SessionCore) {
    log_general_query(exec_stmt, s, false);
}

// go: logGeneralQuery L2 (the user's login string comes with D04.1)
fn log_general_query(exec_stmt: &ExecStmt, s: &SessionCore, is_prepared: bool) {
    let vars = &s.sctx.vars;
    if tidb_sessionctx::variable::tidb_vars::ProcessGeneralLog
        .load(std::sync::atomic::Ordering::SeqCst)
        && !vars.in_restricted_sql
    {
        let mut query = if is_prepared {
            exec_stmt.origin_text().to_owned()
        } else {
            match exec_stmt.get_text_to_log(s, false) {
                Ok(q) => q,
                Err(e) => e.to_string(),
            }
        };

        query = tidb_executor::adapter::format_sql(&query);
        if vars.enable_redact_log != tidb_base::errors::RedactLogEnable {
            query.push_str(&tidb_base::redact::string(
                &vars.enable_redact_log,
                &String::from_utf8_lossy(&vars.state.plan_cache_params.string()),
            ));
        }
        log::info!(
            target: "general_log",
            "GENERAL_LOG conn={} session_alias={} user={:?} schemaVersion={} txnStartTS={} \
             forUpdateTS={} isReadConsistency={} currentDB={} isPessimistic={} \
             sessionTxnMode={} sql={}",
            vars.connection_id,
            vars.session_alias,
            "", // vars.User.LoginString(): the user comes with D04.1
            tidb_sessionctx::expropt::MetaOnlyInfoSchema::schema_meta_version(&*s.sctx.get_info_schema()),
            vars.state.txn_ctx.start_ts,
            vars.state.txn_ctx.get_for_update_ts(),
            vars.state.txn_ctx.isolation == "READ-COMMITTED",
            vars.current_db,
            vars.state.txn_ctx.is_pessimistic,
            vars.get_readable_txn_mode(),
            query
        );
    }
}

/// `sqlLogMaxLen`: the longest SQL text in a log line.
const SQL_LOG_MAX_LEN: usize = 1024;

// go: sqlForLog L1
pub fn sql_for_log(sql: &str) -> String {
    let mut sql = sql.to_owned();
    if sql.len() > SQL_LOG_MAX_LEN {
        let mut cut = SQL_LOG_MAX_LEN;
        while !sql.is_char_boundary(cut) {
            cut -= 1;
        }
        let len = sql.len();
        sql.truncate(cut);
        sql.push_str(&format!("(len:{len})"));
    }
    tidb_executor::adapter::format_sql(&sql)
}

impl Session {
    /// A session with Go's default variables. `global` answers the global values of system
    /// variables (Go's `GlobalVarAccessor`, the domain's sysvar cache). The store is
    /// [`UnwiredStore`] until the store driver is wired (spec §1.2).
    // glue: stand-in for createSession / CreateSession4Test (D05.3)
    pub fn new(dialect: Dialect, global: Box<dyn GlobalVarAccessor>) -> Session {
        let store = Arc::new(UnwiredStore);
        Session::with_store(dialect, global, Arc::<UnwiredStore>::clone(&store), store)
    }

    /// A session over `store` and its TSO.
    // glue: stand-in for createSession (D05.3)
    pub fn with_store(
        dialect: Dialect,
        global: Box<dyn GlobalVarAccessor>,
        store: Arc<dyn Storage>,
        oracle: Arc<dyn Oracle>,
    ) -> Session {
        let mut vars = SessionVars::new(global);
        let killer = SqlKiller::new();
        killer
            .conn_id
            .store(vars.connection_id, std::sync::atomic::Ordering::SeqCst);
        vars.set_status_flag(tidb_base::mysql::ServerStatusAutocommit, true);
        Session {
            core: SessionCore {
                sctx: SessCtx {
                    vars,
                    txn: LazyTxn::default(),
                    killer,
                    store,
                    oracle,
                    domain_is: Arc::new(tidb_infoschema::infoschema_v2::new_info_schema_v2(
                        None,
                        None,
                        tidb_infoschema::new_data(),
                    )),
                    values: HashMap::new(),
                    txn_end: false,
                    expr_ctx: tidb_sessionctx::exprstatic::new_expr_context(Vec::new()),
                    current_plan: None,
                    process: None,
                    start_time: None,
                    common_global_loaded: false,
                },
                txn_manager: new_txn_manager(),
            },
            running: None,
            next_seq: 0,
            executed_stmts: 0,
            dialect,
        }
    }

    /// The connection's dialect (ADR 0010 §4.4).
    // glue: ADR 0010 §4.4
    pub fn dialect(&self) -> Dialect {
        self.dialect
    }

    /// `GetSessionVars()`.
    // glue: accessor (Go session.GetSessionVars)
    pub fn vars(&self) -> &SessionVars {
        &self.core.sctx.vars
    }

    /// `GetSessionVars()`, for writing.
    // glue: accessor (Go session.GetSessionVars)
    pub fn vars_mut(&mut self) -> &mut SessionVars {
        &mut self.core.sctx.vars
    }

    /// The session as the executor framework and the transaction manager see it.
    // glue: accessor
    pub fn core(&self) -> &SessionCore {
        &self.core
    }

    /// The session as the executor framework and the transaction manager see it.
    // glue: accessor
    pub fn core_mut(&mut self) -> &mut SessionCore {
        &mut self.core
    }

    /// The part of the session the providers see.
    // glue: accessor
    pub fn sctx(&self) -> &SessCtx {
        &self.core.sctx
    }

    /// The SQL killer (KILL and `max_execution_time` signal it from other tasks).
    // glue: accessor (SessionVars.SQLKiller, N2 SessionShared)
    pub fn sql_killer(&self) -> Arc<SqlKiller> {
        Arc::clone(&self.core.sctx.killer)
    }

    /// The domain's InfoSchema (VS1-17 feeds it from the PoC domain's sync loop).
    // glue: stand-in for the domain's InfoSchema (D06.5, VS1-17)
    pub fn set_domain_info_schema(&mut self, is: InfoSchemaRef) {
        self.core.sctx.domain_is = is;
    }

    /// Whether the session is inside a transaction (ADR 0010 §4.1: state as a query; the
    /// MySQL adapter turns it into `SERVER_STATUS_IN_TRANS`).
    // glue: ADR 0010 §4.1 (reads SessionVars.InTxn)
    pub fn in_txn(&self) -> bool {
        self.core.sctx.vars.in_txn()
    }

    /// Whether autocommit is on.
    // glue: ADR 0010 §4.1 (reads SessionVars.IsAutocommit)
    pub fn is_autocommit(&self) -> bool {
        self.core.sctx.vars.is_autocommit()
    }

    /// The current database (`""` if none).
    // glue: ADR 0010 §4.1 (reads SessionVars.CurrentDB)
    pub fn current_db(&self) -> &str {
        &self.core.sctx.vars.current_db
    }

    /// `String()`: the session as JSON (Go marshals a map: keys in sorted order).
    // go: session.String L2 (the user's string comes with D04.1)
    pub fn string(&self) -> String {
        let vars = &self.core.sctx.vars;
        let mut data: Vec<(&str, String)> = vec![
            ("currDBName", format!("{:?}", vars.current_db)),
            ("id", vars.connection_id.to_string()),
            ("status", vars.status().to_string()),
            (
                "strictMode",
                tidb_base::mysql::SQLMode(vars.sql_mode)
                    .has_strict_mode()
                    .to_string(),
            ),
            ("user", "null".to_owned()),
        ];
        if self.core.sctx.txn.valid() {
            // if txn is committed or rolled back, txn is nil.
            data.push(("txn", format!("{:?}", self.core.sctx.txn.string())));
        }
        if vars.snapshot_ts != 0 {
            data.push(("snapshotTS", vars.snapshot_ts.to_string()));
        }
        if vars.stmt_ctx().last_insert_id() > 0 {
            data.push(("lastInsertID", vars.stmt_ctx().last_insert_id().to_string()));
        }
        if !vars.state.prepared_stmts.is_empty() {
            data.push((
                "preparedStmtCount",
                vars.state.prepared_stmts.len().to_string(),
            ));
        }
        data.sort_by(|a, b| a.0.cmp(b.0));
        let body: Vec<String> = data.iter().map(|(k, v)| format!("  {k:?}: {v}")).collect();
        format!("{{\n{}\n}}", body.join(",\n"))
    }

    /// `Close()`: the read-only node holds no table or advisory locks, no stats collector
    /// and no session plan cache yet; the transaction is rolled back.
    // go: session.Close L2 (table / advisory locks, stats and index usage collectors, bindings, plan cache: not on the read-only node or not ported)
    pub fn close(&mut self) {
        self.running = None;
        self.core.rollback_txn();
        self.core.sctx.vars.withdraw_all_prepared_stmt();
        // stmtStats.SetFinished: D14.2. ClearDiskFullOpt: write option. Trackers: A1.
    }

    /// ExecuteStmt executes a statement (Go `session.ExecuteStmt`, `session.go:2019`).
    ///
    /// `text` is `stmtNode.Text()` (the AST does not keep it). `Ok(None)` is Go's nil record
    /// set. A statement whose rows were not read to the end and closed is finished first
    /// (spec §2.3).
    // go: session.ExecuteStmt L2 (no Top SQL / plan replayer / optimizer debug trace / resource group hint check: D14.2, D13.x, N10; see spec §4)
    pub async fn execute_stmt(
        &mut self,
        stmt_node: Arc<ast::Stmt>,
        text: &str,
    ) -> Result<Option<ExecStmtResult>, Error> {
        self.finish_unfinished_stmt().await;

        self.core.prepare_txn_ctx().await?;
        self.core.sctx.load_common_global_variables_if_needed()?;

        self.core.sctx.start_time = Some(Instant::now());

        // Some executions are done in compile stage, so we reset them before compile.
        reset_context_of_stmt(&mut self.core, &stmt_node, text)?;
        // ExecuteStmt.BinaryArgs (EXECUTE): D13.1.

        // StmtCtx.SQLDigest: the normalized digest (parser.NormalizeDigest, D02.1).
        let digest = String::new();
        let cmd_byte = self.core.sctx.vars.command_value as u8;
        // Top SQL, plan replayer capture, optimizer debug trace: D14.2 / D13.x / D09.13.

        validate_statement_in_txn(&self.core.sctx.vars, &stmt_node)?;
        validate_statement_read_only_in_staleness(&self.core.sctx.vars, &stmt_node)?;

        // Uncorrelated subqueries will execute once when building plan, so we reset process
        // info before building plan.
        self.core.sctx.current_plan = None; // reset current plan
        self.core
            .set_process_info(text, SystemTime::now(), cmd_byte, 0);
        self.core.sctx.txn.on_stmt_start(&digest);
        // defer sessiontxn.GetTxnManager(s).OnStmtEnd(); defer s.txn.onStmtEnd(): a drop
        // guard (M-DEFER), so they also run when the statement's future is cancelled.
        let guard = OnStmtEnd(self);
        guard.0.execute_stmt_started(stmt_node).await
    }

    /// The statement `stmt` returns rows: it stays in the session under a new sequence
    /// number, which its result carries (spec §2.3).
    // glue: spec §2.3
    pub(crate) fn start_running(
        &mut self,
        stmt: ExecStmt,
        rs: tidb_executor::adapter::RecordSet,
        txn_start_ts: u64,
        wrapped: bool,
    ) -> ExecStmtResult {
        self.next_seq += 1;
        let seq = self.next_seq;
        let slot = Arc::new(parking_lot::Mutex::new(Some(rs)));
        self.running = Some(RunningStmt::new(
            stmt,
            txn_start_ts,
            wrapped,
            seq,
            Arc::clone(&slot),
        ));
        ExecStmtResult::new(slot, seq)
    }

    /// The number of statements whose executors were opened (VS1 plan §4.3: a fallback
    /// after that is a bug of the caller).
    // glue: VS1 plan §4.3 (router check)
    pub fn executed_stmts(&self) -> u64 {
        self.executed_stmts
    }

    /// The part of `ExecuteStmt` after the deferred `OnStmtEnd`s are registered.
    // go-part: session.ExecuteStmt
    async fn execute_stmt_started(
        &mut self,
        stmt_node: Arc<ast::Stmt>,
    ) -> Result<Option<ExecStmtResult>, Error> {
        self.core
            .on_txn_manager_stmt_start_or_retry(Arc::clone(&stmt_node))?;

        // failpoint mockStmtSlow: not ported.

        let stmt_label = crate::glue::get_stmt_label(&stmt_node)?;
        self.core.set_request_source(&stmt_label);

        // Transform abstract syntax tree to a physical plan(stored in executor.ExecStmt).
        let text = self.core.sctx.vars.stmt_ctx().original_sql.clone();
        let compiler = Compiler;
        let stmt = compiler.compile(&mut self.core, Arc::clone(&stmt_node), &text);
        // The resource group hint check (StmtCtx.ResourceGroupName): resource control, N10.
        let stmt = match stmt {
            Ok(stmt) => stmt,
            Err(err) => {
                self.core.rollback_on_error();

                // Only print log message when this SQL is from the user.
                // Mute the warning for internal SQLs.
                if !self.core.sctx.vars.in_restricted_sql
                    && !tidb_sessionctx::variable::error::ErrUnknownSystemVar.equal(&err)
                {
                    log::warn!("compile SQL failed: {err}, SQL={}", sql_for_log(&text));
                }
                return Err(err);
            }
        };

        let dur_compile = self
            .core
            .sctx
            .start_time
            .map_or(std::time::Duration::ZERO, |t| t.elapsed());
        self.core.sctx.vars.duration_compile = dur_compile.as_nanos() as i64;
        // SessionExecuteCompileDuration{Internal,General}.Observe: D14.1.
        self.core.sctx.current_plan = stmt.plan.clone();
        // ExecuteStmt without a name (binary protocol): EXECUTE is D13.1.

        // Execute the physical plan.
        log_stmt(&stmt, &self.core);

        let r = if stmt.ps_stmt.is_some() {
            // point plan short path
            let mut stmt = stmt;
            self.executed_stmts += 1;
            let r = stmt.point_get(&mut self.core).await;
            self.core.sctx.txn.change_to_invalid();
            match r {
                Ok(rs) => {
                    let txn_start_ts = rs.txn_start_ts();
                    Ok(Some(self.start_running(stmt, rs, txn_start_ts, false)))
                }
                Err(e) => Err(e),
            }
        } else {
            run_stmt(self, stmt).await
        };

        // ResourceGroupQueryTotalCounter: resource control (N10).

        if let Err(err) = &r
            && !err_is_noisy(err)
        {
            log::warn!(
                "run statement failed: schemaVersion={} {err} session={}",
                tidb_sessionctx::expropt::MetaOnlyInfoSchema::schema_meta_version(
                    &*self.core.sctx.get_info_schema()
                ),
                self.string()
            );
        }
        // Telemetry: not ported.
        r
    }

    /// Finishes the statement whose record set was dropped before `close` (spec §2.3): the
    /// session side of Go's `execStmtResult.Close` (`finishStmt`, then
    /// `ExecStmt.CloseRecordSet`). Called before the next statement and by the router
    /// before it takes a checkpoint.
    // glue: spec §2.3 (Go's deferred rs.Close of the protocol layer)
    pub async fn finish_unfinished_stmt(&mut self) {
        let Some(run) = self.running.as_ref() else {
            return;
        };
        // A result kept but not closed: its record set is still in the slot, so this is
        // Go's `execStmtResult.Close` (the executors are closed too).
        let rs = run.rs.lock().take();
        if let Some(mut rs) = rs {
            if let Err(e) = crate::exec_result::close_running(self, &mut rs).await {
                log::warn!("finish an unfinished statement: {e}");
            }
            return;
        }
        // The result was dropped: the session side of `Close` only (spec §2.3, §4).
        let Some(mut run) = self.running.take() else {
            return;
        };
        if run.wrapped && !run.finished {
            run.finished = true;
            if let Err(e) = finish_stmt(&mut self.core, None, &run.stmt).await {
                log::warn!("finish an unfinished statement: {e}");
            }
        }
        let last_err = crate::exec_result::join_errors(&run.last_errs);
        run.stmt
            .close_record_set(&mut self.core, run.txn_start_ts, last_err);
    }

    /// Whether a statement's record set is still open (or was dropped without `close`).
    // glue: spec §2.3
    pub fn has_unfinished_stmt(&self) -> bool {
        self.running.is_some()
    }

    /// PrepareStmt is used for executing prepare statement in binary protocol.
    ///
    /// The statement itself is prepared by `executor.NewPrepareExec` (D12.12), which builds
    /// the plan-cache statement (`PlanCacheStmt`, D13.1): both are the next work item (VS1-15,
    /// prepared-statement planning) and a boundary stub here, so every PREPARE on the new
    /// session ends `Blocked` after the transaction work below; the router then restores the
    /// session (spec §6.2) and the PoC owns the statement.
    // go: session.PrepareStmt L2 (StmtCtx.DetachMemDiskTracker: no session memory tracker, A1; the dedup-cache hit and NewPrepareExec are stubs for VS1-15)
    pub async fn prepare_stmt(&mut self, sql: &str) -> Result<PreparedStmtInfo, Error> {
        let core = &mut self.core;
        if core.sctx.vars.state.txn_ctx.info_schema.is_none() {
            // We don't need to create a transaction for prepare statement, just get
            // information schema will do.
            core.sctx.vars.state.txn_ctx.info_schema =
                Some(TxnInfoSchema(core.sctx.get_domain_info_schema()));
        }
        core.sctx.load_common_global_variables_if_needed()?;

        // NewPrepareExec may need startTS to build the executor, for example prepare
        // statement has subquery in int. So we have to call PrepareTxnCtx here.
        core.prepare_txn_ctx().await?;

        core.on_txn_manager_stmt_start_or_retry(Arc::new(prepare_stmt_node()))?;

        core.txn_manager.advise_warmup(&mut core.sctx)?;

        let mut dedup_key = Vec::new();
        if core.sctx.vars.enable_cache_prepare_stmt {
            // Session-level prepare dedup cache: if the same SQL text has been prepared
            // before in this session (with the same charset/collation/currentDB), reuse
            // the already-built PlanCacheStmt and skip the expensive Preprocess+Build.
            let vars = &mut core.sctx.vars;
            let (charset, collation) = vars.get_charset_info();
            dedup_key = prepare_dedup_cache_key(
                sql.as_bytes(),
                &charset,
                &collation,
                &vars.current_db,
                tidb_base::mysql::SQLMode(vars.sql_mode),
            );
            if vars.get_prepare_stmt_dedup_cache(&dedup_key).is_some() {
                // The entry is a `PrepareStmtCacheEntry` and `rebuildFromPrepareCache`
                // rebuilds a `PlanCacheStmt` from it (D13.1). Only a successful
                // NewPrepareExec fills the cache, so this is not reached before VS1-15.
                core.rollback_on_error();
                return Err(blocked(
                    "D13.1",
                    "the prepare dedup cache (PlanCacheStmt, prepared-statement planning)",
                ));
            }
        }

        let prepare_exec = new_prepare_exec(sql);
        // Rollback even if err is nil.
        core.rollback_on_error();
        let prepared = prepare_exec?;

        // Store the result in the dedup cache for future Prepares of the same SQL: the
        // entry holds the PlanCacheStmt (D13.1), with NewPrepareExec above.
        let _ = dedup_key;
        Ok(prepared)
    }

    /// ExecutePreparedStmt executes a prepared statement.
    ///
    /// Go turns the statement into an `ast.ExecuteStmt` holding its `PlanCacheStmt` and runs
    /// it through `ExecuteStmt`; neither the plan-cache statement (D13.1) nor `EXECUTE` in
    /// the Rust AST exists before VS1-15 (prepared-statement planning), so after the lookup
    /// this is a boundary stub. No statement can be found yet anyway: `PrepareStmt` never
    /// adds one before VS1-15.
    // go: session.ExecutePreparedStmt L2 (PlanCacheStmt / ast.ExecuteStmt are stubs for VS1-15)
    pub async fn execute_prepared_stmt(
        &mut self,
        stmt_id: u32,
        params: Vec<Datum>,
    ) -> Result<Option<ExecStmtResult>, Error> {
        if self
            .core
            .sctx
            .vars
            .get_prepared_stmt_by_id(stmt_id)
            .is_err()
        {
            log::error!("prepared statement not found, stmtID: {stmt_id}");
            return Err(ErrStmtNotFound.clone());
        }
        // stmt, ok := prepStmt.(*plannercore.PlanCacheStmt); execStmt := &ast.ExecuteStmt{
        // BinaryArgs: params, PrepStmt: stmt, PrepStmtId: stmtID}; s.ExecuteStmt(ctx, execStmt)
        let _ = params;
        Err(blocked(
            "D13.1",
            "executing a prepared statement (PlanCacheStmt, prepared-statement planning)",
        ))
    }

    /// DropPreparedStmt (COM_STMT_CLOSE).
    ///
    /// Go only records the id in `RetryInfo.DroppedPreparedStmtIDs`; `cleanRetryInfo` removes
    /// the statement when the transaction ends, so that a retried optimistic transaction can
    /// still execute it. Retrying is the write path (never on the read-only node, N11 D8), so
    /// the statement is removed at once (spec §6.1). `cleanRetryInfo` also
    /// deletes the statement's plan from the session plan cache: the cache is never built
    /// before VS1-15 ([`Session::get_session_plan_cache`]), so there is nothing to delete.
    // go: session.DropPreparedStmt L2 (removed at once: RetryInfo is the write path)
    pub fn drop_prepared_stmt(&mut self, stmt_id: u32) -> Result<(), Error> {
        let vars = &mut self.core.sctx.vars;
        if !vars.state.prepared_stmts.contains_key(&stmt_id) {
            return Err(ErrStmtNotFound.clone());
        }
        // cleanRetryInfo: GetSessionPlanCache().Delete(NewPlanCacheKey(...)): no cache yet.
        vars.remove_prepared_stmt(stmt_id);
        Ok(())
    }

    /// GetSessionPlanCache: the session's prepared plan cache, `None` when both plan caches
    /// are off. The cache (`plannercore.NewLRUPlanCache`, D13.1) is built lazily by the first
    /// plan-cache lookup, which is VS1-15 (prepared-statement planning): until then building
    /// it is a boundary stub, and nothing on the new stack asks for it.
    // go: session.GetSessionPlanCache L2 (the LRU plan cache is a stub for VS1-15)
    pub fn get_session_plan_cache(&mut self) -> Result<Option<()>, Error> {
        let vars = &self.core.sctx.vars;
        // use the prepared plan cache
        if !vars.enable_prepared_plan_cache && !vars.enable_non_prepared_plan_cache {
            return Ok(None);
        }
        Err(blocked(
            "D13.1",
            "the session plan cache (prepared-statement planning)",
        ))
    }
}

/// The node `PrepareStmt` starts the transaction manager's statement with (Go
/// `&ast.PrepareStmt{SQLText: sql}`): `PREPARE` is not in the Rust AST, so it is the
/// grammar's unported production. The providers only ask whether a node is a `SELECT` or a
/// transaction statement, which it is not.
// glue: ast.PrepareStmt (not in the Rust AST)
fn prepare_stmt_node() -> ast::Stmt {
    ast::Stmt::Unported(Box::new(tidb_parser::Node {
        kind: "PreparedStmt#0",
        span: tidb_parser::Span::default(),
        children: Vec::new(),
    }))
}

/// `executor.NewPrepareExec(s, sql)` followed by `Next`: parses the statement, checks it,
/// builds the `PlanCacheStmt` (D13.1) and adds it to the session. The next work item
/// (VS1-15, prepared-statement planning) ports it.
// glue: stub, blocked-by:D12.12 (executor.NewPrepareExec: the prepare executor and the plan-cache statement come with prepared-statement planning)
fn new_prepare_exec(sql: &str) -> Result<PreparedStmtInfo, Error> {
    let _ = sql;
    Err(blocked(
        "D12.12",
        "preparing a statement (PrepareExec, prepared-statement planning)",
    ))
}

/// resolvePreparedStmt: the statement an `EXECUTE` runs. `EXECUTE` is not in the Rust AST
/// (Go reads it with `plannercore.GetPreparedStmt`, D13.1, VS1-15), so every statement
/// resolves to itself. Go's only caller (`shouldCheckConnectionAliveBeforeCommit`) asks
/// whether a statement writes; it is not ported (the write path).
// go: resolvePreparedStmt L2 (no EXECUTE in the Rust AST)
pub fn resolve_prepared_stmt<'a>(stmt: &'a ast::Stmt, vars: &SessionVars) -> &'a ast::Stmt {
    let _ = vars;
    stmt
}

/// The deferred `OnStmtEnd`s of `ExecuteStmt` (Go: `defer s.txn.onStmtEnd()`, `defer
/// sessiontxn.GetTxnManager(s).OnStmtEnd()`, run in that order).
struct OnStmtEnd<'a>(&'a mut Session);

impl Drop for OnStmtEnd<'_> {
    // glue: M-DEFER
    fn drop(&mut self) {
        self.0.core.sctx.txn.on_stmt_end();
        self.0.core.txn_manager.on_stmt_end();
    }
}

/// `validateStatementInTxn`: only `IMPORT INTO` in a transaction is refused; it is not in the
/// Rust AST.
// glue: stub of session.validateStatementInTxn, blocked-by:D02.2 (ast.ImportIntoStmt: nothing to refuse)
fn validate_statement_in_txn(vars: &SessionVars, node: &ast::Stmt) -> Result<(), Error> {
    let _ = (vars, node);
    Ok(())
}

/// `validateStatementReadOnlyInStaleness`: only reached in a stale-read transaction or with
/// `tx_read_ts` / external ts, which the VS1 router keeps on the PoC; a SELECT there needs
/// `planner.IsReadOnly` (D02.2).
// glue: stub of session.validateStatementReadOnlyInStaleness, blocked-by:D05.4 (stale read transactions, part 2)
fn validate_statement_read_only_in_staleness(
    vars: &SessionVars,
    node: &ast::Stmt,
) -> Result<(), Error> {
    let peak = vars
        .state
        .txn_read_ts
        .as_ref()
        .map_or(0, |t| t.peak_txn_read_ts());
    if (!vars.state.txn_ctx.is_staleness && peak == 0 && !vars.enable_external_ts_read)
        || vars.in_restricted_sql
    {
        return Ok(());
    }
    let _ = node;
    Err(blocked("D05.4", "statements in stale read transactions"))
}

/// `runStmt`: executes the compiled statement; a statement with rows stays in the session
/// until its result is closed (spec §2.3).
// go: runStmt L2 (no DDL / pipelined DML / EXPLAIN ANALYZE DML / LOAD DATA branches: the new stack runs SELECT)
pub(crate) async fn run_stmt(
    se: &mut Session,
    mut s: ExecStmt,
) -> Result<Option<ExecStmtResult>, Error> {
    // failpoint assertTxnManagerInRunStmt, tracing: not ported.
    se.core
        .sctx
        .set_value(QueryString, Box::new(s.origin_text().to_owned()));
    // The new stack runs no DDL (ast.DDLNode).
    se.core.sctx.clear_value(LastExecuteDDL);

    // Save origTxnCtx here to avoid it reset in the transaction retry.
    let orig_txn_start_ts = se.core.sctx.vars.state.txn_ctx.start_ts;
    // checkTxnAborted: TxnCtx.LockExpire is set by pessimistic locks (write path): never.
    if se.core.sctx.vars.state.txn_ctx.could_retry && !s.is_read_only()? {
        // Only when the txn is could retry and the statement is not read only, need to do
        // stmt-count-limit check.
        crate::tidb::check_stmt_limit(&mut se.core, false).await?;
    }

    se.executed_stmts += 1;
    let r = s.exec(&mut se.core).await;

    // Pipelined DML errors: the write path.
    // se.updateTelemetryMetric: telemetry, not ported.
    se.core.sctx.vars.state.txn_ctx.statement_count += 1;
    if let Ok(Some(rs)) = r {
        // StmtCtx.IsExplainAnalyzeDML: EXPLAIN ANALYZE of DML (write path).
        let txn_start_ts = rs.txn_start_ts();
        return Ok(Some(se.start_running(s, rs, txn_start_ts, true)));
    }

    let meets_err = r.err();
    let err = finish_stmt(&mut se.core, meets_err, &s).await;
    // hasFileTransInConn: LOAD DATA (write path). If it is not a select statement or special
    // query, we record its slow log here, then it could include the transaction commit time.
    s.finish_execute_stmt(&mut se.core, orig_txn_start_ts, err.as_ref().err(), false);
    err.map(|()| None)
}
