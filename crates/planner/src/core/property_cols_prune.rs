//! go-file: pkg/planner/core/property_cols_prune.go
//!
//! `preparePossibleProperties`: the sort properties each logical operator can provide,
//! computed bottom up before FindBestTask (block D10.2). Only joins and aggregations keep
//! the result; for the operators of the first vertical slice the walk has no effect besides
//! its cost.

use std::sync::Arc;

use tidb_expression::Column;

use crate::arena::PlanId;
use crate::planctx::PlanCtx;

/// preparePossibleProperties traverses the plan tree by a post-order method,
/// recursively calls LogicalPlan PreparePossibleProperties interface.
// go: preparePossibleProperties L1 [M-PLAN]
pub fn prepare_possible_properties(ctx: &PlanCtx<'_>, lp: PlanId) -> Vec<Vec<Arc<Column>>> {
    let node = ctx.arena.get(lp);
    let mut children_properties = Vec::with_capacity(node.children().len());
    for &child in node.children() {
        children_properties.push(prepare_possible_properties(ctx, child));
    }
    node.prepare_possible_properties(&ctx.arena, children_properties)
}
