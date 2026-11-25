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
use tidb_executor::prepared::new_prepare_exec;
use tidb_executor::select::reset_context_of_stmt;
use tidb_kv::kv::{Client, Storage};
use tidb_memory::sqlkiller::SqlKiller;
use tidb_parser::ast;
use tidb_planner::core::plan_cache_utils::{
    PlanCacheStmt, PlanCacheStmtRef, get_prepared_stmt, session_plan_cache_key,
};
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
#[derive(Clone, Debug, PartialEq)]
pub struct PreparedStmtInfo {
    /// The session's id of the statement (Go `SessionVars.GetNextPreparedStmtID`), not the
    /// protocol's: the adapter maps between them (ADR 0010 §4.1).
    pub stmt_id: u32,
    /// Number of `?` placeholders.
    pub param_count: usize,
    /// The result columns (`PrepareExec.Fields`).
    pub fields: Vec<tidb_executor::glue::ResultField>,
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
    /// Whether the store is a wired one (`with_store`): the stand-in [`UnwiredStore`] has no
    /// client (the store driver's, D07.1), so the planning context gets none.
    pub(crate) store_wired: bool,
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
    /// `GetPlanCtx().GetReadonlyUserVarMap()` (Go `PlanCtxExtended.readonlyUserVars`): the
    /// user variables the statement's preprocessor found read-only; `None` is Go's nil map.
    readonly_user_vars: Option<std::collections::HashSet<String>>,
    /// `currentPlan`.
    pub(crate) current_plan: Option<Plan>,
    /// `SetProcessInfo` (spec §1.2).
    pub(crate) process: Option<ProcessArgs>,
    /// `sessionVars.StartTime`.
    pub(crate) start_time: Option<Instant>,
    /// `sessionVars.CommonGlobalLoaded`.
    common_global_loaded: bool,
    /// `sessionPlanCache`: built by the first `GetSessionPlanCache`.
    pub(crate) plan_cache: Option<SessionPlanCache>,
    /// `domain.GetDomain(s).GetInstancePlanCache()`: the instance-level plan cache shared by
    /// every session (the plan-cache exception A3); `None` without a domain.
    pub(crate) instance_plan_cache: Option<Arc<dyn tidb_sessionctx::context::InstancePlanCache>>,
    /// The `ExecPool` the connection runs on, which the statement's executors start their
    /// tasks on (N3-A1 §3.1 items 3 and 5); `None` until the server sets it.
    pub(crate) exec_pool: Option<tidb_runtime::pool::ExecPool>,
}

/// `sessionctx.SessionPlanCache` (`plannercore.NewLRUPlanCache`): the cache is the plan-cache
/// exception A3, whose spec is not confirmed, so this stand-in holds no plan (every lookup of
/// the miss path misses, D13.1 `core::plan_cache`). It keeps the keys `Delete` was called
/// with, so the deletions of `cleanRetryInfo` can be checked against Go's (spec §6).
#[derive(Debug, Default)]
pub struct SessionPlanCache {
    /// The keys passed to `Delete`, in order.
    pub deleted_keys: Vec<String>,
}

impl SessionPlanCache {
    /// `Delete(key)`: there is no plan to delete; the key is recorded.
    // glue: stub of LRUPlanCache.Delete, blocked-by:D13.1 (the cache is the plan-cache exception A3: it holds no plans)
    pub fn delete(&mut self, key: &str) {
        self.deleted_keys.push(key.to_owned());
    }
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

/// Loads the tables `stmt` names into the table cache of the session's InfoSchema
/// (`InfoSchema.TableByName`, which refills the cache), so that the synchronous planner
/// finds them there (D06.3 spec §11.2). Errors are left to the planner's own lookup.
// glue: the table loads before planning (D06.3 spec §11.2)
fn load_stmt_tables(
    sctx: &SessCtx,
    stmt: &ast::Stmt,
) -> impl std::future::Future<Output = ()> + Send + use<> {
    let names: Vec<(String, String)> =
        tidb_planner::core::plan_cache_utils::statement_table_names(stmt)
            .into_iter()
            .filter_map(|(schema, table)| {
                let schema = if schema.is_empty() {
                    sctx.vars.current_db.clone()
                } else {
                    schema
                };
                (!schema.is_empty()).then_some((schema, table))
            })
            .collect();
    let is = sctx.get_info_schema();
    load_tables(is, names)
}

// glue: the table loads before planning (D06.3 spec §11.2)
async fn load_tables(is: InfoSchemaRef, names: Vec<(String, String)>) {
    for (schema, table) in names {
        let _ = is
            .table_by_name(
                tidb_infoschema::infoschema_v2::Ctx::background(),
                &tidb_parser::model::CIStr::new(&schema),
                &tidb_parser::model::CIStr::new(&table),
            )
            .await;
    }
}

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

