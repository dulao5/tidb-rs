//! go-file: pkg/sessiontxn/isolation/readcommitted.go
//!
//! The read path of the pessimistic read-committed provider: each statement reads at its
//! own ts (a TSO; the transaction's start ts for the first statement).
//!
//! `tidb_rc_read_check_ts` (read with `RCCheckTS` and retry on a write conflict) needs the
//! read-side check in client-rust; until then a statement that would use it is rejected
//! with "not supported" instead of reading at the reused ts without the check (N11 D11,
//! spec §4.4). The write-side pieces (`handleAfterPessimisticLockError`, the
//! `tidb_rc_write_check_ts` plans of `planSkipGetTsoFromPD`, `GetSnapshotWithStmtForUpdateTS`
//! with `RCCheckTS`, `IsCheckTSInWriteStmtMode`) are the write path.

use std::sync::Arc;
use std::sync::atomic::Ordering::SeqCst;
use std::time::Instant;

use tidb_base::Error;
use tidb_kv::kv::{RCCheckTS, Snapshot};
use tidb_kv::option::{IsolationLevel, SnapshotTS};
use tidb_sessionctx::variable::session::SessionVars;
use tidb_sessionctx::variable::tidb_vars::EnableRCReadCheckTS;

use super::base::{BaseTxnContextProvider, Kind, new_oracle_future};
use crate::sctx::{PlanInfo, PlanKind, SessionCtx, StmtInfo, TsFuture};
use crate::{StmtErrorAction, StmtErrorHandlePoint, no_idea, retry_ready};

/// The statement ts future of RC (Go `oracle.Future`: a `funcFuture` over the provider, a
/// `ConstantFuture`, or the wrapped oracle future of `getOracleFuture`).
pub enum StmtTsFuture {
    /// `funcFuture(p.getTxnStartTS)`.
    UseStartTs,
    /// `sessiontxn.ConstantFuture`.
    Constant(u64),
    /// `getOracleFuture()`: a TSO, then the for-update ts bookkeeping.
    Oracle(TsFuture),
}

/// stmtState: the per-statement state of RC, and the provider's other fields.
#[derive(Default)]
pub struct RcState {
    pub(crate) stmt_ts: u64,
    pub(crate) stmt_ts_future: Option<StmtTsFuture>,
    pub(crate) stmt_use_start_ts: bool,
    /// latestOracleTS records the latest timestamp fetched from the oracle (including the
    /// start ts).
    pub(crate) latest_oracle_ts: u64,
    /// latestOracleTSValid shows whether we have already fetched a ts from pd and whether
    /// the ts we fetched is still valid.
    pub(crate) latest_oracle_ts_valid: bool,
    /// checkTSInWriteStmt is used to set RCCheckTS isolation for getting value when doing
    /// point-write
    pub(crate) check_ts_in_write_stmt: bool,
}

impl RcState {
    // go: stmtState.prepareStmt L1
    fn prepare_stmt(&mut self, use_start_ts: bool) -> Result<(), Error> {
        self.stmt_ts = 0;
        self.stmt_ts_future = None;
        self.stmt_use_start_ts = use_start_ts;
        Ok(())
    }
}

/// PessimisticRCTxnContextProvider provides txn context for isolation level read-committed
/// (the [`Kind::PessimisticRC`] provider).
pub type PessimisticRCTxnContextProvider = BaseTxnContextProvider;

/// NewPessimisticRCTxnContextProvider returns a new PessimisticRCTxnContextProvider
// go: NewPessimisticRCTxnContextProvider L1
pub fn new_pessimistic_rc_txn_context_provider(
    causal_consistency_only: bool,
) -> PessimisticRCTxnContextProvider {
    BaseTxnContextProvider::with_kind(
        causal_consistency_only,
        Kind::PessimisticRC(RcState::default()),
    )
}

/// NeedSetRCCheckTSFlag checks whether it's needed to set `RCCheckTS` flag in current stmtctx.
// go: NeedSetRCCheckTSFlag L1
pub fn need_set_rc_check_ts_flag(vars: &SessionVars, node: &dyn StmtInfo) -> bool {
    vars.connection_id > 0
        && EnableRCReadCheckTS.load(SeqCst)
        && vars.in_txn()
        && !retrying(vars)
        && node.is_read_only(vars)
}

/// `SessionVars.RetryInfo.Retrying`: the optimistic retry of a transaction's writes (block X,
/// not in tidb-rs's `SessionVars`), never in progress on a read-only node.
// glue: RetryInfo.Retrying of a read-only session
fn retrying(_vars: &SessionVars) -> bool {
    false
}

impl BaseTxnContextProvider {
    // glue: the RC state of the provider (Go: the fields of PessimisticRCTxnContextProvider)
    fn rc(&mut self) -> &mut RcState {
        match &mut self.kind {
            Kind::PessimisticRC(rc) => rc,
            _ => unreachable!("an RC method on a provider of another isolation"),
        }
    }

