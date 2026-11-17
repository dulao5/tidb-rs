//! go-file: pkg/planner/core/operator/logicalop/logical_selection.go
//!
//! `LogicalSelection`, a WHERE or HAVING filter. It has no schema of its own (`Schema()` is
//! the child's).

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_base::errors::RedactLogDisable;
use tidb_base::kv::StoreType;
use tidb_expr_pushdown::infer_pushdown::can_exprs_push_down;
use tidb_expression::chunk_executor::has_get_set_var_func;
use tidb_expression::constant_propagation::{propagate_constant, valid_compare_constant_predicate};
use tidb_expression::util::{
    contain_virtual_column, extract_columns_from_expressions, extract_cor_columns,
};
use tidb_expression::{Column, CorrelatedColumn, ExprRef};

use crate::arena::PlanId;
use crate::base::{self, LogicalPlan, ReplaceMap};
use crate::core::constraint::delete_true_exprs;
use crate::core::rule_util::resolve_expr_and_replace;
use crate::logicalop::logical_join::conds2_table_dual;
use crate::logicalop::{BaseLogicalPlan, new_base_logical_plan};
use crate::planctx::PlanCtx;
use crate::util::logicaltrace::{TraceId, trace_id};
use crate::util::optimizetrace::LogicalOptimizeOp;

/// LogicalSelection represents a where or having predicate.
pub struct LogicalSelection {
    /// The embedded `BaseLogicalPlan`.
    pub base: BaseLogicalPlan,
    /// Originally the WHERE or ON condition is parsed into a single expression,
    /// but after we converted to CNF(Conjunctive normal form), it can be
    /// split into a list of AND conditions.
    pub conditions: Vec<ExprRef>,
}

impl LogicalSelection {
    /// Init initializes LogicalSelection.
    // go: LogicalSelection.Init L1 [M-SELF]
    pub fn init(conditions: Vec<ExprRef>, ctx: &PlanCtx<'_>, qb_offset: isize) -> LogicalSelection {
        LogicalSelection {
            base: new_base_logical_plan(ctx, "Selection", qb_offset),
            conditions,
        }
    }

    /// CanPushDown is utility function to check whether we can push down Selection to TiKV
    /// or TiFlash.
    // go: LogicalSelection.CanPushDown L1
    pub fn can_push_down(&self, ctx: &PlanCtx<'_>, store_tp: StoreType) -> Result<bool, Error> {
        Ok(!contain_virtual_column(&self.conditions)
            && self.can_push_to_cop(ctx, store_tp)
            && can_exprs_push_down(&ctx.push_down_ctx()?, &self.conditions, store_tp))
    }
}

// glue: stub of LogicalSelection.HashCode, blocked-by:D14.2 (plancodec.TypeStringToPhysicalID, util/plancodec)
// glue: stub of LogicalSelection.BuildKeyInfo, blocked-by:D09.13 (rule/util CheckMaxOneRowCond comes with the key-info rule, VS1-24)
// glue: stub of LogicalSelection.ExtractFD, blocked-by:D09.14 (pkg/planner/funcdep is not ported)
/// The boundary of the stubbed `LogicalSelection` functions above.
pub fn selection_boundary(block: &str, what: &str) -> Error {
    blocked(block, what)
}

impl LogicalPlan for LogicalSelection {
    crate::logical_plan_boilerplate!(Selection);

    /// ExplainInfo implements Plan interface.
    // go: LogicalSelection.ExplainInfo L2 (expression.SortedExplainExpressionList is blocked-by:D10.9)
    fn explain_info(&self, _ctx: &PlanCtx<'_>) -> Result<String, Error> {
        Err(blocked(
            "D10.9",
            "the EXPLAIN text of an expression list (SortedExplainExpressionList)",
        ))
    }

    /// ReplaceExprColumns implements base.LogicalPlan interface.
    // go: LogicalSelection.ReplaceExprColumns L1 [M-EXPR]
    fn replace_expr_columns(&mut self, replace: &ReplaceMap) {
        for expr in self.conditions.iter_mut() {
            resolve_expr_and_replace(expr, replace);
        }
    }

