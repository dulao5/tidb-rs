//! go-file: pkg/session/txnmanager.go
//!
//! `txnManager`: the session's transaction manager, which owns the transaction context
//! provider of the current transaction (D05.4, `tidb-sessiontxn`).
//!
//! Carriers (spec `specs/D05.3-session/spec.md` §2.1):
//! - Go stores the manager in `SessionVars.TxnManager` and keeps the session in it
//!   (`m.sctx`). Here the session owns the manager next to the part of the session the
//!   providers see ([`SessCtx`]), and every method takes that part: the provider and the
//!   session are borrowed at the same time, so a future cancelled while it waits for a TSO
//!   leaves the provider in place.
//! - A provider that commits the old transaction while it enters a new one
//!   (`CommitBeforeEnterNewTxn`) reaches the session's `CommitTxn`, whose `OnTxnEnd` is the
//!   manager's: the session records it ([`SessCtx::take_txn_end`]) and the manager runs it
//!   when the provider returns.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tidb_base::Error;
use tidb_kv::kv::Snapshot;
use tidb_parser::ast;
use tidb_sessiontxn::isolation::{
    BaseTxnContextProvider, new_pessimistic_rc_txn_context_provider,
    new_pessimistic_rr_txn_context_provider, new_pessimistic_serializable_txn_context_provider,
};
use tidb_sessiontxn::sctx::{InfoSchemaRef, PlanInfo};
use tidb_sessiontxn::staleread::new_staleness_txn_context_provider;
use tidb_sessiontxn::{
    EnterNewTxnRequest, EnterNewTxnType, StmtErrorAction, StmtErrorHandlePoint, TxnContextProvider,
    no_idea,
};

use crate::glue::StmtNode;
use crate::session::SessCtx;

/// `ast.Optimistic`.
#[allow(non_upper_case_globals)] // M-GONAME
pub const Optimistic: &str = "OPTIMISTIC";
/// `ast.Pessimistic`.
#[allow(non_upper_case_globals)] // M-GONAME
pub const Pessimistic: &str = "PESSIMISTIC";

/// One event of a transaction, for the slow transaction log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    pub event: String,
    pub duration: Duration,
}

impl Event {
    /// The zap object of the slow transaction log: `{event, gap}`.
    // go: event.MarshalLogObject L2 (a log string instead of a zap encoder)
    pub fn marshal_log_object(&self) -> String {
        format!(
            "{{\"event\": {:?}, \"gap\": {:?}}}",
            self.event, self.duration
        )
    }
}

/// txnManager implements sessiontxn.TxnManager
///
/// Go keeps two optimistic providers and alternates them (`reservedOptimisticProviders`) to
/// save an allocation per transaction; here each transaction gets a new one (M-POOL).
pub struct TxnManager {
    stmt_node: Option<Arc<ast::Stmt>>,
    ctx_provider: Option<Box<dyn TxnContextProvider>>,

    // used for slow transaction logs
    events: Option<Vec<Event>>,
    last_instant: Instant,
    enter_txn_instant: Instant,
}

/// `errors.New("context provider not set")`.
// glue: Go's inline errors.New of the file
fn context_provider_not_set() -> Error {
    tidb_base::errors::new("context provider not set")
}

impl Default for TxnManager {
    // glue: Default through newTxnManager
    fn default() -> Self {
        new_txn_manager()
    }
}

/// `getTxnManager(sctx)`: Go creates the manager on first use and stores it in
/// `SessionVars.TxnManager`; here the session creates it with itself.
// go: getTxnManager L2 (the session owns the manager)
pub fn get_txn_manager(sctx: &mut crate::session::SessionCore) -> &mut TxnManager {
    &mut sctx.txn_manager
}

/// `init`: Go installs `getTxnManager` as `sessiontxn.GetTxnManager`; the session passes its
/// manager to sessiontxn's helpers instead (D05.4 spec §2).
// go: init L2 (no global hook: the manager is passed)
pub fn init() {}

// go: newTxnManager L2 (the session is passed to the methods, not stored)
pub fn new_txn_manager() -> TxnManager {
    let now = Instant::now();
    TxnManager {
        stmt_node: None,
        ctx_provider: None,
        events: None,
        last_instant: now,
        enter_txn_instant: now,
    }
}

impl TxnManager {
    // go: txnManager.GetTxnInfoSchema L2 (no SessionExtendedInfoSchema: D06.3 T9)
    pub fn get_txn_info_schema(&mut self, sctx: &mut SessCtx) -> Option<InfoSchemaRef> {
        if let Some(p) = self.ctx_provider.as_mut() {
            return p.get_txn_info_schema(sctx);
        }

        Some(sctx.get_domain_info_schema())
    }

