//! go-file: pkg/planner/core/operator/logicalop/logical_table_dual.go
//!
//! `LogicalTableDual`: a table of 0 or 1 rows. Note that sometimes Go does not set its schema
//! (`buildTableDual()`), which means outputting 0/1 row with zero column.

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_expression::{Column, ExprRef, get_used_list};

use crate::arena::PlanId;
use crate::base::LogicalPlan;
use crate::logicalop::{BaseLogicalPlan, LogicalSchemaProducer, new_base_logical_plan};
use crate::planctx::PlanCtx;
use crate::util::logicaltrace;
use crate::util::optimizetrace::LogicalOptimizeOp;

/// LogicalTableDual represents a dual table plan.
pub struct LogicalTableDual {
    /// The embedded `BaseLogicalPlan`.
    pub base: BaseLogicalPlan,
    /// The embedded `LogicalSchemaProducer`.
    pub sp: LogicalSchemaProducer,
    /// RowCount could only be 0 or 1.
    pub row_count: isize,
}

impl LogicalTableDual {
    /// Init initializes LogicalTableDual.
    // go: LogicalTableDual.Init L1 [M-SELF]
    pub fn init(ctx: &PlanCtx<'_>, offset: isize) -> LogicalTableDual {
        LogicalTableDual {
            base: new_base_logical_plan(ctx, "TableDual", offset),
            sp: LogicalSchemaProducer::new(None),
            row_count: 0,
        }
    }

    /// BuildKeyInfo implements base.LogicalPlan.<4th> interface.
    // go: LogicalTableDual.BuildKeyInfo L1
    pub fn build_key_info(&mut self, arena: &crate::PlanArena) {
        crate::logicalop::base_logical_plan::build_key_info(self, arena);
        if self.row_count == 1 {
            self.base.set_max_one_row(true);
        }
    }
}

// glue: stub of LogicalTableDual.HashCode, blocked-by:D14.2 (plancodec.TypeStringToPhysicalID, util/plancodec)
/// The boundary of the stubbed `LogicalTableDual.HashCode`.
pub fn table_dual_hash_code_boundary() -> Error {
    blocked(
        "D14.2",
        "plancodec.TypeStringToPhysicalID (LogicalTableDual.HashCode)",
    )
}

impl LogicalPlan for LogicalTableDual {
    crate::logical_plan_boilerplate!(TableDual, producer);

    /// ExplainInfo implements Plan interface.
    // go: LogicalTableDual.ExplainInfo L1
    fn explain_info(&self, _ctx: &PlanCtx<'_>) -> Result<String, Error> {
        Ok(format!("rowcount:{}", self.row_count))
    }

    /// HashCode implements base.LogicalPlan.<0th> interface.
    // glue: dispatch to the HashCode boundary stub above
    fn hash_code(&self) -> Result<Vec<u8>, Error> {
        Err(table_dual_hash_code_boundary())
    }

    /// PredicatePushDown implements base.LogicalPlan.<1st> interface.
    // go: LogicalTableDual.PredicatePushDown L1
    fn predicate_push_down(
        &mut self,
        _ctx: &mut PlanCtx<'_>,
        this: PlanId,
        predicates: Vec<ExprRef>,
        _opt: &LogicalOptimizeOp,
    ) -> Result<(Vec<ExprRef>, PlanId), Error> {
        Ok((predicates, this))
    }

    /// PruneColumns implements base.LogicalPlan.<2nd> interface.
    // go: LogicalTableDual.PruneColumns L1
    fn prune_columns(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        this: PlanId,
        parent_used_cols: Vec<Arc<Column>>,
        opt: &LogicalOptimizeOp,
    ) -> Result<PlanId, Error> {
        let ectx = ctx.eval_ctx();
        let used = get_used_list(&*ectx, &parent_used_cols, self.schema(&ctx.arena));
        let mut pruned_columns = Vec::new();
        for i in (0..used.len()).rev() {
            if !used[i] {
                pruned_columns.push(Arc::clone(&self.schema(&ctx.arena).columns[i]));
                self.schema_mut(&mut ctx.arena).columns.remove(i);
            }
        }
        logicaltrace::append_column_prune_trace_step(self, ctx, &pruned_columns, opt);
        Ok(this)
    }
}