    /// OnStmtStart is the hook that should be called when a new statement started. Where
    /// Go sets `StmtCtx.RCCheckTS` (`tidb_rc_read_check_ts`), the statement is rejected
    /// (module docs).
    // go: PessimisticRCTxnContextProvider.OnStmtStart L1
    // go-exception: N11 D11 (tidb_rc_read_check_ts is rejected until client-rust checks reads)
    pub(crate) fn rc_on_stmt_start(
        &mut self,
        sctx: &mut dyn SessionCtx,
        node: Option<&dyn StmtInfo>,
    ) -> Result<(), Error> {
        self.on_stmt_start_base()?;

        if let Some(node) = node
            && need_set_rc_check_ts_flag(sctx.get_session_vars(), node)
        {
            return Err(crate::blocked(
                "N11 D11",
                "tidb_rc_read_check_ts (read-committed reads with ts checking)",
            ));
        }
        let prepared = self.is_txn_prepared;
        let rc = self.rc();
        rc.check_ts_in_write_stmt = false;

        rc.prepare_stmt(!prepared)
    }

    /// OnStmtErrorForNextAction is the hook that should be called when a new statement get
    /// an error. (The errors after a pessimistic lock come from locking statements, write
    /// path: they are returned.)
    // go: PessimisticRCTxnContextProvider.OnStmtErrorForNextAction L2 (lock errors are returned: write path)
    pub(crate) fn rc_on_stmt_error_for_next_action(
        &mut self,
        sctx: &mut dyn SessionCtx,
        point: StmtErrorHandlePoint,
        err: Error,
    ) -> (StmtErrorAction, Result<(), Error>) {
        match point {
            StmtErrorHandlePoint::StmtErrAfterQuery => self.rc_handle_after_query_error(sctx, err),
            StmtErrorHandlePoint::StmtErrAfterPessimisticLock => crate::error_action(err),
        }
    }

    /// OnStmtRetry is the hook that should be called when a statement is retried internally.
    // go: PessimisticRCTxnContextProvider.OnStmtRetry L1
    pub(crate) fn rc_on_stmt_retry(&mut self) -> Result<(), Error> {
        self.on_stmt_retry_base()?;
        let rc = self.rc();
        rc.latest_oracle_ts_valid = false;
        rc.check_ts_in_write_stmt = false;
        rc.prepare_stmt(false)
    }

    // go: PessimisticRCTxnContextProvider.prepareStmtTS L1
    fn rc_prepare_stmt_ts(&mut self, sctx: &mut dyn SessionCtx) {
        if self.rc().stmt_ts_future.is_some() {
            return;
        }
        let rc_check_ts = sctx.get_session_vars().stmt_ctx().rc_check_ts;
        let rc = self.rc();
        let stmt_ts_future = if rc.stmt_use_start_ts {
            StmtTsFuture::UseStartTs
        } else if rc.latest_oracle_ts_valid && rc_check_ts {
            StmtTsFuture::Constant(rc.latest_oracle_ts)
        } else {
            self.rc_get_oracle_future(sctx)
        };

        self.rc().stmt_ts_future = Some(stmt_ts_future);
    }

    /// The TSO future of the statement (Go wraps it with the for-update ts bookkeeping,
    /// done when it is waited: [`BaseTxnContextProvider::rc_get_stmt_ts`]).
    // go: PessimisticRCTxnContextProvider.getOracleFuture L2 (the wrapper runs where the future is waited)
    fn rc_get_oracle_future(&self, sctx: &mut dyn SessionCtx) -> StmtTsFuture {
        let scope = sctx.get_session_vars().state.txn_ctx.txn_scope.clone();
        StmtTsFuture::Oracle(new_oracle_future(sctx, &scope))
    }

    // go: PessimisticRCTxnContextProvider.getStmtTS L1
    pub(crate) async fn rc_get_stmt_ts(&mut self, sctx: &mut dyn SessionCtx) -> Result<u64, Error> {
        if self.rc().stmt_ts != 0 {
            return Ok(self.rc().stmt_ts);
        }

        self.activate_txn_base(sctx).await?;

        self.rc_prepare_stmt_ts(sctx);
        let start = Instant::now();
        let ts = match self.rc().stmt_ts_future.take() {
            Some(StmtTsFuture::UseStartTs) => self.get_txn_start_ts(sctx).await?,
            Some(StmtTsFuture::Constant(ts)) => ts,
            Some(StmtTsFuture::Oracle(future)) => {
                // The funcFuture of getOracleFuture.
                let ts = future.await?;
                let txn_ctx = &mut sctx.get_session_vars_mut().state.txn_ctx;
                txn_ctx.set_for_update_ts(ts);
                let ts = txn_ctx.get_for_update_ts();
                let rc = self.rc();
                rc.latest_oracle_ts = ts;
                rc.latest_oracle_ts_valid = true;
                ts
            }
            None => unreachable!("prepared above"),
        };
        sctx.get_session_vars_mut().duration_wait_ts += start.elapsed().as_nanos() as i64;

        sctx.txn().set_option(SnapshotTS, Box::new(ts));
        self.rc().stmt_ts = ts;
        Ok(ts)
    }