    /// HashCode implements base.LogicalPlan.<0th> interface.
    // glue: dispatch to the HashCode boundary stub above
    fn hash_code(&self) -> Result<Vec<u8>, Error> {
        Err(selection_boundary(
            "D14.2",
            "plancodec.TypeStringToPhysicalID (LogicalSelection.HashCode)",
        ))
    }

    /// PredicatePushDown implements base.LogicalPlan.<1st> interface.
    // go: LogicalSelection.PredicatePushDown L1 [M-PLAN]
    fn predicate_push_down(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        this: PlanId,
        mut predicates: Vec<ExprRef>,
        opt: &LogicalOptimizeOp,
    ) -> Result<(Vec<ExprRef>, PlanId), Error> {
        predicates = delete_true_exprs(ctx, &predicates);
        self.conditions = delete_true_exprs(ctx, &self.conditions);
        let (can_be_push_down, can_not_be_push_down) = split_set_get_var_func(&self.conditions);
        let origin_conditions = can_be_push_down.clone();
        let mut pushed = can_be_push_down;
        pushed.extend(predicates);
        let child0 = self.children()[0];
        let (mut ret_conditions, child) = base::predicate_push_down(ctx, child0, pushed, opt)?;
        ret_conditions.extend(can_not_be_push_down);
        if !ret_conditions.is_empty() {
            self.conditions = propagate_constant(&ctx.expr_ctx, &ret_conditions);
            // Return table dual when filter is constant false or null.
            // Go: p.Children()[0] is still the old child here (PredicatePushDown's result
            // `child` is dropped when the selection stays).
            let schema = self.schema(&ctx.arena).clone();
            let qb = self.query_block_offset();
            if let Some(dual) = conds2_table_dual(ctx, schema, qb, &self.conditions) {
                append_table_dual_trace_step(ctx, trace_id(self), dual, &self.conditions, opt);
                return Ok((Vec::new(), dual));
            }
            return Ok((Vec::new(), this));
        }
        append_selection_predicate_push_down_trace_step(self, ctx, &origin_conditions, opt);
        Ok((Vec::new(), child))
    }

    /// PruneColumns implements base.LogicalPlan.<2nd> interface.
    // go: LogicalSelection.PruneColumns L1 [M-PLAN]
    fn prune_columns(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        this: PlanId,
        mut parent_used_cols: Vec<Arc<Column>>,
        opt: &LogicalOptimizeOp,
    ) -> Result<PlanId, Error> {
        let child = self.children()[0];
        parent_used_cols =
            extract_columns_from_expressions(parent_used_cols, &self.conditions, None);
        self.children_mut()[0] = base::prune_columns(ctx, child, parent_used_cols, opt)?;
        Ok(this)
    }

    /// DeriveTopN implements the base.LogicalPlan.<6th> interface.
    // go: LogicalSelection.DeriveTopN L2 (windowIsTopN needs a LogicalWindow child: the window operator is block D09.4 and cannot be in the tree, so the derive branch is unreachable)
    fn derive_top_n(
        &mut self,
        _ctx: &mut PlanCtx<'_>,
        this: PlanId,
        _opt: &LogicalOptimizeOp,
    ) -> Result<PlanId, Error> {
        // windowIsTopN, limitValue := utilfuncp.WindowIsTopN(s): false without a window child.
        Ok(this)
    }

    /// PullUpConstantPredicates implements the base.LogicalPlan.<9th> interface.
    // go: LogicalSelection.PullUpConstantPredicates L1
    fn pull_up_constant_predicates(&self, ctx: &PlanCtx<'_>) -> Vec<ExprRef> {
        let mut result = Vec::new();
        let ectx = ctx.eval_ctx();
        for candidate_predicate in &self.conditions {
            // the candidate predicate should be a constant and compare predicate
            let m = valid_compare_constant_predicate(&*ectx, candidate_predicate);
            if m {
                result.push(Arc::clone(candidate_predicate));
            }
        }
        result
    }

