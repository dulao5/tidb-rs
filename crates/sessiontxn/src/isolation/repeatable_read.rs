//! go-file: pkg/sessiontxn/isolation/repeatable_read.go
//!
//! The read path of the pessimistic repeatable-read provider: reads use the transaction's
//! start ts. The for-update ts (`getForUpdateTs`, `updateForUpdateTS`) and the pessimistic
//! lock error handling (`handleAfterPessimisticLockError`) are the write path: tidb-rs
//! rejects writes and locking reads in transactions (N11 D8), so the provider keeps no
//! for-update state.

use tidb_base::Error;

use super::base::{BaseTxnContextProvider, Kind};
use crate::sctx::{PlanInfo, PlanKind, SessionCtx};
use crate::{StmtErrorAction, StmtErrorHandlePoint, error_action, no_idea};

/// PessimisticRRTxnContextProvider provides txn context for isolation level repeatable-read
/// (the [`Kind::PessimisticRR`] provider).
pub type PessimisticRRTxnContextProvider = BaseTxnContextProvider;

/// NewPessimisticRRTxnContextProvider returns a new PessimisticRRTxnContextProvider
// go: NewPessimisticRRTxnContextProvider L1
pub fn new_pessimistic_rr_txn_context_provider(
    causal_consistency_only: bool,
) -> PessimisticRRTxnContextProvider {
    BaseTxnContextProvider::with_kind(
        causal_consistency_only,
        Kind::PessimisticRR {
            optimize_for_not_fetching_latest_ts: false,
        },
    )
}

impl BaseTxnContextProvider {
    /// OnStmtStart is the hook that should be called when a new statement started. (Go
    /// also clears `forUpdateTS`, write path.)
    // go: PessimisticRRTxnContextProvider.OnStmtStart L2 (no for-update ts: write path)
    pub(crate) fn rr_on_stmt_start(&mut self) -> Result<(), Error> {
        self.on_stmt_start_base()?;
        if let Kind::PessimisticRR {
            optimize_for_not_fetching_latest_ts,
        } = &mut self.kind
        {
            *optimize_for_not_fetching_latest_ts = false;
        }
        Ok(())
    }

    /// OnStmtRetry is the hook that should be called when a statement retry. (Go also
    /// moves `forUpdateTS` to the latest one, write path.)
    // go: PessimisticRRTxnContextProvider.OnStmtRetry L2 (no for-update ts: write path)
    pub(crate) fn rr_on_stmt_retry(&mut self) -> Result<(), Error> {
        self.on_stmt_retry_base()?;
        if let Kind::PessimisticRR {
            optimize_for_not_fetching_latest_ts,
        } = &mut self.kind
        {
            *optimize_for_not_fetching_latest_ts = false;
        }
        Ok(())
    }

    /// OnStmtErrorForNextAction of the pessimistic providers: the errors after a
    /// pessimistic lock come from locking statements, which a read-only node does not run,
    /// so they are returned (`handleAfterPessimisticLockError` is the write path).
    // go: PessimisticRRTxnContextProvider.OnStmtErrorForNextAction L2 (lock errors are returned: write path)
    pub(crate) fn pessimistic_on_stmt_error_for_next_action(
        &mut self,
        point: StmtErrorHandlePoint,
        err: Error,
    ) -> (StmtErrorAction, Result<(), Error>) {
        match point {
            StmtErrorHandlePoint::StmtErrAfterPessimisticLock => error_action(err),
            _ => no_idea(),
        }
    }

    /// AdviseOptimizeWithPlan optimizes for update point get related execution. Use case:
    /// In for update point get related operations, we do not fetch ts from PD but use the
    /// last ts we fetched. We expect that the data that the point get acquires has not been
    /// changed. Benefit: Save the cost of acquiring ts from PD. Drawbacks: If the data has
    /// been changed since the ts we used, we need to retry. One exception is insert
    /// operation, when it has no select plan, we do not fetch the latest ts immediately. We
    /// only update ts if write conflict is incurred.
    // go: PessimisticRRTxnContextProvider.AdviseOptimizeWithPlan L1
    pub(crate) fn rr_advise_optimize_with_plan(
        &mut self,
        sctx: &mut dyn SessionCtx,
        val: Option<&dyn PlanInfo>,
    ) -> Result<(), Error> {
        if self.is_tidb_snapshot_enabled(sctx) || self.is_begin_stmt_with_stale_read(sctx) {
            return Ok(());
        }

        let Some(mut plan) = val else {
            return Ok(());
        };

        if let PlanKind::Execute(p) = plan.plan_kind() {
            plan = p;
        }

        let v = not_need_get_latest_ts_from_pd(plan, false);
        if let Kind::PessimisticRR {
            optimize_for_not_fetching_latest_ts,
        } = &mut self.kind
        {
            *optimize_for_not_fetching_latest_ts = v;
        }
        Ok(())
    }
}

/// notNeedGetLatestTSFromPD searches for optimization condition recursively Note: For
/// PessimisticSelectForUpdate, we do not fetch the latest ts from PD.
// go: notNeedGetLatestTSFromPD L1 [M-IFACE]
pub fn not_need_get_latest_ts_from_pd(plan: &dyn PlanInfo, in_lock_or_write_stmt: bool) -> bool {
    match plan.plan_kind() {
        PlanKind::PointGet { lock } | PlanKind::BatchPointGet { lock } => {
            !in_lock_or_write_stmt || lock
        }
        PlanKind::Physical { children, is_lock } => {
            if children.is_empty() {
                return false;
            }
            for p in children {
                if !not_need_get_latest_ts_from_pd(p, is_lock || in_lock_or_write_stmt) {
                    return false;
                }
            }
            true
        }
        PlanKind::Update(sel) | PlanKind::Delete(sel) => not_need_get_latest_ts_from_pd(sel, true),
        PlanKind::Insert {
            has_select_plan, ..
        } => !has_select_plan,
        PlanKind::Execute(_) | PlanKind::Other => false,
    }
}