    /// handleAfterQueryError will be called when the handle point is `StmtErrAfterQuery`.
    /// At this point the query will be retried if the error is a write conflict of a read
    /// with ts checking (never set here: `tidb_rc_read_check_ts` is rejected).
    // go: PessimisticRCTxnContextProvider.handleAfterQueryError L2 (no metrics counter, D14.1)
    fn rc_handle_after_query_error(
        &mut self,
        sctx: &mut dyn SessionCtx,
        query_err: Error,
    ) -> (StmtErrorAction, Result<(), Error>) {
        let vars = sctx.get_session_vars();
        if !tidb_kv::error::ErrWriteConflict.equal(&query_err) || !vars.stmt_ctx().rc_check_ts {
            return no_idea();
        }

        log::info!(
            "RC read with ts checking has failed, retry RC read, sql: {}, error: {query_err}",
            vars.stmt_ctx().original_sql
        );
        retry_ready()
    }

    /// AdviseWarmup provides warmup for inner state
    // go: PessimisticRCTxnContextProvider.AdviseWarmup L1
    pub(crate) fn rc_advise_warmup(&mut self, sctx: &mut dyn SessionCtx) -> Result<(), Error> {
        self.prepare_txn(sctx)?;

        if !self.is_tidb_snapshot_enabled(sctx) {
            self.rc_prepare_stmt_ts(sctx);
        }

        Ok(())
    }

    /// AdviseOptimizeWithPlan in read-committed covers as many cases as repeatable-read. We
    /// do not fetch latest ts immediately for such scenes. 1. A query like the form of
    /// "SELECT ... FOR UPDATE" whose execution plan is "PointGet". 2. An INSERT statement
    /// without "SELECT" subquery. 3. A UPDATE statement whose sub execution plan is
    /// "PointGet". 4. A DELETE statement whose sub execution plan is "PointGet".
    // go: PessimisticRCTxnContextProvider.AdviseOptimizeWithPlan L1
    pub(crate) fn rc_advise_optimize_with_plan(
        &mut self,
        sctx: &mut dyn SessionCtx,
        val: Option<&dyn PlanInfo>,
    ) -> Result<(), Error> {
        if self.is_tidb_snapshot_enabled(sctx) || self.is_begin_stmt_with_stale_read(sctx) {
            return Ok(());
        }
        let rc = self.rc();
        if rc.stmt_use_start_ts || !rc.latest_oracle_ts_valid {
            return Ok(());
        }

        let Some(mut plan) = val else {
            return Ok(());
        };

        if let PlanKind::Execute(p) = plan.plan_kind() {
            plan = p;
        }

        let mut use_last_oracle_ts = false;
        if !retrying(sctx.get_session_vars()) {
            use_last_oracle_ts = plan_skip_get_tso_from_pd(sctx.get_session_vars(), plan, false);
        }

        if use_last_oracle_ts {
            let rc = self.rc();
            rc.check_ts_in_write_stmt = true;
            rc.stmt_ts_future = Some(StmtTsFuture::Constant(rc.latest_oracle_ts));
        }

        Ok(())
    }

    /// GetSnapshotWithStmtReadTS returns snapshot with read ts
    // go: PessimisticRCTxnContextProvider.GetSnapshotWithStmtReadTS L1
    pub(crate) async fn rc_get_snapshot_with_stmt_read_ts(
        &mut self,
        sctx: &mut dyn SessionCtx,
    ) -> Result<Arc<dyn Snapshot>, Error> {
        let snapshot = self.get_snapshot_with_stmt_for_update_ts_base(sctx).await?;

        if sctx.get_session_vars().stmt_ctx().rc_check_ts {
            snapshot.set_option(IsolationLevel, Box::new(RCCheckTS));
        }

        Ok(snapshot)
    }
}

/// planSkipGetTsoFromPD identifies the plans which don't need get newest ts from PD.
// go: planSkipGetTsoFromPD L1 [M-IFACE]
fn plan_skip_get_tso_from_pd(
    vars: &SessionVars,
    plan: &dyn PlanInfo,
    in_lock_or_write_stmt: bool,
) -> bool {
    match plan.plan_kind() {
        PlanKind::PointGet { lock } => vars.rc_write_check_ts && (lock || in_lock_or_write_stmt),
        PlanKind::Physical { children, is_lock } => {
            if children.is_empty() {
                return false;
            }
            for p in children {
                if !plan_skip_get_tso_from_pd(vars, p, is_lock || in_lock_or_write_stmt) {
                    return false;
                }
            }
            true
        }
        PlanKind::Update(sel) | PlanKind::Delete(sel) => plan_skip_get_tso_from_pd(vars, sel, true),
        PlanKind::Insert {
            has_select_plan,
            has_on_duplicate,
            is_replace,
        } => !has_select_plan && !has_on_duplicate && !is_replace,
        // A BatchPointGet is a base.PhysicalPlan without children in Go's switch.
        PlanKind::BatchPointGet { .. } | PlanKind::Execute(_) | PlanKind::Other => false,
    }
}