    /// The session's expression context (`sessionexpr.NewExprContext(s)`) over a view of the
    /// session as it is now (spec §8).
    // glue: N2 (sessionexpr over a view of the session, built when the context is handed out)
    pub fn expr_ctx(&self) -> Arc<crate::sessionexpr::ExprContext> {
        let readonly = self.readonly_user_vars.clone().unwrap_or_default();
        crate::sessionexpr::new_expr_context(crate::sessionexpr::SessionView::new(
            &self.vars,
            &readonly,
            Arc::clone(&self.store),
            self.get_info_schema(),
            self.get_domain_info_schema(),
        ))
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
        self.clean_retry_info();
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
        self.rollback_txn_state_in(false);
    }

    /// `RollbackTxn`; `in_close_session`: Go's `ctx.Value(inCloseSession{}) != nil` (from
    /// `Close`), where the closed prepared statements are not cleaned (`Close` withdraws all
    /// of them).
    // go-part: session.RollbackTxn
    fn rollback_txn_state_in(&mut self, in_close_session: bool) {
        if self.txn.valid()
            && let Err(e) = self.txn.rollback()
        {
            log::error!("{e}");
        }
        if !in_close_session {
            self.clean_retry_info();
        }
        self.txn.change_to_invalid();
        self.vars.state.txn_ctx.cleanup();
        self.vars.cleanup_txn_read_ts_if_used();
        self.vars.set_in_txn(false);
        // metrics.DDLRollbackTempIndexWrite: DDL (delegated).
    }

    /// cleanRetryInfo: removes the prepared statements closed in the transaction (Go keeps
    /// them executable until the transaction ends, `DropPreparedStmt`), and their plans from
    /// the session plan cache when `tidb_enable_prepared_plan_cache` is on. Called from
    /// `doCommitWithRetry` (commit, and the end of an autocommit statement through
    /// `finishStmt` → `CommitTxn`) and `RollbackTxn` (not when the session closes), as Go.
    ///
    /// As Go v8.5.8 (D13.1 tasks, possibly an upstream bug): the cache key is computed from
    /// the first closed statement only, and that same statement is used again for every
    /// later one, so closing several statements in one transaction deletes the first one's
    /// plan each time. The cache holds no plans (`SessionPlanCache`), the keys are recorded.
    // go: session.cleanRetryInfo L1
    fn clean_retry_info(&mut self) {
        if self.vars.state.retry_info.retrying {
            return;
        }

        // defer retryInfo.Clean()
        let dropped = std::mem::take(&mut self.vars.state.retry_info.dropped_prepared_stmt_ids);
        self.vars.state.retry_info.clean();
        if dropped.is_empty() {
            return;
        }

        let plan_cache_enabled = self.vars.enable_prepared_plan_cache;
        let mut cache_key = String::new();
        let mut prepared_obj: Option<PlanCacheStmtRef> = None;
        if plan_cache_enabled {
            let first_stmt_id = dropped[0];
            if let Some(prepared_pointer) = self.vars.state.prepared_stmts.get(&first_stmt_id) {
                prepared_obj = Arc::clone(prepared_pointer)
                    .downcast::<parking_lot::Mutex<PlanCacheStmt>>()
                    .ok();
                if let Some(obj) = &prepared_obj {
                    match session_plan_cache_key(&obj.lock()) {
                        Ok((key, ..)) => cache_key = key,
                        Err(e) => {
                            log::warn!("clean cached plan failed: {e}");
                            return;
                        }
                    }
                }
            }
        }
        for (i, stmt_id) in dropped.into_iter().enumerate() {
            if plan_cache_enabled {
                if i > 0
                    && let Some(obj) = &prepared_obj
                {
                    match session_plan_cache_key(&obj.lock()) {
                        Ok((key, ..)) => cache_key = key,
                        Err(e) => {
                            log::warn!("clean cached plan failed: {e}");
                            return;
                        }
                    }
                }
                if !self.vars.ignore_prepared_cache_close_stmt {
                    // keep the plan in cache
                    self.get_session_plan_cache().delete(&cache_key);
                }
            }
            self.vars.remove_prepared_stmt(stmt_id);
        }
    }

    /// `GetSessionPlanCache()` for a caller that has checked a plan cache is on: the cache,
    /// built by the first call (`plannercore.NewLRUPlanCache`, a stand-in, see
    /// [`SessionPlanCache`]).
    // glue: the lazily built cache of session.GetSessionPlanCache
    pub(crate) fn get_session_plan_cache(&mut self) -> &mut SessionPlanCache {
        self.plan_cache
            .get_or_insert_with(SessionPlanCache::default)
    }

