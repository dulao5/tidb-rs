//! go-file: pkg/planner/core/operator/logicalop/logical_top_n.go (spike subset)

use std::sync::Arc;

use tidb_base::Error;
use tidb_expression::Column;

use crate::arena::PlanId;
use crate::base::{LogicalPlan, ReplaceMap, prune_columns};
use crate::logicalop::logical_plans_misc::prune_by_items;
use crate::logicalop::{BaseLogicalPlan, logical_plan_boilerplate};
use crate::optimizetrace::LogicalOptimizeOp;
use crate::planctx::PlanCtx;
use crate::rule_util::resolve_expr_and_replace;
use crate::util::ByItems;

/// LogicalTopN represents a top-n plan (no own schema: `Schema()` is the child's).
pub struct LogicalTopN {
    /// BaseLogicalPlan.
    pub base: BaseLogicalPlan,
    /// ByItems.
    pub by_items: Vec<ByItems>,
}

impl LogicalTopN {
    /// Init initializes LogicalTopN.
    // go: pkg/planner/core/operator/logicalop/logical_top_n.go LogicalTopN.Init L1 [M-PLAN]
    pub fn init(by_items: Vec<ByItems>, ctx: &PlanCtx<'_>, qb_offset: i32) -> LogicalTopN {
        LogicalTopN {
            base: BaseLogicalPlan::new(ctx, "TopN", qb_offset),
            by_items,
        }
    }
}

impl LogicalPlan for LogicalTopN {
    logical_plan_boilerplate!(TopN);

    /// ReplaceExprColumns implements base.LogicalPlan interface.
    // go: pkg/planner/core/operator/logicalop/logical_top_n.go (*LogicalTopN).ReplaceExprColumns L1 [M-EXPR]
    fn replace_expr_columns(&mut self, replace: &ReplaceMap) {
        for by_item in self.by_items.iter_mut() {
            resolve_expr_and_replace(&mut by_item.expr, replace);
        }
    }

    /// PruneColumns implements base.LogicalPlan.<2nd> interface.
    /// If any expression can view as a constant in execution stage, such as correlated column,
    /// constant, we do prune them. Note that we can't prune the expressions contain
    /// non-deterministic functions, such as rand().
    // go: pkg/planner/core/operator/logicalop/logical_top_n.go (*LogicalTopN).PruneColumns L1 [M-PLAN]
    fn prune_columns(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        this: PlanId,
        mut parent_used_cols: Vec<Arc<Column>>,
        opt: &mut LogicalOptimizeOp,
    ) -> Result<PlanId, Error> {
        let cols;
        (self.by_items, cols) = prune_by_items(self, &*ctx.eval_ctx(), &self.by_items, opt);
        parent_used_cols.extend(cols);
        self.children_mut()[0] = prune_columns(ctx, self.children()[0], parent_used_cols, opt)?;
        Ok(this)
    }
}
