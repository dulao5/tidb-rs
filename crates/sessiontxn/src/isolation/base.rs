//! go-file: pkg/sessiontxn/isolation/base.go
//!
//! `baseTxnContextProvider` and the parts every isolation provider shares.
//!
//! Go builds each provider by embedding `baseTxnContextProvider` and setting its function
//! fields (`onInitializeTxnCtx`, `onTxnActiveFunc`, `getStmtReadTSFunc`, ...). Here the
//! providers are one struct, [`BaseTxnContextProvider`], whose [`Kind`] holds the state of
//! the concrete provider, and the function fields are `match`es on it (L2, as mapping rule
//! M-SELF does for embedded structs). The per-isolation bodies are in the sibling files.
//!
//! Not here (write path): `SetOptionsBeforeCommit` (commit options, the schema checker),
//! `basePessimisticTxnContextProvider`'s fair locking, `temporaryTableKVFilter`.

use std::sync::Arc;
use std::sync::atomic::Ordering::SeqCst;

use futures::FutureExt;
use futures::future::BoxFuture;
use tidb_base::Error;
use tidb_kv::kv::{GlobalReplicaScope, RC, Snapshot};
use tidb_kv::option::{IsolationLevel, ReplicaRead, RequestSourceInternal, RequestSourceType};
use tidb_kv::txn_scope_var::GlobalTxnScope;
use tidb_sessionctx::variable::session_state::{TransactionContext, TxnInfoSchema};
use tidb_sessionctx::variable::tidb_vars::EnableMDL;

use super::readcommitted::RcState;
use crate::sctx::{
    InfoSchemaRef, PlanInfo, ReadTsSource, SessionCtx, StmtInfo, TsFuture, TxnTsFuture,
};
use crate::{
    EnterNewTxnType, StmtErrorAction, StmtErrorHandlePoint, TxnContextProvider, error_action,
    internal, no_idea,
};

/// The concrete provider (Go's struct embedding `baseTxnContextProvider`) and its state.
pub enum Kind {
    /// `OptimisticTxnContextProvider`.
    Optimistic {
        /// optimizeWithMaxTS: the point-get MaxTS optimization is applied.
        optimize_with_max_ts: bool,
    },
    /// `PessimisticRRTxnContextProvider` (its for-update state is the write path).
    PessimisticRR {
        /// optimizeForNotFetchingLatestTS
        optimize_for_not_fetching_latest_ts: bool,
    },
    /// `PessimisticRCTxnContextProvider`.
    PessimisticRC(RcState),
    /// `PessimisticSerializableTxnContextProvider`.
    PessimisticSerializable,
}

/// baseTxnContextProvider is a base class for the transaction context providers that
/// implement `TxnContextProvider` in different isolation. It provides some common functions
/// below:
///   - Provides a default `OnInitialize` method to initialize its inner state.
///   - Provides some methods like `activateTxn` and `prepareTxn` to manage the inner transaction.
///   - Provides default methods `GetTxnInfoSchema`, `GetStmtReadTS` and `GetStmtForUpdateTS`
///     and return the snapshot information schema or ts when `tidb_snapshot` is set.
///   - Provides other default methods like `Advise`, `OnStmtStart`, `OnStmtRetry` and
///     `OnStmtErrorForNextAction`
pub struct BaseTxnContextProvider {
    // States that should be initialized when baseTxnContextProvider is created and should
    // not be changed after that
    /// Read by `SetOptionsOnTxnActive`'s `GuaranteeLinearizability`, a commit option (write
    /// path).
    #[allow(dead_code)]
    pub(crate) causal_consistency_only: bool,
    pub(crate) kind: Kind,

    // Runtime states
    pub(crate) info_schema: Option<InfoSchemaRef>,
    /// Go `p.txn != nil`: the transaction was activated through this provider (the
    /// transaction itself is the session's, [`SessionCtx::txn`]).
    pub(crate) txn_active: bool,
    pub(crate) is_txn_prepared: bool,
    pub(crate) enter_new_txn_type: EnterNewTxnType,
    /// constStartTS is only used by point get max ts optimization currently. When
    /// constStartTS != 0, we use constStartTS directly without fetching it from tso. To
    /// save the cpu cycles `PrepareTSFuture` will also not be called when warmup (postpone
    /// to activate txn).
    pub(crate) const_start_ts: u64,
}