    /// ExtractCorrelatedCols implements base.LogicalPlan.<15th> interface.
    // go: LogicalSelection.ExtractCorrelatedCols L1
    fn extract_correlated_cols(&self) -> Vec<Arc<CorrelatedColumn>> {
        let mut cor_cols = Vec::with_capacity(self.conditions.len());
        for cond in &self.conditions {
            cor_cols.extend(extract_cor_columns(cond));
        }
        cor_cols
    }

    /// ConvertOuterToInnerJoin implements base.LogicalPlan.<24th> interface.
    // go: LogicalSelection.ConvertOuterToInnerJoin L1
    fn convert_outer_to_inner_join(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        this: PlanId,
        mut predicates: Vec<ExprRef>,
    ) -> PlanId {
        predicates.extend(self.conditions.iter().cloned());
        let combined_cond = predicates;
        let mut child = self.children()[0];
        child = base::convert_outer_to_inner_join(ctx, child, combined_cond);
        self.set_children(vec![child]);
        this
    }
}

// go: splitSetGetVarFunc L1
fn split_set_get_var_func(filters: &[ExprRef]) -> (Vec<ExprRef>, Vec<ExprRef>) {
    let mut can_be_push_down = Vec::with_capacity(filters.len());
    let mut can_not_be_push_down = Vec::with_capacity(filters.len());
    for expr in filters {
        if has_get_set_var_func(expr) {
            can_not_be_push_down.push(Arc::clone(expr));
        } else {
            can_be_push_down.push(Arc::clone(expr));
        }
    }
    (can_be_push_down, can_not_be_push_down)
}

/// The trace text of a condition list.
// glue: the shared loop of the trace closures below (Go repeats it inline)
fn conds_string(ctx: &PlanCtx<'_>, conditions: &[ExprRef]) -> String {
    let ectx = ctx.eval_ctx();
    let mut buffer = String::new();
    for (i, cond) in conditions.iter().enumerate() {
        if i > 0 {
            buffer.push(',');
        }
        buffer.push_str(&String::from_utf8_lossy(
            &cond.string_with_ctx(&*ectx, RedactLogDisable),
        ));
    }
    buffer
}

/// AppendTableDualTraceStep appends a trace step for replacing a plan with a dual table.
// go: AppendTableDualTraceStep L1 [M-CTX]
pub fn append_table_dual_trace_step(
    ctx: &PlanCtx<'_>,
    replaced: TraceId,
    dual: PlanId,
    conditions: &[ExprRef],
    opt: &LogicalOptimizeOp,
) {
    let dual = trace_id(ctx.arena.get(dual));
    let action = || {
        format!(
            "{}_{} is replaced by {}_{}",
            replaced.0, replaced.1, dual.0, dual.1
        )
    };
    let reason = || {
        format!(
            "The conditions[{}] are constant false or null",
            conds_string(ctx, conditions)
        )
    };
    opt.append_step_to_current(dual.1 as isize, dual.0, reason, action);
}

// go: appendSelectionPredicatePushDownTraceStep L1 [M-CTX]
fn append_selection_predicate_push_down_trace_step(
    p: &LogicalSelection,
    ctx: &PlanCtx<'_>,
    conditions: &[ExprRef],
    opt: &LogicalOptimizeOp,
) {
    let (tp, id) = trace_id(p);
    let action = || format!("{tp}_{id} is removed");
    if !conditions.is_empty() {
        let reason = || {
            format!(
                "The conditions[{}] in {tp}_{id} are pushed down",
                conds_string(ctx, conditions)
            )
        };
        opt.append_step_to_current(id as isize, tp, reason, action);
    } else {
        opt.append_step_to_current(id as isize, tp, String::new, action);
    }
}

// go: appendDerivedTopNTrace L1 [M-CTX]
#[allow(dead_code)] // called by DeriveTopN once the window operator exists (D09.4)
fn append_derived_top_n_trace(top_n: TraceId, child: TraceId, opt: &LogicalOptimizeOp) {
    let action = || {
        format!(
            "{}_{} top N added below  {}_{} ",
            top_n.0, top_n.1, child.0, child.1
        )
    };
    let reason = || format!("{} filter on row number", top_n.0);
    opt.append_step_to_current(top_n.1 as isize, top_n.0, reason, action);
}
