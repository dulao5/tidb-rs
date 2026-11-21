//! go-file: pkg/planner/core/operator/logicalop/logical_plans_misc.go
//!
//! Helpers shared by the logical operators. Go's type switches over `base.LogicalPlan` are
//! matches on [`LogicalKind`] (`M-IFACE`); the operators not ported yet (join, aggregation,
//! window, ...) have no kind, so their arms are written as comments where Go has them.

use std::collections::HashSet;
use std::sync::Arc;

use tidb_base::kv::{self, StoreType};
use tidb_base::mysql::TypeNull;
use tidb_expression::constant_propagation::propagate_constant;
use tidb_expression::util::{extract_columns, is_runtime_const_expr};
use tidb_expression::{Column, ExprRef};
use tidb_model::TableCacheStatusDisable;

use crate::arena::PlanId;
use crate::base::{self, LogicalKind, LogicalPlan};
use crate::core::constraint::delete_true_exprs;
use crate::logicalop::logical_join::{append_add_selection_trace_step, conds2_table_dual};
use crate::logicalop::logical_selection::append_table_dual_trace_step;
use crate::logicalop::{DataSource, LogicalSelection, LogicalTopN};
use crate::planctx::PlanCtx;
use crate::util::ByItems;
use crate::util::logicaltrace::{self, trace_id};
use crate::util::optimizetrace::LogicalOptimizeOp;

/// HasMaxOneRow returns if the LogicalPlan will output at most one row.
// go: HasMaxOneRow L1 [M-IFACE]
pub fn has_max_one_row(p: LogicalKind, child_max_one_row: &[bool]) -> bool {
    if child_max_one_row.is_empty() {
        // The reason why we use this check is that, this function
        // is used both in planner/core and planner/cascades.
        // In cascades planner, LogicalPlan may have no `children`.
        return false;
    }
    match p {
        // *LogicalLock, *LogicalApply, *LogicalWindow, *LogicalAggregation: not ported yet.
        LogicalKind::Limit
        | LogicalKind::Sort
        | LogicalKind::Selection
        | LogicalKind::Projection => child_max_one_row[0],
        // *LogicalMaxOneRow: true; *LogicalJoin: by join type (not ported yet).
        _ => false,
    }
}

/// AddSelection adds a LogicalSelection to the given LogicalPlan. `p` is the detached
/// operator whose child `ch_idx` is set.
// go: AddSelection L1 [M-PLAN]
pub fn add_selection<P: LogicalPlan + ?Sized>(
    p: &mut P,
    ctx: &mut PlanCtx<'_>,
    child: PlanId,
    mut conditions: Vec<ExprRef>,
    ch_idx: usize,
    opt: &LogicalOptimizeOp,
) {
    if conditions.is_empty() {
        p.children_mut()[ch_idx] = child;
        return;
    }
    conditions = propagate_constant(&ctx.expr_ctx, &conditions);
    // Return table dual when filter is constant false or null.
    let child_node = ctx.arena.get(child);
    let (child_schema, child_qb) = (
        child_node.schema(&ctx.arena).clone(),
        child_node.query_block_offset(),
    );
    let child_tid = trace_id(child_node);
    if let Some(dual) = conds2_table_dual(ctx, child_schema, child_qb, &conditions) {
        p.children_mut()[ch_idx] = dual;
        append_table_dual_trace_step(ctx, child_tid, dual, &conditions, opt);
        return;
    }

    conditions = delete_true_exprs(ctx, &conditions);
    if conditions.is_empty() {
        p.children_mut()[ch_idx] = child;
        return;
    }
    let mut selection = LogicalSelection::init(conditions, ctx, p.query_block_offset());
    selection.set_children(vec![child]);
    let sel_tid = trace_id(&selection);
    let selection = ctx.arena.alloc(Box::new(selection));
    p.children_mut()[ch_idx] = selection;
    append_add_selection_trace_step(trace_id(&*p), child_tid, sel_tid, opt);
}

/// pushDownTopNForBaseLogicalPlan can be moved when LogicalTopN has been moved to logicalop.
// go: pushDownTopNForBaseLogicalPlan L1 [M-SELF, M-PLAN]
pub fn push_down_top_n_for_base_logical_plan<P: LogicalPlan + ?Sized>(
    p: &mut P,
    ctx: &mut PlanCtx<'_>,
    this: PlanId,
    top_n: Option<PlanId>,
    opt: &LogicalOptimizeOp,
) -> PlanId {
    for i in 0..p.children().len() {
        let child = p.children()[i];
        p.children_mut()[i] = base::push_down_top_n(ctx, child, None, opt);
    }
    if let Some(top_n) = top_n {
        return LogicalTopN::attach_child_to(ctx, top_n, this, p, opt);
    }
    this
}