impl BaseTxnContextProvider {
    /// A provider of `kind` in its initial state.
    // glue: Go's composite literal &baseTxnContextProvider{sctx, causalConsistencyOnly, ...}
    pub(crate) fn with_kind(causal_consistency_only: bool, kind: Kind) -> BaseTxnContextProvider {
        BaseTxnContextProvider {
            causal_consistency_only,
            kind,
            info_schema: None,
            txn_active: false,
            is_txn_prepared: false,
            enter_new_txn_type: EnterNewTxnType::EnterNewTxnDefault,
            const_start_ts: 0,
        }
    }

    /// OnInitialize is the hook that should be called when enter a new txn with this
    /// provider. (Go first checks that the ts functions are set; here they always are.)
    // go: baseTxnContextProvider.OnInitialize L2 (no CreateTime: metrics and slow log, D14.x)
    pub async fn on_initialize_base(
        &mut self,
        sctx: &mut dyn SessionCtx,
        tp: EnterNewTxnType,
    ) -> Result<(), Error> {
        let mut active_now = true;
        match tp {
            EnterNewTxnType::EnterNewTxnDefault => {
                // As we will enter a new txn, we need to commit the old txn if it's still
                // valid. There are two main steps here to enter a new txn:
                // 1. prepareTxnWithOracleTS
                // 2. ActivateTxn
                internal::commit_before_enter_new_txn(sctx).await?;
                self.prepare_txn_with_oracle_ts(sctx)?;
            }
            EnterNewTxnType::EnterNewTxnWithBeginStmt => {
                if !can_reuse_txn_when_explicit_begin(sctx) {
                    internal::commit_before_enter_new_txn(sctx).await?;
                    self.prepare_txn_with_oracle_ts(sctx)?;
                }
                sctx.get_session_vars_mut().set_in_txn(true);
            }
            EnterNewTxnType::EnterNewTxnBeforeStmt => {
                active_now = false;
            }
            _ => {
                return Err(tidb_base::errors::errorf(
                    "Unsupported type: %v",
                    &[tidb_base::gofmt::Arg::from(tp as i64)],
                ));
            }
        }

        self.enter_new_txn_type = tp;
        let is = sctx.get_domain_info_schema();
        self.info_schema = Some(is.clone());
        let vars = sctx.get_session_vars_mut();
        let mut txn_ctx = TransactionContext::default();
        txn_ctx.info_schema = Some(TxnInfoSchema(is));
        txn_ctx.txn_scope = vars.check_and_get_txn_scope();
        self.on_initialize_txn_ctx(&mut txn_ctx);
        vars.state.txn_ctx = txn_ctx;
        if EnableMDL.load(SeqCst) {
            vars.state.txn_ctx.enable_mdl = true;
        }

        let valid = sctx.txn().valid();
        self.is_txn_prepared = valid || sctx.has_prepared_txn_future();
        if active_now {
            self.activate_txn_base(sctx).await?;
        }
        Ok(())
    }

    /// The `onInitializeTxnCtx` function field of the concrete providers.
    // go-part: NewPessimisticRRTxnContextProvider
    fn on_initialize_txn_ctx(&self, txn_ctx: &mut TransactionContext) {
        let isolation = match self.kind {
            Kind::Optimistic { .. } => return,
            Kind::PessimisticRR { .. } => "REPEATABLE-READ", // ast.RepeatableRead
            Kind::PessimisticRC(_) => "READ-COMMITTED",      // ast.ReadCommitted
            Kind::PessimisticSerializable => "SERIALIZABLE", // ast.Serializable
        };
        txn_ctx.is_pessimistic = true;
        txn_ctx.isolation = isolation.to_owned();
    }

    /// GetTxnInfoSchema returns the information schema used by txn.
    ///
    /// Go first returns `SessionVars.SnapshotInfoschema` when `tidb_snapshot` is set (the
    /// field comes with D05.2 / D06.5; `tidb_snapshot` statements go to the PoC, VS1 plan
    /// §4.3), and wraps the schema in `SessionExtendedInfoSchema` (D06.3 part 2, T9).
    // go: baseTxnContextProvider.GetTxnInfoSchema L2 (no SnapshotInfoschema, no SessionExtendedInfoSchema: see doc)
    pub fn get_txn_info_schema_base(&mut self) -> Option<InfoSchemaRef> {
        self.info_schema.clone()
    }

