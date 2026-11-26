//! go-file: pkg/planner/util/optimizetrace/logicaltrace/logical_tracer.go
//!
//! The trace steps of the logical rules (package `logicaltrace`, ported as a whole leaf util
//! file, constitution §4.4). Go passes the plan and reads `p.TP()`, `p.ID()` and
//! `p.SCtx()`; here a plan is its [`TraceId`] (type and plan id) and the context is the
//! `PlanCtx` (`M-CTX`). The closures run only when the tracer is on (Go's lazy strings).

use std::sync::Arc;

use tidb_base::errors::RedactLogDisable;
use tidb_expression::aggregation::AggFuncDesc;
use tidb_expression::{Column, ExprRef};

use crate::base::LogicalPlan;
use crate::planctx::PlanCtx;
use crate::util::ByItems;
use crate::util::optimizetrace::LogicalOptimizeOp;

/// What a trace step shows of a plan: `TP()` and `ID()`.
pub type TraceId = (&'static str, i32);

/// The [`TraceId`] of a plan.
// glue: M-CTX (Go passes the plan to the trace functions and reads TP() / ID())
pub fn trace_id<P: LogicalPlan + ?Sized>(p: &P) -> TraceId {
    (p.tp(), p.id())
}

/// The trace text of one pruned item (`expression.StringerWithCtx`).
// glue: M-IFACE (expression.StringerWithCtx over the item types of this file)
fn item_string(ctx: &PlanCtx<'_>, item: &PrunedItem<'_>) -> String {
    let ectx = ctx.eval_ctx();
    let b = match item {
        PrunedItem::Column(c) => c.string_with_ctx(&*ectx, RedactLogDisable),
        PrunedItem::AggFunc(f) => f.string_with_ctx(&*ectx, RedactLogDisable),
        PrunedItem::ByItems(b) => b.string_with_ctx(&*ectx, RedactLogDisable),
        PrunedItem::Expr(e) => e.string_with_ctx(&*ectx, RedactLogDisable),
    };
    String::from_utf8_lossy(&b).into_owned()
}

/// An `expression.StringerWithCtx` of the pruned items (`M-IFACE`).
enum PrunedItem<'a> {
    Column(&'a Column),
    AggFunc(&'a AggFuncDesc),
    ByItems(&'a ByItems),
    Expr(&'a ExprRef),
}

// go: appendItemPruneTraceStep L1 [M-CTX]
fn append_item_prune_trace_step(
    p: TraceId,
    ctx: &PlanCtx<'_>,
    item_type: &str,
    pruned_objects: &[PrunedItem<'_>],
    opt: &LogicalOptimizeOp,
) {
    if pruned_objects.is_empty() {
        return;
    }
    let action = || {
        let mut buffer = format!("{}_{}'s {}[", p.0, p.1, item_type);
        for (i, item) in pruned_objects.iter().enumerate() {
            if i > 0 {
                buffer.push(',');
            }
            buffer.push_str(&item_string(ctx, item));
        }
        buffer.push_str("] have been pruned");
        buffer
    };
    let reason = String::new;
    opt.append_step_to_current(p.1 as isize, p.0, reason, action);
}

/// AppendColumnPruneTraceStep appends a trace step for column pruning.
// go: AppendColumnPruneTraceStep L1 [M-CTX]
pub fn append_column_prune_trace_step<P: LogicalPlan + ?Sized>(
    p: &P,
    ctx: &PlanCtx<'_>,
    pruned_columns: &[Arc<Column>],
    opt: &LogicalOptimizeOp,
) {
    if pruned_columns.is_empty() {
        return;
    }
    let s: Vec<PrunedItem<'_>> = pruned_columns
        .iter()
        .map(|c| PrunedItem::Column(c))
        .collect();
    append_item_prune_trace_step(trace_id(p), ctx, "columns", &s, opt);
}

/// AppendFunctionPruneTraceStep appends a trace step for group by pruning.
// go: AppendFunctionPruneTraceStep L1 [M-CTX]
pub fn append_function_prune_trace_step<P: LogicalPlan + ?Sized>(
    p: &P,
    ctx: &PlanCtx<'_>,
    pruned_functions: &[AggFuncDesc],
    opt: &LogicalOptimizeOp,
) {
    if pruned_functions.is_empty() {
        return;
    }
    let s: Vec<PrunedItem<'_>> = pruned_functions.iter().map(PrunedItem::AggFunc).collect();
    append_item_prune_trace_step(trace_id(p), ctx, "aggregation functions", &s, opt);
}

/// AppendByItemsPruneTraceStep appends a trace step for group by pruning.
// go: AppendByItemsPruneTraceStep L1 [M-CTX]
pub fn append_by_items_prune_trace_step<P: LogicalPlan + ?Sized>(
    p: &P,
    ctx: &PlanCtx<'_>,
    pruned_by_items: &[ByItems],
    opt: &LogicalOptimizeOp,
) {
    if pruned_by_items.is_empty() {
        return;
    }
    let s: Vec<PrunedItem<'_>> = pruned_by_items.iter().map(PrunedItem::ByItems).collect();
    append_item_prune_trace_step(trace_id(p), ctx, "byItems", &s, opt);
}

/// AppendGroupByItemsPruneTraceStep appends a trace step for group by pruning.
// go: AppendGroupByItemsPruneTraceStep L1 [M-CTX]
pub fn append_group_by_items_prune_trace_step(
    p: TraceId,
    ctx: &PlanCtx<'_>,
    pruned_group_by_items: &[ExprRef],
    opt: &LogicalOptimizeOp,
) {
    if pruned_group_by_items.is_empty() {
        return;
    }
    let s: Vec<PrunedItem<'_>> = pruned_group_by_items.iter().map(PrunedItem::Expr).collect();
    append_item_prune_trace_step(p, ctx, "groupByItems", &s, opt);
}

/// ApplyEliminateTraceStep appends a trace step for aggregation pruning.
// go: ApplyEliminateTraceStep L1
pub fn apply_eliminate_trace_step(lp: TraceId, opt: &LogicalOptimizeOp) {
    let action = || format!("{}_{} is eliminated.", lp.0, lp.1);
    let reason = || {
        format!(
            "{}_{} can be eliminated because it hasn't been used by it's parent.",
            lp.0, lp.1
        )
    };
    opt.append_step_to_current(lp.1 as isize, lp.0, reason, action);
}
