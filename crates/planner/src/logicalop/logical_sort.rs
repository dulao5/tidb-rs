//! go-file: pkg/planner/core/operator/logicalop/logical_sort.go
//!
//! `LogicalSort`: ORDER BY. No schema of its own.

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::errors::RedactLogDisable;
use tidb_expression::util::extract_cor_columns;
use tidb_expression::{Column, CorrelatedColumn, Expr};

use crate::arena::PlanId;
use crate::base::{self, LogicalPlan, ReplaceMap};
use crate::core::rule_util::resolve_expr_and_replace;
use crate::logicalop::logical_plans_misc::prune_by_items;
use crate::logicalop::{BaseLogicalPlan, LogicalTopN, new_base_logical_plan};
use crate::planctx::PlanCtx;
use crate::util::ByItems;
use crate::util::explain_misc::explain_by_items;
use crate::util::logicaltrace::trace_id;
use crate::util::optimizetrace::LogicalOptimizeOp;

/// LogicalSort stands for the order by plan.
pub struct LogicalSort {
    /// The embedded `BaseLogicalPlan`.
    pub base: BaseLogicalPlan,
    /// ByItems. Go's `[]*util.ByItems`: the items are values here; Go's in-place writes
    /// through shared items are listed in the M-FT audit of the VS1 plan.
    pub by_items: Vec<ByItems>,
}

impl LogicalSort {
    /// Init initializes LogicalSort.
    // go: LogicalSort.Init L1 [M-SELF]
    pub fn init(by_items: Vec<ByItems>, ctx: &PlanCtx<'_>, offset: isize) -> LogicalSort {
        LogicalSort {
            base: new_base_logical_plan(ctx, "Sort", offset),
            by_items,
        }
    }
}

impl LogicalPlan for LogicalSort {
    crate::logical_plan_boilerplate!(Sort);

    /// PreparePossibleProperties implements base.LogicalPlan.<13th> interface.
    // go: LogicalSort.PreparePossibleProperties L1
    fn prepare_possible_properties(
        &mut self,
        _arena: &crate::PlanArena,
        _children_properties: Vec<Vec<Vec<Arc<Column>>>>,
    ) -> Vec<Vec<Arc<Column>>> {
        let prop_cols = get_possible_property_from_by_items(&self.by_items);
        if prop_cols.is_empty() {
            return Vec::new();
        }
        vec![prop_cols]
    }

    /// ExhaustPhysicalPlans implements base.LogicalPlan.<14th> interface.
    // go: LogicalSort.ExhaustPhysicalPlans L1 [M-PLAN]
    fn exhaust_physical_plans(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        _this: PlanId,
        prop: &crate::property::PhysicalProperty,
    ) -> Result<(Vec<crate::PhysicalPlanRef>, bool), Error> {
        crate::core::exhaust_physical_plans::exhaust_physical_plans4_logical_sort(self, ctx, prop)
    }

    /// ExplainInfo implements Plan interface.
    // go: LogicalSort.ExplainInfo L1
    fn explain_info(&self, ctx: &PlanCtx<'_>) -> Result<String, Error> {
        let mut buffer = String::new();
        let e_ctx = ctx.eval_ctx();
        explain_by_items(&*e_ctx, &mut buffer, &self.by_items)?;
        Ok(buffer)
    }

    /// ReplaceExprColumns implements base.LogicalPlan interface.
    // go: LogicalSort.ReplaceExprColumns L1 [M-EXPR]
    fn replace_expr_columns(&mut self, replace: &ReplaceMap) {
        for by_item in self.by_items.iter_mut() {
            resolve_expr_and_replace(&mut by_item.expr, replace);
        }
    }

    /// PruneColumns implements base.LogicalPlan.<2nd> interface.
    /// If any expression can view as a constant in execution stage, such as correlated column, constant,
    /// we do prune them. Note that we can't prune the expressions contain non-deterministic functions, such as rand().
    // go: LogicalSort.PruneColumns L1 [M-PLAN]
    fn prune_columns(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        this: PlanId,
        mut parent_used_cols: Vec<Arc<Column>>,
        opt: &LogicalOptimizeOp,
    ) -> Result<PlanId, Error> {
        let (by_items, cols) = prune_by_items(self, ctx, &self.by_items, opt);
        self.by_items = by_items;
        parent_used_cols.extend(cols);
        let child = self.children()[0];
        self.children_mut()[0] = base::prune_columns(ctx, child, parent_used_cols, opt)?;
        Ok(this)
    }

    /// PushDownTopN implements the base.LogicalPlan.<5th> interface.
    // go: LogicalSort.PushDownTopN L1 [M-PLAN]
    fn push_down_top_n(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        this: PlanId,
        top_n: Option<PlanId>,
        opt: &LogicalOptimizeOp,
    ) -> PlanId {
        let Some(top_n) = top_n else {
            return crate::logicalop::base_logical_plan::push_down_top_n(
                self, ctx, this, None, opt,
            );
        };
        let top_n_node = ctx
            .arena
            .downcast_mut::<LogicalTopN>(top_n)
            .expect("LogicalTopN");
        if top_n_node.is_limit() {
            // topN.ByItems = ls.ByItems: Go shares the *ByItems (M-FT audit: the sort leaves
            // the tree here).
            top_n_node.by_items = self.by_items.clone();
            let top_n_tid = trace_id(top_n_node);
            append_sort_pass_by_items_trace_step(self, ctx, top_n_tid, opt);
            return base::push_down_top_n(ctx, self.children()[0], Some(top_n), opt);
        }
        // If a TopN is pushed down, this sort is useless.
        base::push_down_top_n(ctx, self.children()[0], Some(top_n), opt)
    }

    /// ExtractCorrelatedCols implements base.LogicalPlan.<15th> interface.
    // go: LogicalSort.ExtractCorrelatedCols L1
    fn extract_correlated_cols(&self) -> Vec<Arc<CorrelatedColumn>> {
        let mut cor_cols = Vec::with_capacity(self.by_items.len());
        for item in &self.by_items {
            cor_cols.extend(extract_cor_columns(&item.expr));
        }
        cor_cols
    }
}

// go: appendSortPassByItemsTraceStep L1 [M-CTX]
fn append_sort_pass_by_items_trace_step(
    sort: &LogicalSort,
    ctx: &PlanCtx<'_>,
    top_n: (&'static str, i32),
    opt: &LogicalOptimizeOp,
) {
    let action = || {
        let ectx = ctx.eval_ctx();
        let mut buffer = format!("{}_{} passes ByItems[", sort.tp(), sort.id());
        for (i, item) in sort.by_items.iter().enumerate() {
            if i > 0 {
                buffer.push(',');
            }
            buffer.push_str(&String::from_utf8_lossy(
                &item.string_with_ctx(&*ectx, RedactLogDisable),
            ));
        }
        buffer.push_str(&format!("] to {}_{}", top_n.0, top_n.1));
        buffer
    };
    let reason = || format!("{}_{} is Limit originally", top_n.0, top_n.1);
    opt.append_step_to_current(sort.id() as isize, sort.tp(), reason, action);
}

/// getPossiblePropertyFromByItems.
// go: getPossiblePropertyFromByItems L1
pub fn get_possible_property_from_by_items(items: &[ByItems]) -> Vec<Arc<Column>> {
    let mut cols = Vec::with_capacity(items.len());
    for item in items {
        let Expr::Column(col) = &*item.expr else {
            break;
        };
        cols.push(Arc::new(col.clone()));
    }
    cols
}