    /// GetTxnScope returns the current txn scope
    // go: baseTxnContextProvider.GetTxnScope L1
    pub fn get_txn_scope_base(&self, sctx: &dyn SessionCtx) -> String {
        sctx.get_session_vars().state.txn_ctx.txn_scope.clone()
    }

    /// GetReadReplicaScope returns the read replica scope
    // go: baseTxnContextProvider.GetReadReplicaScope L1
    pub fn get_read_replica_scope_base(&self, sctx: &dyn SessionCtx) -> String {
        let txn_scope = self.get_txn_scope_base(sctx);
        if txn_scope != GlobalTxnScope && !txn_scope.is_empty() {
            // In local txn, we should use txnScope as the readReplicaScope
            return txn_scope;
        }

        if sctx.get_session_vars().get_replica_read().is_closest_read() {
            // If closest read is set, we should use the scope where instance located.
            return tidb_base::config::get_txn_scope_from_config();
        }

        // When it is not local txn or closet read, we should use global scope
        GlobalReplicaScope.to_owned()
    }

    /// GetStmtReadTS returns the read timestamp used by select statement (not for select ... for update)
    // go: baseTxnContextProvider.GetStmtReadTS L1
    pub async fn get_stmt_read_ts_base(&mut self, sctx: &mut dyn SessionCtx) -> Result<u64, Error> {
        self.activate_txn_base(sctx).await?;

        let snapshot_ts = sctx.get_session_vars().snapshot_ts;
        if snapshot_ts != 0 {
            return Ok(snapshot_ts);
        }
        // p.getStmtReadTSFunc()
        match self.kind {
            Kind::PessimisticRC(_) => self.rc_get_stmt_ts(sctx).await,
            _ => self.get_txn_start_ts(sctx).await,
        }
    }

    /// GetStmtForUpdateTS returns the read timestamp used by update/insert/delete or select
    /// ... for update. Used by the RC read path (`GetSnapshotWithStmtReadTS`); the RR
    /// for-update ts is the write path.
    // go: baseTxnContextProvider.GetStmtForUpdateTS L1
    pub(crate) async fn get_stmt_for_update_ts_base(
        &mut self,
        sctx: &mut dyn SessionCtx,
    ) -> Result<u64, Error> {
        self.activate_txn_base(sctx).await?;

        let snapshot_ts = sctx.get_session_vars().snapshot_ts;
        if snapshot_ts != 0 {
            return Ok(snapshot_ts);
        }
        // p.getStmtForUpdateTSFunc()
        match self.kind {
            Kind::PessimisticRC(_) => self.rc_get_stmt_ts(sctx).await,
            Kind::Optimistic { .. } | Kind::PessimisticSerializable => {
                self.get_txn_start_ts(sctx).await
            }
            // glue: stub, the RR for-update ts is the write path (SELECT ... FOR UPDATE in a
            // transaction is rejected by the session, N11 D8)
            Kind::PessimisticRR { .. } => Err(crate::blocked(
                "D05.4",
                "the for-update ts of a pessimistic repeatable-read transaction (write path)",
            )),
        }
    }

    /// OnStmtStart is the hook that should be called when a new statement started
    // go: baseTxnContextProvider.OnStmtStart L1
    pub fn on_stmt_start_base(&mut self) -> Result<(), Error> {
        Ok(())
    }

    /// OnPessimisticStmtStart is the hook that should be called when starts handling a
    /// pessimistic DML or a pessimistic select-for-update statements.
    // go: baseTxnContextProvider.OnPessimisticStmtStart L1
    pub fn on_pessimistic_stmt_start(&mut self) -> Result<(), Error> {
        Ok(())
    }

    /// OnPessimisticStmtEnd is the hook that should be called when finishes handling a
    /// pessimistic DML or select-for-update statement.
    // go: baseTxnContextProvider.OnPessimisticStmtEnd L1
    pub fn on_pessimistic_stmt_end(&mut self, _is_successful: bool) -> Result<(), Error> {
        Ok(())
    }

    /// OnStmtRetry is the hook that should be called when a statement is retried
    /// internally. (Go also drops `TxnCtx.CurrentStmtPessimisticLockCache`, write path.)
    // go: baseTxnContextProvider.OnStmtRetry L2 (the pessimistic lock cache is the write path)
    pub fn on_stmt_retry_base(&mut self) -> Result<(), Error> {
        Ok(())
    }