/// pruneByItems: drops duplicated and constant by-items and returns the columns the kept
/// ones use. `p` is the operator (for the trace).
// go: pruneByItems L1
pub fn prune_by_items<P: LogicalPlan + ?Sized>(
    p: &P,
    ctx: &PlanCtx<'_>,
    old: &[ByItems],
    opt: &LogicalOptimizeOp,
) -> (Vec<ByItems>, Vec<Arc<Column>>) {
    let ectx = ctx.eval_ctx();
    let mut parent_used_cols = Vec::new();
    let mut pruned_by_items = Vec::new();
    let mut by_items = Vec::with_capacity(old.len());
    let mut seen: HashSet<Vec<u8>> = HashSet::with_capacity(old.len());
    for by_item in old {
        let mut pruned = true;
        let hash = by_item.expr.hash_code().to_vec();
        let hash_match = !seen.insert(hash);
        let cols = extract_columns(&by_item.expr);
        if !hash_match {
            if cols.is_empty() {
                if !is_runtime_const_expr(&by_item.expr) {
                    pruned = false;
                    by_items.push(by_item.clone());
                }
            } else if by_item.expr.get_type(&*ectx).get_type() != TypeNull {
                pruned = false;
                parent_used_cols.extend(cols);
                by_items.push(by_item.clone());
            }
        }
        if pruned {
            pruned_by_items.push(by_item.clone());
        }
    }
    logicaltrace::append_by_items_prune_trace_step(p, ctx, &pruned_by_items, opt);
    (by_items, parent_used_cols)
}

/// CanPushToCopImpl checks whether the logical plan can be pushed to coprocessor.
// go: CanPushToCopImpl L1 [M-IFACE]
pub fn can_push_to_cop_impl<P: LogicalPlan + ?Sized>(
    p: &P,
    ctx: &PlanCtx<'_>,
    store_tp: StoreType,
    consider_dual: bool,
) -> bool {
    let mut ret = true;
    for &ch in p.children() {
        let c = ctx.arena.get(ch);
        match c.kind() {
            LogicalKind::DataSource => {
                let c = c.as_any().downcast_ref::<DataSource>().expect("DataSource");
                let mut valid_ds = false;
                let mut index_merge_is_intersection = false;
                // since CanPushToCopImpl is only used in physical enumeration of physical plan phase.
                // we definitely here should use the specific PossibleAccessPaths for each DS alternative.
                for path in &c.possible_access_paths {
                    if path.store_type == store_tp {
                        valid_ds = true;
                    }
                    if !path.partial_index_paths.is_empty() && path.index_merge_is_intersection {
                        index_merge_is_intersection = true;
                    }
                }
                ret = ret && valid_ds;

                let is_top_n = p.kind() == LogicalKind::TopN;
                let is_limit = p.kind() == LogicalKind::Limit;
                if (is_top_n || is_limit) && index_merge_is_intersection {
                    return false; // TopN and Limit cannot be pushed down to the intersection type IndexMerge
                }

                if c.table_info.table_cache_status_type != TableCacheStatusDisable {
                    // Don't push to cop for cached table, it brings more harm than good:
                    // 1. Those tables are small enough, push to cop can't utilize several TiKV to accelerate computation.
                    // 2. Cached table use UnionScan to read the cache data, and push to cop is not supported when an UnionScan exists.
                    // Once aggregation is pushed to cop, the cache data can't be use any more.
                    return false;
                }
            }
            // *LogicalUnionAll: not ported yet (default arm).
            LogicalKind::Sort => {
                if store_tp != kv::TiFlash {
                    return false;
                }
                ret = ret && can_push_to_cop_impl(c, ctx, store_tp, true);
            }
            LogicalKind::Projection => {
                if store_tp != kv::TiFlash {
                    return false;
                }
                ret = ret && can_push_to_cop_impl(c, ctx, store_tp, consider_dual);
            }
            // *LogicalExpand: not ported yet (default arm).
            LogicalKind::TableDual => {
                return store_tp == kv::TiFlash && consider_dual;
            }
            // *LogicalAggregation, *LogicalJoin, *LogicalWindow: not ported yet (default arm).
            LogicalKind::Selection => {
                if store_tp != kv::TiFlash {
                    return false;
                }
                ret = ret && c.can_push_to_cop(ctx, store_tp);
            }
            // These operators can be partially push down to TiFlash, so we don't raise warning for them.
            LogicalKind::Limit | LogicalKind::TopN => {
                return false;
            }
            // *LogicalSequence, *LogicalCTE: not ported yet (default arm, reached by the
            // operators added later).
            #[allow(unreachable_patterns)]
            _ => {
                ctx.vars.raise_warning_when_mpp_enforced(&format!(
                    "MPP mode may be blocked because operator `{}` is not supported now.",
                    c.tp()
                ));
                return false;
            }
        }
    }
    ret
}
