//! go-file: pkg/planner/core/operator/logicalop/logical_limit.go (spike subset)

use std::sync::Arc;

use tidb_base::Error;
use tidb_expression::Column;

use crate::arena::PlanId;
use crate::base::{LogicalPlan, prune_columns};
use crate::logicalop::{BaseLogicalPlan, LogicalSchemaProducer, logical_plan_boilerplate};
use crate::optimizetrace::LogicalOptimizeOp;
use crate::planctx::PlanCtx;

/// LogicalLimit represents offset and limit plan.
pub struct LogicalLimit {
    /// BaseLogicalPlan.
    pub base: BaseLogicalPlan,
    /// LogicalSchemaProducer (nil schema at build: cloned from the child on first read).
    pub sp: LogicalSchemaProducer,
    /// Offset.
    pub offset: u64,
    /// Count.
    pub count: u64,
}

impl LogicalLimit {
    /// Init initializes LogicalLimit.
    // go: pkg/planner/core/operator/logicalop/logical_limit.go LogicalLimit.Init L1 [M-PLAN]
    pub fn init(offset: u64, count: u64, ctx: &PlanCtx<'_>, qb_offset: i32) -> LogicalLimit {
        LogicalLimit {
            base: BaseLogicalPlan::new(ctx, "Limit", qb_offset),
            sp: LogicalSchemaProducer::new(None),
            offset,
            count,
        }
    }
}

impl LogicalPlan for LogicalLimit {
    logical_plan_boilerplate!(Limit, producer);

    /// PruneColumns implements base.LogicalPlan.<2nd> interface.
    // go: pkg/planner/core/operator/logicalop/logical_limit.go (*LogicalLimit).PruneColumns L1 [M-PLAN]
    fn prune_columns(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        this: PlanId,
        parent_used_cols: Vec<Arc<Column>>,
        opt: &mut LogicalOptimizeOp,
    ) -> Result<PlanId, Error> {
        let saved_used_cols = parent_used_cols.clone();
        self.children_mut()[0] = prune_columns(ctx, self.children()[0], parent_used_cols, opt)?;
        self.sp.set_schema(None);
        self.inline_projection(ctx, saved_used_cols, opt);
        Ok(this)
    }
}
