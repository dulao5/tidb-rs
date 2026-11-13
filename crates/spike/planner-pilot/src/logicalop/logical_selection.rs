//! go-file: pkg/planner/core/operator/logicalop/logical_selection.go (spike subset)

use std::sync::Arc;

use tidb_base::Error;
use tidb_expression::util::extract_columns_from_expressions;
use tidb_expression::{Column, ExprRef};

use crate::arena::PlanId;
use crate::base::{LogicalPlan, ReplaceMap, prune_columns};
use crate::logicalop::{BaseLogicalPlan, logical_plan_boilerplate};
use crate::optimizetrace::LogicalOptimizeOp;
use crate::planctx::PlanCtx;
use crate::rule_util::resolve_expr_and_replace;

/// LogicalSelection represents a where or having predicate.
pub struct LogicalSelection {
    /// BaseLogicalPlan (no own schema: `Schema()` is the child's).
    pub base: BaseLogicalPlan,
    /// Originally the WHERE or ON condition is parsed into a single expression,
    /// but after we converted to CNF(Conjunctive normal form), it can be
    /// split into a list of AND conditions.
    pub conditions: Vec<ExprRef>,
}

impl LogicalSelection {
    /// Init initializes LogicalSelection.
    // go: pkg/planner/core/operator/logicalop/logical_selection.go LogicalSelection.Init L1 [M-PLAN]
    pub fn init(conditions: Vec<ExprRef>, ctx: &PlanCtx<'_>, qb_offset: i32) -> LogicalSelection {
        LogicalSelection {
            base: BaseLogicalPlan::new(ctx, "Selection", qb_offset),
            conditions,
        }
    }
}

impl LogicalPlan for LogicalSelection {
    logical_plan_boilerplate!(Selection);

    /// ReplaceExprColumns implements base.LogicalPlan interface.
    // go: pkg/planner/core/operator/logicalop/logical_selection.go (*LogicalSelection).ReplaceExprColumns L1 [M-EXPR]
    fn replace_expr_columns(&mut self, replace: &ReplaceMap) {
        for expr in self.conditions.iter_mut() {
            resolve_expr_and_replace(expr, replace);
        }
    }

    /// PruneColumns implements base.LogicalPlan.<2nd> interface.
    // go: pkg/planner/core/operator/logicalop/logical_selection.go (*LogicalSelection).PruneColumns L1 [M-PLAN]
    fn prune_columns(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        this: PlanId,
        mut parent_used_cols: Vec<Arc<Column>>,
        opt: &mut LogicalOptimizeOp,
    ) -> Result<PlanId, Error> {
        let child = self.children()[0];
        parent_used_cols =
            extract_columns_from_expressions(parent_used_cols, &self.conditions, None);
        self.children_mut()[0] = prune_columns(ctx, child, parent_used_cols, opt)?;
        Ok(this)
    }
}
