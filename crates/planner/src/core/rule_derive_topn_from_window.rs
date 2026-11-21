//! go-file: pkg/planner/core/rule_derive_topn_from_window.go
//!
//! The rule wrapper of `DeriveTopN` (block D09.11): the plan builder sets its flag for
//! every query with a WHERE clause, so the optimizer driver (D09.13) runs it on the plans
//! of the first vertical slice. The window checks (`checkPartitionBy`, `windowIsTopN`) need
//! `LogicalWindow` (D09.4) and come with it; `LogicalSelection.DeriveTopN` reaches them only
//! over a window.

use tidb_base::Error;

use crate::arena::PlanId;
use crate::base::{self, LogicalOptRule};
use crate::planctx::PlanCtx;
use crate::util::optimizetrace::LogicalOptimizeOp;

/// DeriveTopNFromWindow pushes down the topN or limit. In the future we will remove the limit from `requiredProperty` in CBO phase.
#[derive(Clone, Copy, Debug, Default)]
pub struct DeriveTopNFromWindow;

impl LogicalOptRule for DeriveTopNFromWindow {
    /// Optimize implements base.LogicalOptRule.<0th> interface.
    // go: DeriveTopNFromWindow.Optimize L1
    fn optimize(
        &self,
        ctx: &mut PlanCtx<'_>,
        p: PlanId,
        opt: &LogicalOptimizeOp,
    ) -> Result<(PlanId, bool), Error> {
        let plan_changed = false;
        Ok((base::derive_top_n(ctx, p, opt)?, plan_changed))
    }

    /// Name implements base.LogicalOptRule.<1st> interface.
    // go: DeriveTopNFromWindow.Name L1
    fn name(&self) -> &'static str {
        "derive_topn_from_window"
    }
}
