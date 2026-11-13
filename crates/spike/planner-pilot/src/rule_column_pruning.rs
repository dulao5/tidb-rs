//! go-file: pkg/planner/core/rule_column_pruning.go

use tidb_base::Error;

use crate::arena::{PlanArena, PlanId};
use crate::base::{LogicalKind, LogicalOptRule, prune_columns};
use crate::optimizetrace::LogicalOptimizeOp;
use crate::planctx::PlanCtx;

/// ColumnPruner is used to prune unnecessary columns.
#[derive(Clone, Copy, Debug, Default)]
pub struct ColumnPruner;

impl LogicalOptRule for ColumnPruner {
    /// Optimize implements base.LogicalOptRule.<0th> interface.
    // go: pkg/planner/core/rule_column_pruning.go (*ColumnPruner).Optimize L1 [M-PLAN]
    fn optimize(
        &self,
        ctx: &mut PlanCtx<'_>,
        lp: PlanId,
        opt: &mut LogicalOptimizeOp,
    ) -> Result<(PlanId, bool), Error> {
        let plan_changed = false;
        let cols = ctx.arena.get(lp).schema(&ctx.arena).columns.clone();
        let lp = prune_columns(ctx, lp, cols, opt)?;
        debug_assert!(
            no_zero_column_lay_out(&ctx.arena, lp),
            "After column pruning, some operator got zero row output. Please fix it."
        );
        Ok((lp, plan_changed))
    }

    /// Name implements base.LogicalOptRule.<1st> interface.
    // go: pkg/planner/core/rule_column_pruning.go (*ColumnPruner).Name L1
    fn name(&self) -> &'static str {
        "column_prune"
    }
}

/// noZeroColumnLayOut. Go compares `p.Schema() == p.Children()[0].Schema()` by pointer;
/// `BaseLogicalPlan::schema` returns the child's schema by reference, so pointer equality
/// of the references is the same test.
// go: pkg/planner/core/rule_column_pruning.go noZeroColumnLayOut L1 [M-PLAN, M-IFACE]
pub fn no_zero_column_lay_out(arena: &PlanArena, p: PlanId) -> bool {
    let node = arena.get(p);
    for &child in node.children() {
        if !no_zero_column_lay_out(arena, child) {
            return false;
        }
    }
    if node.schema(arena).is_empty() {
        // The p don't hold its schema. So we don't need check itself.
        if !node.children().is_empty()
            && std::ptr::eq(
                node.schema(arena),
                arena.get(node.children()[0]).schema(arena),
            )
        {
            return true;
        }
        if node.kind() != LogicalKind::TableDual {
            return false;
        }
    }
    true
}
