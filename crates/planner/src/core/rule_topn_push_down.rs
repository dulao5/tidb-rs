//! go-file: pkg/planner/core/rule_topn_push_down.go
//!
//! The TopN / Limit push down rule (block D09.11).

use tidb_base::Error;

use crate::arena::PlanId;
use crate::base::{self, LogicalOptRule};
use crate::planctx::PlanCtx;
use crate::util::optimizetrace::LogicalOptimizeOp;

/// PushDownTopNOptimizer pushes down the topN or limit. In the future we will remove the
/// limit from `requiredProperty` in CBO phase.
pub struct PushDownTopNOptimizer;

impl LogicalOptRule for PushDownTopNOptimizer {
    /// Optimize implements the base.LogicalOptRule.<0th> interface.
    // go: PushDownTopNOptimizer.Optimize L1
    fn optimize(
        &self,
        ctx: &mut PlanCtx<'_>,
        p: PlanId,
        opt: &LogicalOptimizeOp,
    ) -> Result<(PlanId, bool), Error> {
        let plan_changed = false;
        Ok((base::push_down_top_n(ctx, p, None, opt)?, plan_changed))
    }

    /// Name implements the base.LogicalOptRule.<1st> interface.
    // go: PushDownTopNOptimizer.Name L1
    fn name(&self) -> &'static str {
        "topn_push_down"
    }
}