    // go: txnManager.GetStmtReadTS L1
    pub async fn get_stmt_read_ts(&mut self, sctx: &mut SessCtx) -> Result<u64, Error> {
        match self.ctx_provider.as_mut() {
            None => Err(context_provider_not_set()),
            Some(p) => p.get_stmt_read_ts(sctx).await,
        }
    }

    // go: txnManager.GetTxnScope L1
    pub fn get_txn_scope(&self, sctx: &SessCtx) -> String {
        match &self.ctx_provider {
            None => tidb_kv::txn_scope_var::GlobalTxnScope.to_owned(),
            Some(p) => p.get_txn_scope(sctx),
        }
    }

    // go: txnManager.GetReadReplicaScope L1
    pub fn get_read_replica_scope(&self, sctx: &SessCtx) -> String {
        match &self.ctx_provider {
            None => tidb_kv::kv::GlobalReplicaScope.to_owned(),
            Some(p) => p.get_read_replica_scope(sctx),
        }
    }

    /// GetSnapshotWithStmtReadTS gets snapshot with read ts
    // go: txnManager.GetSnapshotWithStmtReadTS L1
    pub async fn get_snapshot_with_stmt_read_ts(
        &mut self,
        sctx: &mut SessCtx,
    ) -> Result<Arc<dyn Snapshot>, Error> {
        match self.ctx_provider.as_mut() {
            None => Err(context_provider_not_set()),
            Some(p) => p.get_snapshot_with_stmt_read_ts(sctx).await,
        }
    }

    // go: txnManager.GetContextProvider L1
    pub fn get_context_provider(&self) -> Option<&dyn TxnContextProvider> {
        self.ctx_provider.as_deref()
    }

    // go: txnManager.EnterNewTxn L2 (the provider's OnTxnEnd through the session is run here, see the module doc)
    pub async fn enter_new_txn(
        &mut self,
        sctx: &mut SessCtx,
        r: EnterNewTxnRequest,
    ) -> Result<(), Error> {
        let tp = r.tp;
        let mut ctx_provider = self.new_provider_with_request(sctx, r)?;

        let res = ctx_provider.on_initialize(sctx, tp).await;
        // The old transaction committed by the provider (CommitBeforeEnterNewTxn) ends here.
        if sctx.take_txn_end().is_some() {
            self.on_txn_end(sctx);
        }
        if let Err(err) = res {
            sctx.rollback_txn_state();
            self.on_txn_end(sctx);
            return Err(err);
        }

        self.ctx_provider = Some(ctx_provider);
        if tp == EnterNewTxnType::EnterNewTxnWithBeginStmt {
            sctx.vars.set_in_txn(true);
        }

        self.reset_events();
        self.record_event("enter txn");
        Ok(())
    }

    // go: txnManager.OnTxnEnd L1
    pub fn on_txn_end(&mut self, sctx: &SessCtx) {
        self.ctx_provider = None;
        self.stmt_node = None;

        let gap = self.last_instant.elapsed();
        self.events.get_or_insert_with(Vec::new).push(Event {
            event: "txn end".to_owned(),
            duration: gap,
        });

        let duration = self.enter_txn_instant.elapsed();
        let threshold = sctx.vars.slow_txn_threshold;
        if threshold > 0 && duration.as_millis() as u64 >= threshold {
            let events: Vec<String> = self
                .events
                .iter()
                .flatten()
                .map(Event::marshal_log_object)
                .collect();
            let events_str = format!("[{}]", events.join(", "));
            tidb_base::log_info!(
                "slow transaction",
                "duration" => duration,
                "conn" => sctx.vars.connection_id,
                "txnStartTS" => sctx.vars.state.txn_ctx.start_ts,
                "events" => events_str.as_str(),
            );
        }

        self.last_instant = Instant::now();
    }

    // go: txnManager.GetCurrentStmt L1
    pub fn get_current_stmt(&self) -> Option<&Arc<ast::Stmt>> {
        self.stmt_node.as_ref()
    }

    /// The statement the current one is from now on (`GetCurrentStmt`): the session hands it
    /// over after `Compile` froze it (spec §9.2), `PrepareStmt` right after `OnStmtStart`.
    // glue: the second half of Go's `m.stmtNode = node` (the node is shared after planning, spec §9.2)
    pub fn set_current_stmt(&mut self, node: Option<Arc<ast::Stmt>>) {
        self.stmt_node = node;
    }