    /// OnStmtCommit is the hook that should be called when a statement is executed successfully.
    // go: baseTxnContextProvider.OnStmtCommit L1
    pub fn on_stmt_commit_base(&mut self) -> Result<(), Error> {
        Ok(())
    }

    /// OnStmtRollback is the hook that should be called when a statement fails to execute.
    // go: baseTxnContextProvider.OnStmtRollback L1
    pub fn on_stmt_rollback_base(&mut self, _is_for_pessimistic_retry: bool) -> Result<(), Error> {
        Ok(())
    }

    /// OnLocalTemporaryTableCreated is the hook that should be called when a local
    /// temporary table created.
    // glue: stub, blocked-by:D06.4 (part 2: temptable.AttachLocalTemporaryTableInfoSchema)
    pub fn on_local_temporary_table_created_base(&mut self) -> Result<(), Error> {
        Err(crate::blocked("D06.4", "local temporary tables"))
    }

    /// OnStmtErrorForNextAction is the hook that should be called when a new statement get an error
    // go: baseTxnContextProvider.OnStmtErrorForNextAction L1 [M-ERRVAL]
    pub fn on_stmt_error_for_next_action_base(
        &mut self,
        point: StmtErrorHandlePoint,
        err: Error,
    ) -> (StmtErrorAction, Result<(), Error>) {
        match point {
            StmtErrorHandlePoint::StmtErrAfterPessimisticLock => {
                // for pessimistic lock error, return the error by default
                error_action(err)
            }
            _ => no_idea(),
        }
    }

    // go: baseTxnContextProvider.getTxnStartTS L1
    pub(crate) async fn get_txn_start_ts(
        &mut self,
        sctx: &mut dyn SessionCtx,
    ) -> Result<u64, Error> {
        self.activate_txn_base(sctx).await?;
        Ok(sctx.txn().start_ts())
    }

