//! go-file: pkg/planner/core/property_cols_prune.go
//!
//! `preparePossibleProperties`: the sort properties each logical operator can provide,
//! computed bottom up before FindBestTask (block D10.2). Only joins and aggregations keep
//! the result; for the operators of the first vertical slice the walk has no effect besides
//! its cost.

use std::sync::Arc;

use tidb_expression::Column;

use crate::arena::{PlanId, with_detached};
use crate::planctx::PlanCtx;

/// preparePossibleProperties traverses the plan tree by a post-order method,
/// recursively calls LogicalPlan PreparePossibleProperties interface.
// go: preparePossibleProperties L1 [M-PLAN]
pub fn prepare_possible_properties(ctx: &mut PlanCtx<'_>, lp: PlanId) -> Vec<Vec<Arc<Column>>> {
    let child_ids: Vec<PlanId> = ctx.arena.get(lp).children().to_vec();
    let mut children_properties = Vec::with_capacity(child_ids.len());
    for child in child_ids {
        children_properties.push(prepare_possible_properties(ctx, child));
    }
    with_detached(ctx, lp, |node, ctx| {
        node.prepare_possible_properties(&ctx.arena, children_properties)
    })
}