    /// OnStmtStart is the hook that should be called when a new statement started
    ///
    /// The event name is the normalized SQL (`parser.Normalize`, D02.1); until it is ported
    /// the event is the statement kind.
    ///
    /// The node is borrowed: the providers read it during the call (the RC provider's
    /// `IsReadOnly`), and the planner, which writes to it, has not run yet. Go also stores
    /// it as the current statement (`m.stmtNode = node`); here the current statement is
    /// cleared and handed over later with [`TxnManager::set_current_stmt`], once the planner
    /// is done with it (spec §9.2: nothing reads it in between).
    // go: txnManager.OnStmtStart L2 (the event name: parser.Normalize is D02.1; the node is stored after Compile, spec §9.2)
    pub fn on_stmt_start(
        &mut self,
        sctx: &mut SessCtx,
        node: Option<&ast::Stmt>,
    ) -> Result<(), Error> {
        self.stmt_node = None;

        let Some(p) = self.ctx_provider.as_mut() else {
            return Err(context_provider_not_set());
        };

        let sql = match node {
            // node.OriginalText() normalized (parser.Normalize, D02.1).
            Some(n) => crate::glue::stmt_kind(n).to_owned(),
            None => String::new(),
        };
        let r = match node {
            Some(n) => p.on_stmt_start(sctx, Some(&StmtNode(n))),
            None => p.on_stmt_start(sctx, None),
        };
        self.record_event(&sql);
        r
    }

    /// OnStmtEnd implements the TxnManager interface
    // go: txnManager.OnStmtEnd L1
    pub fn on_stmt_end(&mut self) {
        self.record_event("stmt end");
    }

    /// OnStmtErrorForNextAction is the hook that should be called when a new statement get
    /// an error
    // go: txnManager.OnStmtErrorForNextAction L1 [M-ERRVAL]
    pub fn on_stmt_error_for_next_action(
        &mut self,
        sctx: &mut SessCtx,
        point: StmtErrorHandlePoint,
        err: Error,
    ) -> (StmtErrorAction, Result<(), Error>) {
        match self.ctx_provider.as_mut() {
            None => no_idea(),
            Some(p) => p.on_stmt_error_for_next_action(sctx, point, err),
        }
    }

    /// ActivateTxn decides to activate txn according to the parameter `active`
    // go: txnManager.ActivateTxn L2 (returns no transaction: the caller reads the session's)
    pub async fn activate_txn(&mut self, sctx: &mut SessCtx) -> Result<(), Error> {
        match self.ctx_provider.as_mut() {
            None => Err(tidb_kv::error::ErrInvalidTxn.clone()),
            Some(p) => p.activate_txn(sctx).await,
        }
    }

    /// OnStmtRetry is the hook that should be called when a statement retry
    // go: txnManager.OnStmtRetry L1
    pub fn on_stmt_retry(&mut self, sctx: &mut SessCtx) -> Result<(), Error> {
        match self.ctx_provider.as_mut() {
            None => Err(context_provider_not_set()),
            Some(p) => p.on_stmt_retry(sctx),
        }
    }

    /// OnStmtCommit is the hook that should be called when a statement is executed
    /// successfully.
    // go: txnManager.OnStmtCommit L1
    pub fn on_stmt_commit(&mut self, sctx: &mut SessCtx) -> Result<(), Error> {
        if self.ctx_provider.is_none() {
            return Err(context_provider_not_set());
        }
        self.record_event("stmt commit");
        self.ctx_provider
            .as_mut()
            .expect("checked above")
            .on_stmt_commit(sctx)
    }

    // go: txnManager.recordEvent L1
    fn record_event(&mut self, event_name: &str) {
        if self.events.is_none() {
            self.reset_events();
        }
        let gap = self.last_instant.elapsed();
        self.events.get_or_insert_with(Vec::new).push(Event {
            event: event_name.to_owned(),
            duration: gap,
        });
        self.last_instant = Instant::now();
    }

    // go: txnManager.resetEvents L1
    fn reset_events(&mut self) {
        match &mut self.events {
            None => self.events = Some(Vec::with_capacity(10)),
            Some(e) => e.clear(),
        }
        self.enter_txn_instant = Instant::now();
    }

    /// OnStmtRollback is the hook that should be called when a statement fails to execute.
    // go: txnManager.OnStmtRollback L1
    pub fn on_stmt_rollback(
        &mut self,
        sctx: &mut SessCtx,
        is_for_pessimistic_retry: bool,
    ) -> Result<(), Error> {
        if self.ctx_provider.is_none() {
            return Err(context_provider_not_set());
        }
        self.record_event("stmt rollback");
        self.ctx_provider
            .as_mut()
            .expect("checked above")
            .on_stmt_rollback(sctx, is_for_pessimistic_retry)
    }

