//! go-file: pkg/planner/core/rule_column_pruning.go
//!
//! The column pruning rule (block D09.7).

use tidb_base::Error;

use crate::arena::PlanId;
use crate::base::{self, LogicalKind, LogicalOptRule};
use crate::planctx::PlanCtx;
use crate::util::optimizetrace::LogicalOptimizeOp;

/// ColumnPruner is used to prune unnecessary columns.
pub struct ColumnPruner;

impl LogicalOptRule for ColumnPruner {
    /// Optimize implements base.LogicalOptRule.<0th> interface.
    // go: ColumnPruner.Optimize L1 [M-PLAN]
    fn optimize(
        &self,
        ctx: &mut PlanCtx<'_>,
        lp: PlanId,
        opt: &LogicalOptimizeOp,
    ) -> Result<(PlanId, bool), Error> {
        let plan_changed = false;
        let cols = ctx.arena.get(lp).schema(&ctx.arena).columns.clone();
        let lp = base::prune_columns(ctx, lp, cols, opt)?;
        // intest.AssertFunc: test builds only.
        debug_assert!(
            no_zero_column_lay_out(ctx, lp),
            "After column pruning, some operator got zero row output. Please fix it."
        );
        Ok((lp, plan_changed))
    }

    /// Name implements base.LogicalOptRule.<1st> interface.
    // go: ColumnPruner.Name L1
    fn name(&self) -> &'static str {
        "column_prune"
    }
}

// go: noZeroColumnLayOut L1 [M-PLAN]
fn no_zero_column_lay_out(ctx: &PlanCtx<'_>, p: PlanId) -> bool {
    let node = ctx.arena.get(p);
    for &child in node.children() {
        if !no_zero_column_lay_out(ctx, child) {
            return false;
        }
    }
    if node.schema(&ctx.arena).is_empty() {
        // The p don't hold its schema. So we don't need check itself.
        // (Go compares the schema pointers with the child's: an operator without a schema
        // of its own returns the child's.)
        if !node.children().is_empty() && node.schema_producer().is_none() {
            return true;
        }
        if node.kind() != LogicalKind::TableDual {
            return false;
        }
    }
    true
}
