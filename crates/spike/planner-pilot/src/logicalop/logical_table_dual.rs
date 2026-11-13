//! go-file: pkg/planner/core/operator/logicalop/logical_table_dual.go (spike subset)

use std::sync::Arc;

use tidb_base::Error;
use tidb_expression::{Column, Schema, get_used_list};

use crate::arena::PlanId;
use crate::base::LogicalPlan;
use crate::logicalop::{BaseLogicalPlan, LogicalSchemaProducer, logical_plan_boilerplate};
use crate::optimizetrace::{LogicalOptimizeOp, logicaltrace};
use crate::planctx::PlanCtx;

/// LogicalTableDual represents a dual table plan.
pub struct LogicalTableDual {
    /// BaseLogicalPlan.
    pub base: BaseLogicalPlan,
    /// LogicalSchemaProducer.
    pub sp: LogicalSchemaProducer,
    /// RowCount could only be 0 or 1.
    pub row_count: i32,
}

impl LogicalTableDual {
    /// Init initializes LogicalTableDual (plus `SetSchema`).
    // go: pkg/planner/core/operator/logicalop/logical_table_dual.go LogicalTableDual.Init L1 [M-PLAN]
    pub fn init(
        row_count: i32,
        schema: Schema,
        ctx: &PlanCtx<'_>,
        qb_offset: i32,
    ) -> LogicalTableDual {
        LogicalTableDual {
            base: BaseLogicalPlan::new(ctx, "TableDual", qb_offset),
            sp: LogicalSchemaProducer::new(Some(schema)),
            row_count,
        }
    }
}

impl LogicalPlan for LogicalTableDual {
    logical_plan_boilerplate!(TableDual, producer);

    /// PruneColumns implements base.LogicalPlan.<2nd> interface.
    // go: pkg/planner/core/operator/logicalop/logical_table_dual.go (*LogicalTableDual).PruneColumns L1 [M-PLAN]
    fn prune_columns(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        this: PlanId,
        parent_used_cols: Vec<Arc<Column>>,
        opt: &mut LogicalOptimizeOp,
    ) -> Result<PlanId, Error> {
        let used = get_used_list(&*ctx.eval_ctx(), &parent_used_cols, self.schema(&ctx.arena));
        let mut pruned_columns = Vec::new();
        for i in (0..used.len()).rev() {
            if !used[i] {
                pruned_columns.push(self.schema(&ctx.arena).columns[i].clone());
                self.schema_mut(&mut ctx.arena).columns.remove(i);
            }
        }
        logicaltrace::append_column_prune_trace_step(self, &pruned_columns, opt);
        Ok(this)
    }
}