    /// ActivateTxn activates the transaction and set the relevant context variables. (Go
    /// also resets the row ID shard step and `MemDBFootprint`: write path.)
    // go: baseTxnContextProvider.ActivateTxn L2 (no row ID shard step / MemDBFootprint: write path)
    pub fn activate_txn_base<'a>(
        &'a mut self,
        sctx: &'a mut dyn SessionCtx,
    ) -> BoxFuture<'a, Result<(), Error>> {
        async move {
            if self.txn_active {
                return Ok(());
            }

            self.prepare_txn(sctx)?;

            if self.const_start_ts != 0 {
                self.replace_txn_ts_future(sctx, TxnTsFuture::Constant(self.const_start_ts))?;
            }

            sctx.wait_prepared_txn_future().await?;

            let start_ts = sctx.txn().start_ts();
            let vars = sctx.get_session_vars_mut();
            vars.state.txn_ctx.start_ts = start_ts;

            if self.enter_new_txn_type == EnterNewTxnType::EnterNewTxnBeforeStmt
                && !vars.is_autocommit()
                && vars.snapshot_ts == 0
            {
                vars.set_in_txn(true);
            }

            let kv_vars = vars.state.kv_vars.clone();
            sctx.txn().set_vars(&kv_vars);

            self.set_options_on_txn_active(sctx);

            // p.onTxnActiveFunc(txn, p.enterNewTxnType)
            self.on_txn_active(sctx);

            self.txn_active = true;
            Ok(())
        }
        .boxed()
    }

    /// The `onTxnActiveFunc` function field of the concrete providers.
    // go-part: NewPessimisticRRTxnContextProvider
    fn on_txn_active(&mut self, sctx: &mut dyn SessionCtx) {
        match &mut self.kind {
            // OptimisticTxnContextProvider.onTxnActive sets TxnCtx.CouldRetry, read only by
            // the optimistic commit retry (write path).
            Kind::Optimistic { .. } => {}
            Kind::PessimisticRR { .. } | Kind::PessimisticSerializable => {
                sctx.txn()
                    .set_option(tidb_kv::option::Pessimistic, Box::new(true));
            }
            Kind::PessimisticRC(rc) => {
                let txn = sctx.txn();
                txn.set_option(tidb_kv::option::Pessimistic, Box::new(true));
                rc.latest_oracle_ts = txn.start_ts();
                rc.latest_oracle_ts_valid = true;
            }
        }
    }

    /// prepareTxn prepares txn with an oracle ts future. If the snapshotTS is set, the txn
    /// is prepared with it.
    // go: baseTxnContextProvider.prepareTxn L1
    pub(crate) fn prepare_txn(&mut self, sctx: &mut dyn SessionCtx) -> Result<(), Error> {
        if self.is_txn_prepared {
            return Ok(());
        }

        let snapshot_ts = sctx.get_session_vars().snapshot_ts;
        if snapshot_ts != 0 {
            return self.replace_txn_ts_future(sctx, TxnTsFuture::Constant(snapshot_ts));
        }

        let scope = sctx.get_session_vars().state.txn_ctx.txn_scope.clone();
        let future = new_oracle_future(sctx, &scope);
        self.replace_txn_ts_future(sctx, TxnTsFuture::Oracle(future))
    }

    /// prepareTxnWithOracleTS. The difference between prepareTxnWithOracleTS and prepareTxn
    /// is that prepareTxnWithOracleTS does not consider snapshotTS
    // go: baseTxnContextProvider.prepareTxnWithOracleTS L1
    fn prepare_txn_with_oracle_ts(&mut self, sctx: &mut dyn SessionCtx) -> Result<(), Error> {
        if self.is_txn_prepared {
            return Ok(());
        }

        let scope = sctx.get_session_vars().state.txn_ctx.txn_scope.clone();
        let future = new_oracle_future(sctx, &scope);
        self.replace_txn_ts_future(sctx, TxnTsFuture::Oracle(future))
    }

    // go: baseTxnContextProvider.forcePrepareConstStartTS L1
    pub(crate) fn force_prepare_const_start_ts(&mut self, ts: u64) -> Result<(), Error> {
        if self.txn_active {
            return Err(tidb_base::errors::new(
                "cannot force prepare const start ts because txn is active",
            ));
        }
        self.const_start_ts = ts;
        self.is_txn_prepared = true;
        Ok(())
    }

    // go: baseTxnContextProvider.replaceTxnTsFuture L1
    fn replace_txn_ts_future(
        &mut self,
        sctx: &mut dyn SessionCtx,
        future: TxnTsFuture,
    ) -> Result<(), Error> {
        if sctx.txn().valid() {
            return Ok(());
        }

        let txn_scope = sctx.get_session_vars().state.txn_ctx.txn_scope.clone();
        sctx.prepare_ts_future(future, &txn_scope)?;

        self.is_txn_prepared = true;
        Ok(())
    }

    // go: baseTxnContextProvider.isTidbSnapshotEnabled L1
    pub(crate) fn is_tidb_snapshot_enabled(&self, sctx: &dyn SessionCtx) -> bool {
        sctx.get_session_vars().snapshot_ts != 0
    }

    /// isBeginStmtWithStaleRead indicates whether the current statement is `BeginStmt` type
    /// with stale read Because stale read will use `staleread.StalenessTxnContextProvider`
    /// for query, so if `staleread.IsStmtStaleness()` returns true in other providers, it
    /// means the current statement is `BeginStmt` with stale read
    // go: baseTxnContextProvider.isBeginStmtWithStaleRead L1
    pub(crate) fn is_begin_stmt_with_stale_read(&self, sctx: &dyn SessionCtx) -> bool {
        crate::staleread::is_stmt_staleness(sctx)
    }

    /// AdviseWarmup provides warmup for inner state
    // go: baseTxnContextProvider.AdviseWarmup L1
    pub fn advise_warmup_base(&mut self, sctx: &mut dyn SessionCtx) -> Result<(), Error> {
        if self.is_txn_prepared || self.is_begin_stmt_with_stale_read(sctx) {
            // When executing `START TRANSACTION READ ONLY AS OF ...` no need to warmUp
            return Ok(());
        }
        self.prepare_txn(sctx)
    }

    /// AdviseOptimizeWithPlan providers optimization according to the plan
    // go: baseTxnContextProvider.AdviseOptimizeWithPlan L1
    pub fn advise_optimize_with_plan_base(&mut self) -> Result<(), Error> {
        Ok(())
    }

    /// GetSnapshotWithStmtReadTS gets snapshot with read ts
    // go: baseTxnContextProvider.GetSnapshotWithStmtReadTS L1
    pub async fn get_snapshot_with_stmt_read_ts_base(
        &mut self,
        sctx: &mut dyn SessionCtx,
    ) -> Result<Arc<dyn Snapshot>, Error> {
        let ts = self.get_stmt_read_ts_base(sctx).await?;
        Ok(self.get_snapshot_by_ts(sctx, ts))
    }

    /// GetSnapshotWithStmtForUpdateTS gets snapshot with for update ts (the RC read path
    /// reads with it).
    // go: baseTxnContextProvider.GetSnapshotWithStmtForUpdateTS L1
    pub(crate) async fn get_snapshot_with_stmt_for_update_ts_base(
        &mut self,
        sctx: &mut dyn SessionCtx,
    ) -> Result<Arc<dyn Snapshot>, Error> {
        let ts = self.get_stmt_for_update_ts_base(sctx).await?;
        Ok(self.get_snapshot_by_ts(sctx, ts))
    }

    /// getSnapshotByTS get snapshot from store according to the snapshotTS and set the
    /// transaction related options before return. (The temporary-table interceptor is D06.4
    /// part 2: none.)
    // go: baseTxnContextProvider.getSnapshotByTS L2 (no temporary-table interceptor: D06.4 part 2)
    fn get_snapshot_by_ts(&self, sctx: &mut dyn SessionCtx, snapshot_ts: u64) -> Arc<dyn Snapshot> {
        let (start_ts, for_update_ts) = {
            let txn_ctx = &sctx.get_session_vars().state.txn_ctx;
            (txn_ctx.start_ts, txn_ctx.get_for_update_ts())
        };
        let txn = sctx.txn();
        let snapshot = if txn.valid() && start_ts == for_update_ts && start_ts == snapshot_ts {
            txn.get_snapshot()
        } else {
            internal::get_snapshot_with_ts(sctx, snapshot_ts, None)
        };
        snapshot.set_option(
            ReplicaRead,
            Box::new(sctx.get_session_vars().get_replica_read()),
        );
        snapshot
    }

    /// SetOptionsOnTxnActive: the options of the read path. Not set here (write path or
    /// later blocks): the temporary-table interceptor (D06.4 part 2), the assertion level,
    /// the pipelined DML source, the commit hook, async commit / 1PC, the disk-full option,
    /// the RPC interceptor of `KvExecCounter` and the resource group tagger (Top SQL, D14.2),
    /// `GuaranteeLinearizability` (commit), the background-goroutine hooks (commit).
    // go: baseTxnContextProvider.SetOptionsOnTxnActive L2 (the read-path options; see doc)
    pub fn set_options_on_txn_active(&self, sctx: &mut dyn SessionCtx) {
        let vars = sctx.get_session_vars();
        let read_replica_type = vars.get_replica_read();
        let weak = vars.stmt_ctx().weak_consistency;
        let restricted = vars.in_restricted_sql;
        let source = vars.request_source_type.clone();
        let threshold = vars.load_based_replica_read_threshold;
        let info_schema = vars.state.txn_ctx.info_schema.clone();
        let explicit_source = vars.explicit_request_source_type.clone();
        let conn_id = vars.connection_id;
        let txn = sctx.txn();

        if read_replica_type.is_follower_read() {
            txn.set_option(ReplicaRead, Box::new(read_replica_type));
        }
        if weak {
            txn.set_option(IsolationLevel, Box::new(RC));
        }
        if restricted {
            txn.set_option(RequestSourceInternal, Box::new(true));
        }
        if !source.is_empty() {
            txn.set_option(RequestSourceType, Box::new(source));
        }
        if threshold > 0 {
            txn.set_option(
                tidb_kv::option::LoadBasedReplicaReadThreshold,
                Box::new(threshold),
            );
        }
        txn.set_option(tidb_kv::option::InfoSchema, Box::new(info_schema));
        txn.set_option(
            tidb_kv::option::ExplicitRequestSourceType,
            Box::new(explicit_source),
        );
        txn.set_option(tidb_kv::option::SessionID, Box::new(conn_id));
    }

    /// Where the statement's read ts comes from (N11 §4.4): `tidb_snapshot`, the point-get
    /// MaxTS, an explicit transaction's start ts, the RC statement ts, or a fresh TSO.
    // glue: N11 §4.4 ReadTsSource (no Go counterpart)
    pub fn read_ts_source_base(&self, sctx: &dyn SessionCtx) -> ReadTsSource {
        let vars = sctx.get_session_vars();
        if vars.snapshot_ts != 0 {
            return ReadTsSource::Snapshot(vars.snapshot_ts);
        }
        if let Kind::Optimistic {
            optimize_with_max_ts: true,
        } = self.kind
        {
            return ReadTsSource::MaxTs;
        }
        let in_txn_reuse = !matches!(self.kind, Kind::PessimisticRC(_))
            && self.enter_new_txn_type == EnterNewTxnType::EnterNewTxnWithBeginStmt;
        if in_txn_reuse && vars.state.txn_ctx.start_ts != 0 {
            return ReadTsSource::TxnStart(vars.state.txn_ctx.start_ts);
        }
        if vars.use_low_resolution_tso() {
            return ReadTsSource::LowResolution;
        }
        ReadTsSource::Tso
    }
}

