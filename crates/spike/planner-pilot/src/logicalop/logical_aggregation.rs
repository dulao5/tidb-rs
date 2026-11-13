//! go-file: pkg/planner/core/operator/logicalop/logical_aggregation.go (spike subset)

use std::sync::Arc;

use tidb_base::Error;
use tidb_expression::util::{
    expr_has_set_var_or_sleep, exprs_has_side_effects, extract_columns,
    extract_columns_from_expressions,
};
use tidb_expression::{Column, Expr, ExprRef, Schema, get_used_list, new_one};
use tidb_parser::ast::funcs as ast;

use crate::aggregation::{AggFuncDesc, new_agg_func_desc};
use crate::arena::PlanId;
use crate::base::{LogicalPlan, ReplaceMap, prune_columns};
use crate::logicalop::logical_plans_misc::prune_by_items;
use crate::logicalop::{BaseLogicalPlan, LogicalSchemaProducer, logical_plan_boilerplate};
use crate::optimizetrace::{LogicalOptimizeOp, logicaltrace};
use crate::planctx::PlanCtx;
use crate::rule_util::resolve_expr_and_replace;

/// LogicalAggregation represents an aggregate plan.
pub struct LogicalAggregation {
    /// BaseLogicalPlan.
    pub base: BaseLogicalPlan,
    /// LogicalSchemaProducer.
    pub sp: LogicalSchemaProducer,
    /// AggFuncs.
    pub agg_funcs: Vec<AggFuncDesc>,
    /// GroupByItems.
    pub group_by_items: Vec<ExprRef>,
}

impl LogicalAggregation {
    /// Init initializes LogicalAggregation (plus `SetSchema`).
    // go: pkg/planner/core/operator/logicalop/logical_aggregation.go LogicalAggregation.Init L1 [M-PLAN]
    pub fn init(
        agg_funcs: Vec<AggFuncDesc>,
        group_by_items: Vec<ExprRef>,
        schema: Schema,
        ctx: &PlanCtx<'_>,
        qb_offset: i32,
    ) -> LogicalAggregation {
        LogicalAggregation {
            base: BaseLogicalPlan::new(ctx, "Aggregation", qb_offset),
            sp: LogicalSchemaProducer::new(Some(schema)),
            agg_funcs,
            group_by_items,
        }
    }
}

impl LogicalPlan for LogicalAggregation {
    logical_plan_boilerplate!(Aggregation, producer);

    /// ReplaceExprColumns implements base.Plan.<5th> interface.
    // go: pkg/planner/core/operator/logicalop/logical_aggregation.go (*LogicalAggregation).ReplaceExprColumns L1 [M-EXPR]
    fn replace_expr_columns(&mut self, replace: &ReplaceMap) {
        for agg in self.agg_funcs.iter_mut() {
            for agg_expr in agg.args.iter_mut() {
                resolve_expr_and_replace(agg_expr, replace);
            }
            for order_expr in agg.order_by_items.iter_mut() {
                resolve_expr_and_replace(&mut order_expr.expr, replace);
            }
        }
        for gby_item in self.group_by_items.iter_mut() {
            resolve_expr_and_replace(gby_item, replace);
        }
    }

    /// PruneColumns implements base.LogicalPlan.<2nd> interface.
    // go: pkg/planner/core/operator/logicalop/logical_aggregation.go (*LogicalAggregation).PruneColumns L1 [M-PLAN, M-EXPR]
    fn prune_columns(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        this: PlanId,
        parent_used_cols: Vec<Arc<Column>>,
        opt: &mut LogicalOptimizeOp,
    ) -> Result<PlanId, Error> {
        let child = self.children()[0];
        let used = get_used_list(&*ctx.eval_ctx(), &parent_used_cols, self.schema(&ctx.arena));
        let mut pruned_columns = Vec::new();
        let mut pruned_functions = 0usize;
        let mut pruned_group_by_items = 0usize;

        let mut all_first_row = true;
        let mut all_remain_first_row = true;
        for i in (0..used.len()).rev() {
            if self.agg_funcs[i].name != ast::AggFuncFirstRow {
                all_first_row = false;
            }
            if !used[i] && !exprs_has_side_effects(&self.agg_funcs[i].args) {
                pruned_columns.push(self.schema(&ctx.arena).columns[i].clone());
                pruned_functions += 1;
                self.schema_mut(&mut ctx.arena).columns.remove(i);
                self.agg_funcs.remove(i);
            } else if self.agg_funcs[i].name != ast::AggFuncFirstRow {
                all_remain_first_row = false;
            }
        }
        logicaltrace::append_column_prune_trace_step(self, &pruned_columns, opt);
        logicaltrace::append_item_prune_trace_step(
            self,
            "aggregation functions",
            pruned_functions,
            opt,
        );
        let mut self_used_cols: Vec<Arc<Column>> = Vec::new();
        let ectx = ctx.eval_ctx();
        for i in 0..self.agg_funcs.len() {
            self_used_cols =
                extract_columns_from_expressions(self_used_cols, &self.agg_funcs[i].args, None);

            let cols;
            (self.agg_funcs[i].order_by_items, cols) =
                prune_by_items(self, &*ectx, &self.agg_funcs[i].order_by_items, opt);
            self_used_cols.extend(cols);
        }
        if self.agg_funcs.is_empty() || (!all_first_row && all_remain_first_row) {
            // If all the aggregate functions are pruned, we should add an aggregate function to maintain the info of row numbers.
            // For all the aggregate functions except `first_row`, if we have an empty table defined as t(a,b),
            // `select agg(a) from t` would always return one row, while `select agg(a) from t group by b` would return empty.
            // For `first_row` which is only used internally by tidb, `first_row(a)` would always return empty for empty input now.
            let new_agg = if all_first_row {
                new_agg_func_desc(
                    &*ctx.expr_ctx,
                    ast::AggFuncFirstRow,
                    vec![Expr::Constant(new_one()).into_ref()],
                    false,
                )?
            } else {
                new_agg_func_desc(
                    &*ctx.expr_ctx,
                    ast::AggFuncCount,
                    vec![Expr::Constant(new_one()).into_ref()],
                    false,
                )?
            };
            let col = Column {
                unique_id: ctx.vars.alloc_plan_column_id(),
                ret_type: new_agg.ret_tp.clone(),
                ..Column::default()
            };
            self.agg_funcs.push(new_agg);
            self.schema_mut(&mut ctx.arena).columns.push(Arc::new(col));
        }

        if !self.group_by_items.is_empty() {
            for i in (0..self.group_by_items.len()).rev() {
                let cols = extract_columns(&self.group_by_items[i]);
                if cols.is_empty() && !expr_has_set_var_or_sleep(&self.group_by_items[i]) {
                    pruned_group_by_items += 1;
                    self.group_by_items.remove(i);
                } else {
                    self_used_cols.extend(cols);
                }
            }
            // If all the group by items are pruned, we should add a constant 1 to keep the correctness.
            // Because `select count(*) from t` is different from `select count(*) from t group by 1`.
            if self.group_by_items.is_empty() {
                self.group_by_items = vec![Expr::Constant(new_one()).into_ref()];
            }
        }
        logicaltrace::append_item_prune_trace_step(
            self,
            "groupByItems",
            pruned_group_by_items,
            opt,
        );
        self.children_mut()[0] = prune_columns(ctx, child, self_used_cols, opt)?;
        // update children[0]: Go reassigns the local `child` here and never reads it again.
        Ok(this)
    }
}