    /// Whether a transaction ended inside a provider call, and clears it.
    // glue: txnmanager.rs module doc
    pub(crate) fn take_txn_end(&mut self) -> Option<()> {
        std::mem::take(&mut self.txn_end).then_some(())
    }

    /// `loadCommonGlobalVariablesIfNeeded`: copies the global values of the session
    /// variables into a session that has not set them, running each variable's SetSession
    /// hook (so e.g. `tidb_enable_prepared_plan_cache` is on as in Go). The bootstrap
    /// branch (`sessionctx.Initing`) does not apply: the new session never bootstraps.
    /// The session cache is [`session_cache`]'s stand-in for the domain's.
    // go: session.loadCommonGlobalVariablesIfNeeded L2 (domain.GetSessionCache is a stand-in: the domain's sysvar cache is D06.5)
    #[allow(clippy::collapsible_if)] // Go's nested ifs
    fn load_common_global_variables_if_needed(&mut self) -> Result<(), Error> {
        let vars = &mut self.vars;
        if self.common_global_loaded {
            return Ok(());
        }
        self.common_global_loaded = true;

        // Deep copy sessionvar cache
        let session_cache = session_cache(vars)?;
        for (var_name, var_val) in &session_cache {
            if !vars.get_system_var(var_name).1 {
                if let Err(err) = vars.set_system_var_with_relaxed_validation(var_name, var_val) {
                    if tidb_sessionctx::variable::error::ErrUnknownSystemVar.equal(&err) {
                        continue; // sessionCache is stale; sysvar has likely been unregistered
                    }
                    return Err(err);
                }
            }
        }
        // when client set Capability Flags CLIENT_INTERACTIVE, init wait_timeout with interactive_timeout
        //
        // Not reachable on the new stack yet: nothing passes the handshake's capabilities to
        // the new session (`set_client_capability` has no caller; the PoC's connection keeps
        // them), so `client_capability` is 0 and `wait_timeout` keeps its global value.
        if vars.client_capability & tidb_base::mysql::ClientInteractive > 0 {
            // variable.InteractiveTimeout / variable.WaitTimeout
            let (var_val, ok) = vars.get_system_var("interactive_timeout");
            if ok {
                vars.set_system_var("wait_timeout", &var_val)?;
            }
        }
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

/// `domain.GetDomain(s).GetSessionCache()`: the session values of every variable that is
/// not skipped at init, as the domain's `rebuildSysVarCache` builds them from
/// `mysql.global_variables` (a variable without a row keeps its default). The domain's
/// sysvar cache is D06.5; here a global variable's value comes from the session's
/// `GlobalVarAccessor` (the PoC router's stand-in answers the compiled defaults, the PoC
/// reads no `mysql.global_variables` either), and a session-only variable has its default.
// glue: stand-in for Domain.GetSessionCache (the domain's sysvar cache, D06.5)
fn session_cache(vars: &SessionVars) -> Result<HashMap<String, String>, Error> {
    let mut cache = HashMap::new();
    for sv in tidb_sessionctx::variable::variable::get_sys_vars() {
        if sv.skip_init() {
            continue;
        }
        let val = if sv.has_global_scope() {
            vars.global_vars_accessor.get_global_sys_var(sv.name)?
        } else {
            sv.value.clone()
        };
        cache.insert(sv.name.to_owned(), val);
    }
    Ok(cache)
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
    fn on_txn_manager_stmt_start_or_retry(&mut self, node: &ast::Stmt) -> Result<(), Error> {
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
/// yet provide (spec §1.2). The planning context is not among them: the planner gets a
/// `PlanCtx` (N2) built from `SessionContext::get_plan_ctx`.
impl SessionCore {
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
    // glue: domain.GetDomain(sctx).InfoSchema() (the session's domain InfoSchema)
    fn get_domain_info_schema(&self) -> Option<InfoSchemaRef> {
        Some(self.sctx.get_domain_info_schema())
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
    // glue: interface method sessionctx.Context.SetValue with the key sessionctx.QueryString
    fn set_query_string(&mut self, text: String) {
        self.sctx.set_value(QueryString, Box::new(text));
    }
    // glue: sessiontxn.AdviseOptimizeWithPlanAndThenWarmUp (D05.4)
    fn advise_optimize_with_plan_and_then_warm_up(&mut self, plan: &Plan) -> Result<(), Error> {
        let info = PhysicalPlanInfo::new(plan.as_dyn());
        tidb_sessiontxn::advise_optimize_with_plan_and_then_warm_up(self, Some(&info))
    }
    /// `GetExprCtx()`: the session's `sessionexpr.ExprContext`, over a view of the session
    /// taken now (spec §8: Go keeps one context that reads the session on every call).
    // go: session.GetExprCtx L2 (a context over a view of the session per call, N2)
    fn get_expr_ctx(&self) -> tidb_sessionctx::exprctx::ExprContextRef {
        self.sctx.expr_ctx() as tidb_sessionctx::exprctx::ExprContextRef
    }
    // go: pkg/planner/plannersession/context.go PlanCtxExtended.SetReadonlyUserVarMap L1
    fn set_readonly_user_var_map(&mut self, readonly_user_vars: std::collections::HashSet<String>) {
        self.sctx.readonly_user_vars = Some(readonly_user_vars);
    }
    // go: pkg/planner/plannersession/context.go PlanCtxExtended.Reset L1
    fn reset_plan_ctx(&mut self) {
        self.sctx.readonly_user_vars = None;
    }
    /// `GetPlanCtx()`: the expression context ([`SessionContext::get_expr_ctx`]), the
    /// store client when the session runs over a wired store (`None` over the stand-in
    /// store: the push down checks are then a boundary of the store driver, D07.1) and
    /// `HasDirtyContent`: Go looks for the table's keys in the transaction's membuffer; a
    /// read-only (or no, or pipelined) transaction has none, so the answer is false for every
    /// table. A transaction that wrote is not on the new stack (writes run on the PoC) and
    /// the Rust transaction has no membuffer yet, so the check is then unknown (`None`, a
    /// boundary of the plan builder).
    // go: session.HasDirtyContent L2 (only the no-write answer: the membuffer is not in the Rust transaction)
    fn get_plan_ctx(&self) -> tidb_planner::planctx::SessionPlanCtx {
        let txn = &self.sctx.txn;
        let no_dirty_content =
            !txn.valid() || tidb_sessiontxn::sctx::LazyTxn::is_pipelined(txn) || txn.is_read_only();
        let has_dirty_content: Option<tidb_planner::planctx::HasDirtyContentFn> =
            no_dirty_content.then(|| Arc::new(|_tid: i64| false) as _);
        tidb_planner::planctx::SessionPlanCtx {
            expr_ctx: self.sctx.expr_ctx() as tidb_sessionctx::exprctx::ExprContextRef,
            client: self.sctx.store_wired.then(|| self.sctx.store.get_client()),
            has_dirty_content,
            advise_txn_warmup: None,
            domain_info_schema: Some(self.sctx.get_domain_info_schema() as _),
            instance_plan_cache: self.sctx.instance_plan_cache.clone(),
        }
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
    // glue: N3 ExecPool handle of the session (N3-A1 §3.1)
    fn exec_pool(&self) -> Option<tidb_runtime::pool::ExecPool> {
        self.sctx.exec_pool.clone()
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
        let node = self.txn_manager.get_current_stmt().map(Arc::clone);
        let r = self
            .txn_manager
            .on_stmt_start(&mut self.sctx, node.as_deref());
        // The statement stays the current one (Go passes the node it already holds).
        self.txn_manager.set_current_stmt(node);
        r
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

// go: isEndTxnStmt L1 [M-ERRVAL]
pub fn is_end_txn_stmt(stmt: &ast::Stmt, vars: &SessionVars) -> Result<bool, Error> {
    match stmt {
        ast::Stmt::Rollback(_) | ast::Stmt::Commit(_) => Ok(true),
        ast::Stmt::Execute(n) => {
            let ps = get_prepared_stmt(n, vars)?;
            let ps = ps.lock();
            is_end_txn_stmt(&ps.prepared_ast.stmt, vars)
        }
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
                &String::from_utf8_lossy(&vars.state.plan_cache_params.lock().string()),
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
        Session::build(
            dialect,
            global,
            Arc::<UnwiredStore>::clone(&store),
            store,
            false,
        )
    }

    /// A session over `store` and its TSO.
    // glue: stand-in for createSession (D05.3)
    pub fn with_store(
        dialect: Dialect,
        global: Box<dyn GlobalVarAccessor>,
        store: Arc<dyn Storage>,
        oracle: Arc<dyn Oracle>,
    ) -> Session {
        Session::build(dialect, global, store, oracle, true)
    }

    /// The session over `store` / `oracle`; `store_wired` tells whether the store has a client.
    // glue: stand-in for createSession (D05.3)
    fn build(
        dialect: Dialect,
        global: Box<dyn GlobalVarAccessor>,
        store: Arc<dyn Storage>,
        oracle: Arc<dyn Oracle>,
        store_wired: bool,
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
                    store_wired,
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
                    readonly_user_vars: None,
                    current_plan: None,
                    process: None,
                    start_time: None,
                    common_global_loaded: false,
                    plan_cache: None,
                    instance_plan_cache: None,
                    exec_pool: None,
                },
                txn_manager: new_txn_manager(),
            },
            running: None,
            next_seq: 0,
            executed_stmts: 0,
            dialect,
        }
    }

    /// Sets the `ExecPool` the connection runs on (N3-A1 §3.1 items 3 and 5): the
    /// statement's executors start their tasks on it (`ExecCtx.pool`), so that their
    /// time-sliced checkpoints yield and their stalls are detected. `None`: the tasks go to
    /// the current tokio runtime, uninstrumented.
    // glue: N3 ExecPool handle of the session (N3-A1 §3.1)
    pub fn set_exec_pool(&mut self, pool: Option<tidb_runtime::pool::ExecPool>) {
        self.core.sctx.exec_pool = pool;
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

    /// `loadCommonGlobalVariablesIfNeeded` for a caller that opens the session (the VS1
    /// router loads the global values when the connection opens, so that its step-0 check
    /// compares with the values Go's session has after its first statement).
    // glue: session.loadCommonGlobalVariablesIfNeeded for the router
    pub fn load_common_global_variables_if_needed(&mut self) -> Result<(), Error> {
        self.core.sctx.load_common_global_variables_if_needed()
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

    /// Hands the session the domain's instance plan cache (`domain.GetDomain(s)`'s, the
    /// plan-cache exception A3), shared by every session of the process.
    // glue: domain.GetDomain(s).GetInstancePlanCache() (the session holds no domain)
    pub fn set_instance_plan_cache(
        &mut self,
        cache: Option<Arc<dyn tidb_sessionctx::context::InstancePlanCache>>,
    ) {
        self.core.sctx.instance_plan_cache = cache;
    }

    /// `ADMIN FLUSH <scope> PLAN_CACHE` (Go: a `SimpleExec` statement): the statement context
    /// is reset as for every statement (`ResetContextOfStmt`), then
    /// `executeAdminFlushPlanCache` runs on the session's variables and the instance plan
    /// cache (the plan-cache exception A3, decision 8). The parser has no ADMIN AST yet
    /// (D02.2): the caller passes the scope it read from the statement.
    // glue: the SimpleExec of ADMIN FLUSH ... PLAN_CACHE (the ADMIN AST is D02.2)
    pub fn admin_flush_plan_cache(
        &mut self,
        stmt: &ast::Stmt,
        sql: &str,
        scope: tidb_executor::simple::StatementScope,
    ) -> Result<(), Error> {
        tidb_executor::select::reset_context_of_stmt(&mut self.core, stmt, sql)?;
        let cache = self.core.sctx.instance_plan_cache.clone();
        tidb_executor::simple::execute_admin_flush_plan_cache(
            &mut self.core.sctx.vars,
            scope,
            cache.as_deref(),
        )
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
        // RollbackTxn(context.WithValue(ctx, inCloseSession{}, struct{}{})).
        self.core.sctx.rollback_txn_state_in(true);
        self.core.txn_manager.on_txn_end(&self.core.sctx);
        self.core.sctx.vars.withdraw_all_prepared_stmt();
        // stmtStats.SetFinished: D14.2. ClearDiskFullOpt: write option. Trackers: A1.
    }

    /// ExecuteStmt executes a statement (Go `session.ExecuteStmt`, `session.go:2019`).
    ///
    /// `text` is `stmtNode.Text()` (the AST does not keep it). The statement is handed over:
    /// the steps before `Compile` borrow it, `Compile` takes it and freezes it into an `Arc`
    /// after planning (spec §9.2). `Ok(None)` is Go's nil record set. A statement whose rows were not read to the end and closed is finished first
    /// (spec §2.3).
    // go: session.ExecuteStmt L2 (no Top SQL / plan replayer / optimizer debug trace / resource group hint check: D14.2, D13.x, N10; see spec §4)
    pub async fn execute_stmt(
        &mut self,
        stmt_node: ast::Stmt,
        text: &str,
    ) -> Result<Option<ExecStmtResult>, Error> {
        self.finish_unfinished_stmt().await;

        self.core.prepare_txn_ctx().await?;
        self.core.sctx.load_common_global_variables_if_needed()?;

        self.core.sctx.start_time = Some(Instant::now());

        // Some executions are done in compile stage, so we reset them before compile.
        reset_context_of_stmt(&mut self.core, &stmt_node, text)?;
        // ExecuteStmt.BinaryArgs as []param.BinaryParam is turned into expressions here
        // (ExecBinaryParam); the router passes the arguments evaluated already (spec §6.1).

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

    /// The number of statements whose executors were built and opened (VS1 plan §4.3: a
    /// fallback after that is a bug of the caller; a builder's `Blocked` is counted as not
    /// run).
    // glue: VS1 plan §4.3 (router check)
    pub fn executed_stmts(&self) -> u64 {
        self.executed_stmts
    }

    /// The part of `ExecuteStmt` after the deferred `OnStmtEnd`s are registered.
    // go-part: session.ExecuteStmt
    async fn execute_stmt_started(
        &mut self,
        stmt_node: ast::Stmt,
    ) -> Result<Option<ExecStmtResult>, Error> {
        self.core.on_txn_manager_stmt_start_or_retry(&stmt_node)?;

        // failpoint mockStmtSlow: not ported.

        let mut stmt_label = String::new();
        if let ast::Stmt::Execute(exec_stmt) = &stmt_node
            && let Ok(prepare_stmt) = get_prepared_stmt(exec_stmt, &self.core.sctx.vars)
        {
            stmt_label = crate::glue::get_stmt_label(&prepare_stmt.lock().prepared_ast.stmt)?;
        }
        if stmt_label.is_empty() {
            stmt_label = crate::glue::get_stmt_label(&stmt_node)?;
        }
        self.core.set_request_source(&stmt_label);

        // The planner's table lookups do not read TiKV (D06.3 spec §11.2).
        load_stmt_tables(&self.core.sctx, &stmt_node).await;

        // Transform abstract syntax tree to a physical plan(stored in executor.ExecStmt).
        let text = self.core.sctx.vars.stmt_ctx().original_sql.clone();
        let compiler = Compiler;
        // The statement is handed over to Compile, which freezes it after planning (spec §9.2).
        let stmt = compiler.compile(&mut self.core, stmt_node, &text);
        // The resource group hint check (StmtCtx.ResourceGroupName): resource control, N10.
        let stmt = match stmt {
            Ok(stmt) => {
                // Go's transaction manager holds the node since OnStmtStart; here it gets the
                // frozen statement once the planner is done with it (spec §9.2).
                self.core
                    .txn_manager
                    .set_current_stmt(stmt.stmt_node.as_ref().map(Arc::clone));
                stmt
            }
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
        if let Some(ast::Stmt::Execute(exec_stmt)) = stmt.stmt_node.as_deref()
            && exec_stmt.name.is_empty()
        {
            // for exec-stmt on bin-protocol, ignore the plan detail in `show process` to gain performance benefits.
            self.core.sctx.current_plan = None;
        }

        // Execute the physical plan.
        log_stmt(&stmt, &self.core);

        let r = if stmt.ps_stmt.is_some() {
            // point plan short path
            let mut stmt = stmt;
            let r = stmt.point_get(&mut self.core).await;
            if stmt.executor_opened {
                self.executed_stmts += 1;
            }
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
    /// the plan-cache statement (`PlanCacheStmt`, D13.1). Its preprocessor and plan builder
    /// are not complete yet: a statement on a table stops at the InfoSchema interface
    /// (`blocked-by:D06.3`, VS1-17) and every SELECT at `PlanBuilder.Build`, which needs the
    /// planning context of the optimizer driver (`blocked-by:D09.13`, VS1-24); the router
    /// then restores the session (spec §6.2) and the
    /// PoC owns the statement.
    ///
    /// The prepare dedup cache (`tidb_enable_cache_prepare_stmt`) keeps `PlanCacheStmt`s for
    /// reuse: it is a cache of the plan cache's objects and is counted with the plan-cache
    /// exception A3, so the new session neither reads (a hit is a boundary stub) nor writes
    /// it (D13.1 spec §3).
    // go: session.PrepareStmt L2 (StmtCtx.DetachMemDiskTracker: no session memory tracker, A1; the prepare dedup cache is the plan-cache exception A3)
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

        let node = Arc::new(prepare_stmt_node());
        core.on_txn_manager_stmt_start_or_retry(&node)?;
        core.txn_manager.set_current_stmt(Some(node));

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
                // rebuilds a `PlanCacheStmt` from it. The new session never writes the
                // cache (A3, see above), so this is not reached.
                core.rollback_on_error();
                // glue: blocked-by:D13.1 (the prepare dedup cache is the plan-cache exception A3)
                return Err(blocked(
                    "D13.1",
                    "the prepare dedup cache (the plan-cache exception A3)",
                ));
            }
        }

        // The planner's table lookups do not read TiKV (D06.3 spec §11.2): load the
        // statement's tables into the InfoSchema's table cache first. The prepare executor
        // parses the text again (Go parses it once, inside the executor).
        let load = tidb_parser::parse(sql, &tidb_parser::ParseOptions::default())
            .ok()
            .and_then(|out| match out.stmts.as_slice() {
                [stmt] => Some(load_stmt_tables(&core.sctx, stmt)),
                _ => None,
            });
        if let Some(load) = load {
            load.await;
        }

        let mut prepare_exec = new_prepare_exec(sql);
        let res = prepare_exec.next(core);
        // Rollback even if err is nil.
        core.rollback_on_error();
        res?;

        // Store the result in the dedup cache for future Prepares of the same SQL
        // (SetPrepareStmtDedupCache): not written, the plan-cache exception A3 (see above).
        let _ = dedup_key;
        Ok(PreparedStmtInfo {
            stmt_id: prepare_exec.id,
            param_count: prepare_exec.param_count,
            fields: prepare_exec.fields,
        })
    }

    /// ExecutePreparedStmt executes a prepared statement.
    ///
    /// Go turns the statement into an `ast.ExecuteStmt` holding its `PlanCacheStmt` and runs
    /// it through `ExecuteStmt`, whose compiler reaches the plan cache's miss path
    /// (`OptimizeExecStmt` → `GetPlanFromPlanCache`, ported in the planner). `params` are
    /// the values of Go's `[]expression.Expression` (the router evaluates the binary
    /// arguments, `ExecBinaryParam`, spec §6.1). The statement node has no text (Go's
    /// `Text()` of a node built here is empty).
    // go: session.ExecutePreparedStmt L1
    pub async fn execute_prepared_stmt(
        &mut self,
        stmt_id: u32,
        params: Vec<Datum>,
    ) -> Result<Option<ExecStmtResult>, Error> {
        let Ok(prep_stmt) = self.core.sctx.vars.get_prepared_stmt_by_id(stmt_id) else {
            log::error!("prepared statement not found, stmtID: {stmt_id}");
            return Err(ErrStmtNotFound.clone());
        };
        if !prep_stmt.is::<parking_lot::Mutex<PlanCacheStmt>>() {
            return Err(tidb_base::errors::new(format!(
                "invalid PlanCacheStmt type, stmtID: {stmt_id}"
            )));
        }
        let exec_stmt = ast::ExecuteStmt {
            binary_args: Some(Arc::new(params)),
            prep_stmt: Some(prep_stmt),
            prep_stmt_id: stmt_id,
            ..Default::default()
        };
        self.execute_stmt(ast::Stmt::Execute(Box::new(exec_stmt)), "")
            .await
    }

    /// DropPreparedStmt (COM_STMT_CLOSE): the statement is only recorded; `cleanRetryInfo`
    /// removes it when the transaction ends (commit, rollback, the end of an autocommit
    /// statement), so until then it can still be executed and still counts toward
    /// `max_prepared_stmt_count` (spec §6.1).
    // go: session.DropPreparedStmt L1
    pub fn drop_prepared_stmt(&mut self, stmt_id: u32) -> Result<(), Error> {
        let vars = &mut self.core.sctx.vars;
        if !vars.state.prepared_stmts.contains_key(&stmt_id) {
            return Err(ErrStmtNotFound.clone());
        }
        vars.state
            .retry_info
            .dropped_prepared_stmt_ids
            .push(stmt_id);
        Ok(())
    }

    /// The transaction ended in a statement another engine ran (the PoC, VS1 plan §4.7): the
    /// statements closed in it are removed now, as Go's `cleanRetryInfo` at the end of that
    /// statement would (the router calls it; spec §6.1).
    // glue: VS1 plan §4.7 (cleanRetryInfo for a statement the PoC ran)
    pub fn clean_closed_prepared_stmts(&mut self) {
        self.core.sctx.clean_retry_info();
    }

    /// GetSessionPlanCache: the session's prepared plan cache, `None` when both plan caches
    /// are off; built lazily (a stand-in that holds no plans, [`SessionPlanCache`]).
    // go: session.GetSessionPlanCache L1
    pub fn get_session_plan_cache(&mut self) -> Option<&mut SessionPlanCache> {
        let vars = &self.core.sctx.vars;
        // use the prepared plan cache
        if !vars.enable_prepared_plan_cache && !vars.enable_non_prepared_plan_cache {
            return None;
        }
        Some(self.core.sctx.get_session_plan_cache())
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

/// resolvePreparedStmt: the statement an `EXECUTE` runs (`Go: (ast.StmtNode, error)`).
///
/// The statement of an `EXECUTE` is owned by its `PlanCacheStmt` behind a lock, so it is
/// returned as the statement's handle ([`ResolvedStmt::Prepared`]); Go's `nil` (no session
/// variables, or no prepared AST, which a `PlanCacheStmt` always has here) is `None`. Go's
/// only caller (`shouldCheckConnectionAliveBeforeCommit`) asks whether a statement writes;
/// it is not ported (the write path).
// go: resolvePreparedStmt L2 (the prepared statement is returned as its handle)
pub fn resolve_prepared_stmt<'a>(
    stmt: Option<&'a ast::Stmt>,
    vars: Option<&SessionVars>,
) -> Result<Option<ResolvedStmt<'a>>, Error> {
    let Some(stmt) = stmt else {
        return Ok(None);
    };
    let ast::Stmt::Execute(exec_stmt) = stmt else {
        return Ok(Some(ResolvedStmt::Stmt(stmt)));
    };
    let Some(vars) = vars else {
        return Ok(None);
    };
    let prepare_stmt = get_prepared_stmt(exec_stmt, vars)?;
    Ok(Some(ResolvedStmt::Prepared(prepare_stmt)))
}

/// The statement [`resolve_prepared_stmt`] finds.
pub enum ResolvedStmt<'a> {
    /// The statement itself (not an `EXECUTE`).
    Stmt(&'a ast::Stmt),
    /// The prepared statement of an `EXECUTE`: its AST is `PreparedAst.Stmt`.
    Prepared(PlanCacheStmtRef),
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
    if se.core.sctx.vars.state.txn_ctx.could_retry && !s.is_read_only(&se.core.sctx.vars)? {
        // Only when the txn is could retry and the statement is not read only, need to do
        // stmt-count-limit check.
        crate::tidb::check_stmt_limit(&mut se.core, false).await?;
    }

    let r = s.exec(&mut se.core).await;
    if s.executor_opened {
        se.executed_stmts += 1;
    }

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

/// RemoveLockDDLJobs removes the DDL jobs which doesn't get the metadata lock from job2ver.
///
/// Go's DDL check loop (`domain.mdlCheckLoop`) calls it for every session of the node and
/// reports a job as done on this node only if no session still uses an older schema version
/// of the job's tables. Ported per Go (`session.go:4936-4961`; the gaps of Go's rule are
/// recorded in #252, `go-gap`). `job2ids` holds each job's table IDs (Go passes the
/// comma-separated string and parses it here, `util.Str2Int64Map`). Go logs a transaction
/// that blocks a DDL for over a minute; that log needs `TxnCtx.StartTS` and is left to the
/// reporting hook ([`report_metadata_locks`]). Nothing calls it yet: see that hook.
// go: RemoveLockDDLJobs L2 (the log of a long blocking transaction is left to the reporting hook)
pub fn remove_lock_ddl_jobs(
    vars: &SessionVars,
    job2ver: &mut HashMap<i64, i64>,
    job2ids: &HashMap<i64, std::collections::HashSet<i64>>,
) {
    if vars.in_restricted_sql {
        return;
    }
    let Some(mdl) = vars.state.txn_ctx.related_table_for_mdl() else {
        return;
    };
    for (tbl_id, value) in mdl.0.lock().iter() {
        job2ver.retain(|job_id, ver| {
            let holds = job2ids.get(job_id).is_some_and(|ids| ids.contains(tbl_id));
            !(holds && *value < *ver)
        });
    }
}

/// The hook through which this node's metadata-lock report would include the new-stack
/// sessions (Go: the domain's `mdlCheckLoop` → `CheckOldRunningTxn` → [`remove_lock_ddl_jobs`]
/// over every session). Not wired in this version: the cluster report of this node is still
/// made by the prototype (`crates/poc/domain`, from its own admission registry), which does
/// not see the registrations of new-stack sessions. Consequences, until the hook lands:
/// - the DDL owner does not wait for a `PREPARE` running on the new stack (looser than Go;
///   the statement's table lookups use one InfoSchema version and the router compares it
///   with the prototype's);
/// - the router must not send DML or explicit transactions to the new stack (VS1 tasks,
///   completion conditions), because their registrations last across statements.
// glue: stub of the new-stack MDL reporting hook (domain.mdlCheckLoop over new-stack sessions), blocked-by:D06.5 (the domain's MDL check loop is still the prototype's)
pub fn report_metadata_locks() -> Result<(), Error> {
    Err(blocked(
        "D06.5",
        "reporting the metadata locks of new-stack sessions to the cluster",
    ))
}

/// InitMDLVariable initializes the metadata lock variable (`tidb_enable_metadata_lock`, read
/// by each new transaction into `TxnCtx.EnableMDL`).
///
/// Go reads the `MetadataLock` key of the meta (`meta.Mutator.GetMetadataLock`) at bootstrap
/// and writes `true` when it is unset. Reading the meta at bootstrap is the domain's start-up
/// (D06.5); until then the new stack assumes Go's default, on (v8.5.8 clusters are
/// bootstrapped with it; `SET GLOBAL tidb_enable_metadata_lock` runs on the prototype).
// glue: stub of InitMDLVariable, blocked-by:D06.5 (the meta read at bootstrap; Go's default ON is assumed)
pub fn init_mdl_variable() {
    tidb_sessionctx::variable::tidb_vars::EnableMDL
        .store(true, std::sync::atomic::Ordering::SeqCst);
}