/// canReuseTxnWhenExplicitBegin returns whether we should reuse the txn when starting a
/// transaction explicitly. (`TxnCtx.History` is the retry history of writes, always nil
/// on a read-only node.)
// go: canReuseTxnWhenExplicitBegin L2 (TxnCtx.History: write path, always nil)
pub(crate) fn can_reuse_txn_when_explicit_begin(sctx: &dyn SessionCtx) -> bool {
    let vars = sctx.get_session_vars();
    let txn_ctx = &vars.state.txn_ctx;
    // If BEGIN is the first statement in TxnCtx, we can reuse the existing transaction,
    // without the need to call NewTxn, which commits the existing transaction and begins a
    // new one. If the last un-committed/un-rollback transaction is a time-bounded read-only
    // transaction, we should always create a new transaction. If the variable
    // `tidb_snapshot` is set, we should always create a new transaction because the current
    // txn may be initialized with snapshot ts.
    !txn_ctx.is_staleness && vars.snapshot_ts == 0
}

/// newOracleFuture creates new future according to the scope and the session context
// go: newOracleFuture L2 (no tracing region and failpoint)
pub(crate) fn new_oracle_future(sctx: &dyn SessionCtx, scope: &str) -> TsFuture {
    let oracle_store = sctx.get_oracle();
    if sctx.get_session_vars().use_low_resolution_tso() {
        return oracle_store.get_low_resolution_timestamp_async(scope);
    }
    oracle_store.get_timestamp_async(scope)
}