    /// OnLocalTemporaryTableCreated is the hook that should be called when a temporary table
    /// created. The provider will update its state then
    // go: txnManager.OnLocalTemporaryTableCreated L1
    pub fn on_local_temporary_table_created(&mut self, sctx: &mut SessCtx) -> Result<(), Error> {
        if let Some(p) = self.ctx_provider.as_mut() {
            // The providers' hook is a blocked-by:D06.4 stub (temporary tables).
            return p.on_local_temporary_table_created(sctx);
        }
        Ok(())
    }

    /// `BulkDMLEnabled` (pipelined DML) is the write path: never set.
    // go: txnManager.AdviseWarmup L1
    pub fn advise_warmup(&mut self, sctx: &mut SessCtx) -> Result<(), Error> {
        // sessVars.BulkDMLEnabled: the write path, always false.
        if let Some(p) = self.ctx_provider.as_mut() {
            return p.advise_warmup(sctx);
        }
        Ok(())
    }

    /// AdviseOptimizeWithPlan providers optimization according to the plan
    // go: txnManager.AdviseOptimizeWithPlan L1
    pub fn advise_optimize_with_plan(
        &mut self,
        sctx: &mut SessCtx,
        plan: Option<&dyn PlanInfo>,
    ) -> Result<(), Error> {
        if let Some(p) = self.ctx_provider.as_mut() {
            return p.advise_optimize_with_plan(sctx, plan);
        }
        Ok(())
    }

    // go: txnManager.newProviderWithRequest L1
    #[allow(non_upper_case_globals)] // M-GONAME: ast.Optimistic / ast.Pessimistic as patterns
    fn new_provider_with_request(
        &mut self,
        sctx: &mut SessCtx,
        r: EnterNewTxnRequest,
    ) -> Result<Box<dyn TxnContextProvider>, Error> {
        if let Some(p) = r.provider {
            return Ok(p);
        }

        if r.stale_read_ts > 0 {
            sctx.vars.state.txn_ctx.stale_read_ts = r.stale_read_ts;
            return Ok(Box::new(new_staleness_txn_context_provider(
                r.stale_read_ts,
                None,
            )));
        }

        let sess_vars = &sctx.vars;

        let mut txn_mode = r.txn_mode;
        if txn_mode.is_empty() {
            txn_mode = sess_vars.txn_mode.clone();
        }

        match txn_mode.as_str() {
            "" | Optimistic => {
                // When txnMode is 'OPTIMISTIC' or '', the transaction should be optimistic
                Ok(Box::new(BaseTxnContextProvider::reset_for_new_txn(
                    r.causal_consistency_only,
                )))
            }
            Pessimistic => {
                // When txnMode is 'PESSIMISTIC', the provider should be determined by the
                // isolation level
                match sess_vars.isolation_level_for_new_txn().as_str() {
                    "READ-COMMITTED" => Ok(Box::new(new_pessimistic_rc_txn_context_provider(
                        r.causal_consistency_only,
                    ))),
                    // The Oracle serializable isolation is actually SI in pessimistic mode.
                    // Do not update ForUpdateTS when the user is using the Serializable
                    // isolation level. It can be used temporarily on the few occasions when
                    // an Oracle-like isolation level is needed. Support for this does not
                    // mean that TiDB supports serializable isolation of MySQL.
                    // tidb_skip_isolation_level_check should still be disabled by default.
                    "SERIALIZABLE" => {
                        Ok(Box::new(new_pessimistic_serializable_txn_context_provider(
                            r.causal_consistency_only,
                        )))
                    }
                    // We use Repeatable read for all other cases.
                    _ => Ok(Box::new(new_pessimistic_rr_txn_context_provider(
                        r.causal_consistency_only,
                    ))),
                }
            }
            _ => {
                let err = tidb_base::errors::errorf(
                    "Invalid txn mode '%s'",
                    &[tidb_base::gofmt::Arg::from(txn_mode.clone())],
                );
                Err(err)
            }
        }
    }

    /// Whether the manager is idle: no provider, no statement (the state after a
    /// transaction ended; VS1 plan §4.6).
    // glue: VS1 plan §4.6 (checkpoint)
    pub fn is_idle(&self) -> bool {
        self.ctx_provider.is_none() && self.stmt_node.is_none()
    }
}
