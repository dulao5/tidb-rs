//! go-file: pkg/sessiontxn/staleread/provider.go
//!
//! The skeleton of the staleness provider (part 2): its state and the getters that need
//! nothing else. Entering a stale transaction (`OnInitialize`, `activateStaleTxn`,
//! `enterNewStaleTxnWithReplaceProvider`) needs the InfoSchema at the stale ts (the domain,
//! D06.5) and the stale-read processor; those methods are boundary stubs.

use std::sync::Arc;

use futures::FutureExt;
use futures::future::BoxFuture;
use tidb_base::Error;
use tidb_kv::kv::Snapshot;

use crate::sctx::{InfoSchemaRef, PlanInfo, ReadTsSource, SessionCtx, StmtInfo};
use crate::{EnterNewTxnType, StmtErrorAction, StmtErrorHandlePoint, TxnContextProvider};

/// StalenessTxnContextProvider implements sessiontxn.TxnContextProvider
pub struct StalenessTxnContextProvider {
    is: Option<InfoSchemaRef>,
    ts: u64,
}

/// NewStalenessTxnContextProvider creates a new StalenessTxnContextProvider
// go: NewStalenessTxnContextProvider L1
pub fn new_staleness_txn_context_provider(
    ts: u64,
    is: Option<InfoSchemaRef>,
) -> StalenessTxnContextProvider {
    StalenessTxnContextProvider { is, ts }
}

/// The stub error of the stale-read methods of part 2.
// glue: stub, blocked-by:D05.4 (part 2: stale read transactions)
fn stale_read_blocked() -> Error {
    tidb_base::blocked::blocked("D05.4", "stale read transactions (part 2)")
}

impl StalenessTxnContextProvider {
    /// GetStmtForUpdateTS will return an error because stale read does not support it
    // go: StalenessTxnContextProvider.GetStmtForUpdateTS L1
    pub fn get_stmt_for_update_ts(&self) -> Result<u64, Error> {
        Err(tidb_base::errors::new(
            "GetForUpdateTS not supported for stalenessTxnProvider",
        ))
    }
}

impl TxnContextProvider for StalenessTxnContextProvider {
    /// AdviseWarmup provides warmup for inner state
    // go: StalenessTxnContextProvider.AdviseWarmup L1
    fn advise_warmup(&mut self, _sctx: &mut dyn SessionCtx) -> Result<(), Error> {
        Ok(())
    }

    /// AdviseOptimizeWithPlan providers optimization according to the plan
    // go: StalenessTxnContextProvider.AdviseOptimizeWithPlan L1
    fn advise_optimize_with_plan(
        &mut self,
        _sctx: &mut dyn SessionCtx,
        _plan: Option<&dyn PlanInfo>,
    ) -> Result<(), Error> {
        Ok(())
    }

    /// GetTxnInfoSchema returns the information schema used by txn
    // go: StalenessTxnContextProvider.GetTxnInfoSchema L1
    fn get_txn_info_schema(&mut self, _sctx: &mut dyn SessionCtx) -> Option<InfoSchemaRef> {
        self.is.clone()
    }

    /// GetTxnScope returns the current txn scope
    // go: StalenessTxnContextProvider.GetTxnScope L1
    fn get_txn_scope(&self, sctx: &dyn SessionCtx) -> String {
        sctx.get_session_vars().state.txn_ctx.txn_scope.clone()
    }

    /// GetReadReplicaScope returns the read replica scope
    // go: StalenessTxnContextProvider.GetReadReplicaScope L1
    fn get_read_replica_scope(&self, _sctx: &dyn SessionCtx) -> String {
        tidb_base::config::get_txn_scope_from_config()
    }

    /// GetStmtReadTS return the read timestamp used by select statement (not for select ...
    /// for update)
    // go: StalenessTxnContextProvider.GetStmtReadTS L1
    fn get_stmt_read_ts<'a>(
        &'a mut self,
        _sctx: &'a mut dyn SessionCtx,
    ) -> BoxFuture<'a, Result<u64, Error>> {
        let ts = self.ts;
        async move { Ok(ts) }.boxed()
    }

    // glue: stub, blocked-by:D05.4 (part 2: StalenessTxnContextProvider.GetSnapshotWithStmtReadTS)
    fn get_snapshot_with_stmt_read_ts<'a>(
        &'a mut self,
        _sctx: &'a mut dyn SessionCtx,
    ) -> BoxFuture<'a, Result<Arc<dyn Snapshot>, Error>> {
        async move { Err(stale_read_blocked()) }.boxed()
    }

    // glue: stub, blocked-by:D05.4 (part 2: StalenessTxnContextProvider.OnInitialize)
    fn on_initialize<'a>(
        &'a mut self,
        _sctx: &'a mut dyn SessionCtx,
        _tp: EnterNewTxnType,
    ) -> BoxFuture<'a, Result<(), Error>> {
        async move { Err(stale_read_blocked()) }.boxed()
    }

    // glue: stub, blocked-by:D05.4 (part 2: StalenessTxnContextProvider.OnStmtStart)
    fn on_stmt_start(
        &mut self,
        _sctx: &mut dyn SessionCtx,
        _node: Option<&dyn StmtInfo>,
    ) -> Result<(), Error> {
        Err(stale_read_blocked())
    }

    // glue: stub, blocked-by:D05.4 (part 2: StalenessTxnContextProvider.OnStmtErrorForNextAction)
    fn on_stmt_error_for_next_action(
        &mut self,
        _sctx: &mut dyn SessionCtx,
        _point: StmtErrorHandlePoint,
        err: Error,
    ) -> (StmtErrorAction, Result<(), Error>) {
        crate::error_action(err)
    }

    // glue: stub, blocked-by:D05.4 (part 2: StalenessTxnContextProvider.OnStmtRetry)
    fn on_stmt_retry(&mut self, _sctx: &mut dyn SessionCtx) -> Result<(), Error> {
        Err(stale_read_blocked())
    }

    /// OnStmtCommit is the hook that should be called when a statement is executed successfully.
    // go: StalenessTxnContextProvider.OnStmtCommit L1
    fn on_stmt_commit(&mut self, _sctx: &mut dyn SessionCtx) -> Result<(), Error> {
        Ok(())
    }

    /// OnStmtRollback is the hook that should be called when a statement fails to execute.
    // go: StalenessTxnContextProvider.OnStmtRollback L1
    fn on_stmt_rollback(
        &mut self,
        _sctx: &mut dyn SessionCtx,
        _is_for_pessimistic_retry: bool,
    ) -> Result<(), Error> {
        Ok(())
    }

    // glue: stub, blocked-by:D06.4 (part 2: local temporary tables)
    fn on_local_temporary_table_created(
        &mut self,
        _sctx: &mut dyn SessionCtx,
    ) -> Result<(), Error> {
        Err(tidb_base::blocked::blocked(
            "D06.4",
            "local temporary tables",
        ))
    }

    // glue: stub, blocked-by:D05.4 (part 2: StalenessTxnContextProvider.ActivateTxn)
    fn activate_txn<'a>(
        &'a mut self,
        _sctx: &'a mut dyn SessionCtx,
    ) -> BoxFuture<'a, Result<(), Error>> {
        async move { Err(stale_read_blocked()) }.boxed()
    }

    // glue: N11 §4.4 ReadTsSource
    fn read_ts_source(&self, _sctx: &dyn SessionCtx) -> ReadTsSource {
        ReadTsSource::Stale(self.ts)
    }

    // glue: N11 §4.4 (a stale read takes no TSO)
    fn stmt_tso_requests(&self) -> u32 {
        0
    }
}