impl TxnContextProvider for BaseTxnContextProvider {
    // glue: dispatch to the concrete provider's AdviseWarmup
    fn advise_warmup(&mut self, sctx: &mut dyn SessionCtx) -> Result<(), Error> {
        match self.kind {
            Kind::PessimisticRC(_) => self.rc_advise_warmup(sctx),
            _ => self.advise_warmup_base(sctx),
        }
    }

    // glue: dispatch to the concrete provider's AdviseOptimizeWithPlan
    fn advise_optimize_with_plan(
        &mut self,
        sctx: &mut dyn SessionCtx,
        plan: Option<&dyn PlanInfo>,
    ) -> Result<(), Error> {
        match self.kind {
            Kind::Optimistic { .. } => self.optimistic_advise_optimize_with_plan(sctx, plan),
            Kind::PessimisticRR { .. } => self.rr_advise_optimize_with_plan(sctx, plan),
            Kind::PessimisticRC(_) => self.rc_advise_optimize_with_plan(sctx, plan),
            Kind::PessimisticSerializable => self.advise_optimize_with_plan_base(),
        }
    }

    // glue: dispatch (baseTxnContextProvider.GetTxnInfoSchema)
    fn get_txn_info_schema(&mut self, _sctx: &mut dyn SessionCtx) -> Option<InfoSchemaRef> {
        self.get_txn_info_schema_base()
    }

    // glue: dispatch (baseTxnContextProvider.GetTxnScope)
    fn get_txn_scope(&self, sctx: &dyn SessionCtx) -> String {
        self.get_txn_scope_base(sctx)
    }

    // glue: dispatch (baseTxnContextProvider.GetReadReplicaScope)
    fn get_read_replica_scope(&self, sctx: &dyn SessionCtx) -> String {
        self.get_read_replica_scope_base(sctx)
    }

