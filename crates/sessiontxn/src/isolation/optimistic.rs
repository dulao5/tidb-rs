//! go-file: pkg/sessiontxn/isolation/optimistic.go
//!
//! `onTxnActive` and `isOptimisticTxnRetryable` set `TxnCtx.CouldRetry`, read only by the
//! optimistic commit retry (write path, not ported).

use futures::FutureExt;
use futures::future::BoxFuture;
use tidb_base::Error;

use super::base::{BaseTxnContextProvider, Kind};
use crate::sctx::{PlanInfo, PlanKind, SessionCtx};

/// OptimisticTxnContextProvider provides txn context for optimistic transaction (the
/// [`Kind::Optimistic`] provider).
pub type OptimisticTxnContextProvider = BaseTxnContextProvider;

impl BaseTxnContextProvider {
    /// ResetForNewTxn resets OptimisticTxnContextProvider to an initial state for a new txn
    /// (here: a new provider in that state).
    // go: OptimisticTxnContextProvider.ResetForNewTxn L2 (returns a fresh provider instead of resetting one in place)
    pub fn reset_for_new_txn(causal_consistency_only: bool) -> OptimisticTxnContextProvider {
        BaseTxnContextProvider::with_kind(
            causal_consistency_only,
            Kind::Optimistic {
                optimize_with_max_ts: false,
            },
        )
    }

    /// GetStmtReadTS returns the read timestamp used by select statement (not for select ...
    /// for update)
    // go: OptimisticTxnContextProvider.GetStmtReadTS L1
    pub(crate) fn optimistic_get_stmt_read_ts<'a>(
        &'a mut self,
        sctx: &'a mut dyn SessionCtx,
    ) -> BoxFuture<'a, Result<u64, Error>> {
        async move {
            // If `math.MaxUint64` is used for point get optimization, it is not necessary to
            // activate the txn. Just return `math.MaxUint64` to save the performance.
            if let Kind::Optimistic {
                optimize_with_max_ts: true,
            } = self.kind
            {
                return Ok(u64::MAX);
            }
            self.get_stmt_read_ts_base(sctx).await
        }
        .boxed()
    }

    /// GetStmtForUpdateTS returns the read timestamp used by select statement (not for
    /// select ... for update)
    // go: OptimisticTxnContextProvider.GetStmtForUpdateTS L1
    pub async fn optimistic_get_stmt_for_update_ts(
        &mut self,
        sctx: &mut dyn SessionCtx,
    ) -> Result<u64, Error> {
        if let Kind::Optimistic {
            optimize_with_max_ts: true,
        } = self.kind
        {
            return Ok(u64::MAX);
        }
        self.get_stmt_for_update_ts_base(sctx).await
    }

    /// AdviseOptimizeWithPlan providers optimization according to the plan It will use
    /// MaxTS as the startTS in autocommit txn for some plans.
    // go: OptimisticTxnContextProvider.AdviseOptimizeWithPlan L1
    pub(crate) fn optimistic_advise_optimize_with_plan(
        &mut self,
        sctx: &mut dyn SessionCtx,
        plan: Option<&dyn PlanInfo>,
    ) -> Result<(), Error> {
        let optimize_with_max_ts = matches!(
            self.kind,
            Kind::Optimistic {
                optimize_with_max_ts: true
            }
        );
        if optimize_with_max_ts
            || self.is_tidb_snapshot_enabled(sctx)
            || self.is_begin_stmt_with_stale_read(sctx)
        {
            return Ok(());
        }

        if self.txn_active {
            // `p.txn != nil` means the txn has already been activated, we should not
            // optimize the startTS because the startTS has already been used.
            return Ok(());
        }

        let Some(mut real_plan) = plan else {
            return Ok(());
        };

        if let PlanKind::Execute(p) = real_plan.plan_kind() {
            real_plan = p;
        }

        let ok =
            real_plan.is_point_get_with_pk_or_unique_key_by_auto_commit(sctx.get_session_vars());

        if ok {
            let vars = sctx.get_session_vars();
            log::debug!(
                "init txnStartTS with MaxUint64, conn: {}, text: {}",
                vars.connection_id,
                vars.stmt_ctx().original_sql
            );

            if let Err(err) = self.force_prepare_const_start_ts(u64::MAX) {
                log::error!(
                    "failed init txnStartTS with MaxUint64, error: {err}, conn: {}, text: {}",
                    vars.connection_id,
                    vars.stmt_ctx().original_sql
                );
                return Err(err);
            }

            self.kind = Kind::Optimistic {
                optimize_with_max_ts: true,
            };
            let sc = sctx.get_session_vars_mut().stmt_ctx_mut();
            if sc.priority == tidb_base::mysql::NoPriority {
                sc.priority = tidb_base::mysql::PriorityEnum(tidb_kv::kv::PriorityHigh as _);
            }
        }
        Ok(())
    }
}
