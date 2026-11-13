//! go-file: pkg/planner/util/optimizetrace/opt_tracer.go (boundary stub)
//!
//! blocked-by:D09.13 — the optimizer trace. Here a step is recorded as one string so tests
//! can see that the rule reported what it pruned; Go's `tracing.LogicalOptimizeTracer` is
//! not ported.

/// LogicalOptimizeOp is logical optimizing option for tracing.
#[derive(Debug, Default)]
pub struct LogicalOptimizeOp {
    /// `nil` tracer in Go: tracing disabled.
    pub steps: Option<Vec<String>>,
}

impl LogicalOptimizeOp {
    /// A tracing op (Go: `optimizetrace.DefaultLogicalOptimizeOption().WithEnableOptimizeTracer`).
    // glue: stub, blocked-by:D09.13
    pub fn tracing() -> LogicalOptimizeOp {
        LogicalOptimizeOp {
            steps: Some(Vec::new()),
        }
    }

    /// AppendStepToCurrent appends a step of current action.
    // glue: stub, blocked-by:D09.13
    pub fn append_step_to_current(
        &mut self,
        id: i32,
        tp: &str,
        reason: impl FnOnce() -> String,
        action: impl FnOnce() -> String,
    ) {
        if let Some(steps) = &mut self.steps {
            steps.push(format!("{tp}_{id}: {} ({})", action(), reason()));
        }
    }
}

/// go-file: pkg/planner/util/optimizetrace/logicaltrace/logical_tracer.go (boundary stub)
pub mod logicaltrace {
    use std::sync::Arc;

    use tidb_expression::Column;

    use super::LogicalOptimizeOp;
    use crate::base::LogicalPlan;

    /// AppendColumnPruneTraceStep appends a trace step for column pruning.
    // glue: stub, blocked-by:D09.13 (records the pruned columns only)
    pub fn append_column_prune_trace_step<P: LogicalPlan + ?Sized>(
        p: &P,
        pruned_columns: &[Arc<Column>],
        opt: &mut LogicalOptimizeOp,
    ) {
        if pruned_columns.is_empty() {
            return;
        }
        let cols: Vec<String> = pruned_columns
            .iter()
            .map(|c| String::from_utf8_lossy(&c.to_string()).into_owned())
            .collect();
        opt.append_step_to_current(p.id(), p.tp(), String::new, || {
            format!("columns[{}] have been pruned", cols.join(","))
        });
    }

    /// AppendItemPruneTraceStep for aggregation functions / group-by items / by-items.
    // glue: stub, blocked-by:D09.13 (records the count only)
    pub fn append_item_prune_trace_step<P: LogicalPlan + ?Sized>(
        p: &P,
        item_type: &str,
        n: usize,
        opt: &mut LogicalOptimizeOp,
    ) {
        if n == 0 {
            return;
        }
        opt.append_step_to_current(p.id(), p.tp(), String::new, || {
            format!("{n} {item_type} have been pruned")
        });
    }
}
