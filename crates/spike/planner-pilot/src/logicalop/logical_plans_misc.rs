//! go-file: pkg/planner/core/operator/logicalop/logical_plans_misc.go (spike subset)

use std::collections::HashSet;
use std::sync::Arc;

use tidb_base::mysql::TypeNull;
use tidb_expression::util::{extract_columns, is_runtime_const_expr};
use tidb_expression::{Column, EvalContext};

use crate::base::LogicalPlan;
use crate::optimizetrace::{LogicalOptimizeOp, logicaltrace};
use crate::util::ByItems;

/// pruneByItems: drops duplicated and constant by-items and returns the columns the kept
/// ones use. `p` is only used for `p.SCtx()` (passed as `ectx`) and the trace.
// go: pkg/planner/core/operator/logicalop/logical_plans_misc.go pruneByItems L1 [M-CTX]
pub fn prune_by_items<P: LogicalPlan + ?Sized>(
    p: &P,
    ectx: &dyn EvalContext,
    old: &[ByItems],
    opt: &mut LogicalOptimizeOp,
) -> (Vec<ByItems>, Vec<Arc<Column>>) {
    let mut parent_used_cols = Vec::new();
    let mut pruned_by_items = 0usize;
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
            } else if by_item.expr.get_type(ectx).get_type() != TypeNull {
                pruned = false;
                parent_used_cols.extend(cols);
                by_items.push(by_item.clone());
            }
        }
        if pruned {
            pruned_by_items += 1;
        }
    }
    logicaltrace::append_item_prune_trace_step(p, "byItems", pruned_by_items, opt);
    (by_items, parent_used_cols)
}