    // glue: dispatch to the concrete provider's GetStmtReadTS
    fn get_stmt_read_ts<'a>(
        &'a mut self,
        sctx: &'a mut dyn SessionCtx,
    ) -> BoxFuture<'a, Result<u64, Error>> {
        async move {
            match self.kind {
                Kind::Optimistic { .. } => self.optimistic_get_stmt_read_ts(sctx).await,
                _ => self.get_stmt_read_ts_base(sctx).await,
            }
        }
        .boxed()
    }

    // glue: dispatch to the concrete provider's GetSnapshotWithStmtReadTS
    fn get_snapshot_with_stmt_read_ts<'a>(
        &'a mut self,
        sctx: &'a mut dyn SessionCtx,
    ) -> BoxFuture<'a, Result<Arc<dyn Snapshot>, Error>> {
        async move {
            match self.kind {
                Kind::PessimisticRC(_) => self.rc_get_snapshot_with_stmt_read_ts(sctx).await,
                _ => self.get_snapshot_with_stmt_read_ts_base(sctx).await,
            }
        }
        .boxed()
    }

    // glue: dispatch (baseTxnContextProvider.OnInitialize)
    fn on_initialize<'a>(
        &'a mut self,
        sctx: &'a mut dyn SessionCtx,
        tp: EnterNewTxnType,
    ) -> BoxFuture<'a, Result<(), Error>> {
        self.on_initialize_base(sctx, tp).boxed()
    }

    // glue: dispatch to the concrete provider's OnStmtStart
    fn on_stmt_start(
        &mut self,
        sctx: &mut dyn SessionCtx,
        node: Option<&dyn StmtInfo>,
    ) -> Result<(), Error> {
        match self.kind {
            Kind::PessimisticRR { .. } => self.rr_on_stmt_start(),
            Kind::PessimisticRC(_) => self.rc_on_stmt_start(sctx, node),
            _ => self.on_stmt_start_base(),
        }
    }

    // glue: dispatch to the concrete provider's OnStmtErrorForNextAction
    fn on_stmt_error_for_next_action(
        &mut self,
        sctx: &mut dyn SessionCtx,
        point: StmtErrorHandlePoint,
        err: Error,
    ) -> (StmtErrorAction, Result<(), Error>) {
        match self.kind {
            Kind::PessimisticRC(_) => self.rc_on_stmt_error_for_next_action(sctx, point, err),
            Kind::PessimisticRR { .. } => {
                self.pessimistic_on_stmt_error_for_next_action(point, err)
            }
            Kind::PessimisticSerializable => {
                self.serializable_on_stmt_error_for_next_action(point, err)
            }
            Kind::Optimistic { .. } => self.on_stmt_error_for_next_action_base(point, err),
        }
    }

    // glue: dispatch to the concrete provider's OnStmtRetry
    fn on_stmt_retry(&mut self, _sctx: &mut dyn SessionCtx) -> Result<(), Error> {
        match self.kind {
            Kind::PessimisticRR { .. } => self.rr_on_stmt_retry(),
            Kind::PessimisticRC(_) => self.rc_on_stmt_retry(),
            _ => self.on_stmt_retry_base(),
        }
    }

    // glue: dispatch (baseTxnContextProvider.OnStmtCommit)
    fn on_stmt_commit(&mut self, _sctx: &mut dyn SessionCtx) -> Result<(), Error> {
        self.on_stmt_commit_base()
    }

    // glue: dispatch (baseTxnContextProvider.OnStmtRollback)
    fn on_stmt_rollback(
        &mut self,
        _sctx: &mut dyn SessionCtx,
        is_for_pessimistic_retry: bool,
    ) -> Result<(), Error> {
        self.on_stmt_rollback_base(is_for_pessimistic_retry)
    }

    // glue: dispatch (baseTxnContextProvider.OnLocalTemporaryTableCreated)
    fn on_local_temporary_table_created(
        &mut self,
        _sctx: &mut dyn SessionCtx,
    ) -> Result<(), Error> {
        self.on_local_temporary_table_created_base()
    }

    // glue: dispatch (baseTxnContextProvider.ActivateTxn)
    fn activate_txn<'a>(
        &'a mut self,
        sctx: &'a mut dyn SessionCtx,
    ) -> BoxFuture<'a, Result<(), Error>> {
        self.activate_txn_base(sctx)
    }

    // glue: N11 §4.4 ReadTsSource
    fn read_ts_source(&self, sctx: &dyn SessionCtx) -> ReadTsSource {
        match &self.kind {
            Kind::PessimisticRC(rc) if rc.stmt_ts != 0 => ReadTsSource::Tso,
            _ => self.read_ts_source_base(sctx),
        }
    }
}
