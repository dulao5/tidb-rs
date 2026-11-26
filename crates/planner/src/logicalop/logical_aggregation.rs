//! go-file: pkg/planner/core/operator/logicalop/logical_aggregation.go
//!
//! `LogicalAggregation`: the logical aggregation operator (block D09.4 part A, plus its
//! co-ported logical rule methods from D09.7 / D09.8 / D10.6 / D10.2 / D09.6).
//!
//! - `AggFuncs` are stored by value (`Vec<AggFuncDesc>`, matching D08.11 and D09.9).
//! - Cascades-only methods (`Hash64`, `Equals`, `CopyAggHints`, `IsCompleteModeAgg`) are not
//!   ported (`EXC.B`).
//! - `ExplainInfo` (`blocked-by:D10.9`), `ExtractFD` (`blocked-by:D09.14`), and
//!   `ExhaustPhysicalPlans` (`blocked-by:D10.3`) are temporary boundary stubs.

use std::collections::HashMap;
use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::{blocked, blocked_by};
use tidb_base::kv::StoreType;
use tidb_expression::aggregation::{AggFuncDesc, AggFunctionMode, new_agg_func_desc};
use tidb_expression::constant::new_one;
use tidb_expression::expression::{
    compose_cnf_condition, compose_dnf_condition, evaluate_expr_with_null, split_cnf_items,
    split_dnf_items,
};
use tidb_expression::util::{
    column_substitute, contains as expr_contains, expr_has_set_var_or_sleep,
    exprs_has_side_effects, extract_columns, extract_columns_from_expressions, extract_cor_columns,
};
use tidb_expression::{Column, CorrelatedColumn, Expr, ExprRef, Schema, get_used_list, new_schema};
use tidb_parser::ast::funcs as ast_funcs;

use crate::arena::{PlanArena, PlanId};
use crate::base::{self, LogicalPlan, ReplaceMap};
use crate::core::rule_util::resolve_expr_and_replace;
use crate::hint::hint::{PreferHashAgg, PreferStreamAgg};
use crate::logicalop::base_logical_plan::{BaseLogicalPlan, new_base_logical_plan};
use crate::logicalop::logical_plans_misc::prune_by_items;
use crate::logicalop::logical_schema_producer::{self, LogicalSchemaProducer};
use crate::physicalop::PhysicalPlanRef;
use crate::planctx::PlanCtx;
use crate::property::{
    GroupNDV, MPPPartitionColumn, PhysicalProperty, StatsInfo, get_collate_id_by_name_for_partition,
};
use crate::util::logicaltrace::{self, trace_id};
use crate::util::misc::get_max_sort_prefix;
use crate::util::optimizetrace::LogicalOptimizeOp;

/// `plancodec.TypeAgg` (D14.2).
const TYPE_AGG: &str = "Aggregation";

/// LogicalAggregation represents an aggregate plan.
#[derive(Debug)]
pub struct LogicalAggregation {
    /// Embedded `BaseLogicalPlan` (via `LogicalSchemaProducer`).
    pub base: BaseLogicalPlan,
    /// Embedded `LogicalSchemaProducer`.
    pub sp: LogicalSchemaProducer,

    /// Aggregate function descriptors.
    pub agg_funcs: Vec<AggFuncDesc>,
    /// GROUP BY expressions.
    pub group_by_items: Vec<ExprRef>,

    /// PreferAggType and PreferAggToCop store aggregation hint information.
    pub prefer_agg_type: u64,
    pub prefer_agg_to_cop: bool,

    /// Possible sort properties produced by this aggregation.
    pub possible_properties: Vec<Vec<Arc<Column>>>,
    /// InputCount is the input count of this plan.
    pub input_count: f64,

    /// NoCopPushDown indicates if planner must not push this agg down to coprocessor.
    /// It is true when the agg is in the outer child tree of apply.
    pub no_cop_push_down: bool,
}

// go-exception: EXC.B LogicalAggregation.Hash64 (only called by cascades memo via base.HashEquals; tidb-rs does not support cascades planner)
// go-exception: EXC.B LogicalAggregation.Equals (only called by cascades memo via base.HashEquals; tidb-rs does not support cascades planner)
// go-exception: EXC.B LogicalAggregation.CopyAggHints (only called by legacy cascades transformation rules)
// go-exception: EXC.B LogicalAggregation.IsCompleteModeAgg (only called by legacy cascades transformation rules)

impl LogicalAggregation {
    /// Init initializes LogicalAggregation.
    // go: LogicalAggregation.Init L1 [M-PLAN, M-CTX]
    pub fn init(
        agg_funcs: Vec<AggFuncDesc>,
        group_by_items: Vec<ExprRef>,
        ctx: &mut PlanCtx<'_>,
        offset: isize,
    ) -> LogicalAggregation {
        LogicalAggregation {
            base: new_base_logical_plan(ctx, TYPE_AGG, offset),
            sp: LogicalSchemaProducer::default(),
            agg_funcs,
            group_by_items,
            prefer_agg_type: 0,
            prefer_agg_to_cop: false,
            possible_properties: Vec::new(),
            input_count: 0.0,
            no_cop_push_down: false,
        }
    }

    /// BuildKeyInfo implements base.LogicalPlan.<4th> interface.
    // go: LogicalAggregation.BuildKeyInfo L1 [M-PLAN]
    pub fn build_key_info(&mut self, arena: &mut PlanArena) {
        // According to the issue#46962, we can ignore the judgment of partial agg.
        // Sometimes, the agg inside of subquery and there is a true condition in where clause, the agg function is empty.
        if !self.agg_funcs.is_empty() && self.is_partial_mode_agg() {
            return;
        }
        logical_schema_producer::build_key_info(self, arena);
        let group_by_cols = self.get_group_by_cols();
        let all_cols =
            group_by_cols.len() == self.group_by_items.len() && !self.group_by_items.is_empty();
        let no_gby = self.group_by_items.is_empty();
        if all_cols {
            let self_schema = self.schema_mut(arena);
            if let Some(indices) = self_schema.columns_indices(&group_by_cols) {
                let mut new_key = Vec::with_capacity(indices.len());
                for i in indices {
                    new_key.push(Arc::clone(&self_schema.columns[i as usize]));
                }
                self_schema.keys.push(new_key);
            }
        }
        if no_gby {
            self.base.set_max_one_row(true);
        }
    }

    /// BuildSelfKeyInfo builds the key information for the aggregation itself.
    // go: LogicalAggregation.BuildSelfKeyInfo L1
    pub fn build_self_key_info(&mut self, self_schema: &mut Schema) {
        let group_by_cols = self.get_group_by_cols();
        if group_by_cols.len() == self.group_by_items.len()
            && !self.group_by_items.is_empty()
            && let Some(indices) = self_schema.columns_indices(&group_by_cols)
        {
            let mut new_key = Vec::with_capacity(indices.len());
            for i in indices {
                new_key.push(Arc::clone(&self_schema.columns[i as usize]));
            }
            self_schema.keys.push(new_key);
        }
        if self.group_by_items.is_empty() {
            self.base.set_max_one_row(true);
        }
    }

    /// ExtractFD implements base.LogicalPlan.<22nd> interface.
    // glue: stub of LogicalAggregation.ExtractFD, blocked-by:D09.14 (functional dependencies, pkg/planner/funcdep)
    pub fn extract_fd(&self) -> Result<(), Error> {
        Err(blocked(
            "D09.14",
            "LogicalAggregation.ExtractFD (functional dependencies)",
        ))
    }

    /// HasDistinct shows whether LogicalAggregation has functions with distinct.
    // go: LogicalAggregation.HasDistinct L1
    pub fn has_distinct(&self) -> bool {
        self.agg_funcs.iter().any(|f| f.has_distinct)
    }

    /// HasOrderBy shows whether LogicalAggregation has functions with order-by items.
    // go: LogicalAggregation.HasOrderBy L1
    pub fn has_order_by(&self) -> bool {
        self.agg_funcs.iter().any(|f| !f.order_by_items.is_empty())
    }

    /// IsPartialModeAgg returns if all of the AggFuncs are partialMode.
    // go: LogicalAggregation.IsPartialModeAgg L1
    pub fn is_partial_mode_agg(&self) -> bool {
        // Since all of the AggFunc share the same AggMode, we only need to check the first one.
        self.agg_funcs[0].mode == AggFunctionMode::Partial1Mode
    }

    /// GetGroupByCols returns the columns that are group-by items.
    /// For example, `group by a, b, c+d` will return [a, b].
    // go: LogicalAggregation.GetGroupByCols L1
    pub fn get_group_by_cols(&self) -> Vec<Arc<Column>> {
        let mut group_by_cols = Vec::with_capacity(self.group_by_items.len());
        for item in &self.group_by_items {
            if let Expr::Column(col) = &**item {
                group_by_cols.push(Arc::new(col.clone()));
            }
        }
        group_by_cols
    }

    /// GetPotentialPartitionKeys returns potential partition keys for aggregation, the potential
    /// partition keys are the group by keys.
    // go: LogicalAggregation.GetPotentialPartitionKeys L1
    pub fn get_potential_partition_keys(&self) -> Vec<Arc<MPPPartitionColumn>> {
        let mut group_by_cols = Vec::with_capacity(self.group_by_items.len());
        for item in &self.group_by_items {
            if let Expr::Column(col) = &**item {
                let collate_id =
                    get_collate_id_by_name_for_partition(col.get_static_type().get_collate());
                group_by_cols.push(Arc::new(MPPPartitionColumn {
                    col: Arc::new(col.clone()),
                    collate_id,
                }));
            }
        }
        group_by_cols
    }

    /// GetUsedCols extracts all of the Columns used by agg including GroupByItems and AggFuncs.
    // go: LogicalAggregation.GetUsedCols L1
    pub fn get_used_cols(&self) -> Vec<Arc<Column>> {
        let mut used_cols = Vec::new();
        for group_by_item in &self.group_by_items {
            used_cols.extend(extract_columns(group_by_item));
        }
        for agg_desc in &self.agg_funcs {
            for expr in &agg_desc.base.args {
                used_cols.extend(extract_columns(expr));
            }
            for item in &agg_desc.order_by_items {
                used_cols.extend(extract_columns(&item.expr));
            }
        }
        used_cols
    }

    /// ResetHintIfConflicted resets the PreferAggType if they are conflicted,
    /// and returns the two PreferAggType hints.
    // go: LogicalAggregation.ResetHintIfConflicted L1 [M-CTX]
    pub fn reset_hint_if_conflicted(&mut self, ctx: &PlanCtx<'_>) -> (bool, bool) {
        let mut prefer_hash = (self.prefer_agg_type & PreferHashAgg) > 0;
        let mut prefer_stream = (self.prefer_agg_type & PreferStreamAgg) > 0;
        if prefer_hash && prefer_stream {
            ctx.vars
                .stmt_ctx()
                .set_hint_warning("Optimizer aggregation hints are conflicted");
            self.prefer_agg_type = 0;
            prefer_hash = false;
            prefer_stream = false;
        }
        (prefer_hash, prefer_stream)
    }

    /// DistinctArgsMeetsProperty checks if the distinct args meet the property.
    // go: LogicalAggregation.DistinctArgsMeetsProperty L1 [M-CTX]
    pub fn distinct_args_meets_property(&self, ctx: &PlanCtx<'_>) -> bool {
        let ectx = ctx.eval_ctx();
        for agg_func in &self.agg_funcs {
            if agg_func.has_distinct {
                for distinct_arg in &agg_func.base.args {
                    if !expr_contains(&*ectx, &self.group_by_items, distinct_arg) {
                        return false;
                    }
                }
            }
        }
        true
    }

    /// pushDownCNFPredicatesForAggregation splits a CNF condition into two parts:
    /// those that can be pushed down and those that cannot be pushed down below aggregation.
    // go: LogicalAggregation.pushDownCNFPredicatesForAggregation L1 [M-CTX]
    fn push_down_cnf_predicates_for_aggregation(
        &self,
        ctx: &PlanCtx<'_>,
        cond: ExprRef,
        group_by_columns: &Schema,
        exprs_original: &[ExprRef],
    ) -> (Vec<ExprRef>, Vec<ExprRef>) {
        let mut conds_to_push = Vec::new();
        let mut ret = Vec::new();
        let sub_cnf_item = split_cnf_items(&cond);
        if sub_cnf_item.len() == 1 {
            return self.push_down_predicates_for_aggregation(
                ctx,
                sub_cnf_item.into_iter().next().expect("one item"),
                group_by_columns,
                exprs_original,
            );
        }
        for item in sub_cnf_item {
            let (conds_to_push_for_item, ret_for_item) = self
                .push_down_dnf_predicates_for_aggregation(
                    ctx,
                    item,
                    group_by_columns,
                    exprs_original,
                );
            if !conds_to_push_for_item.is_empty()
                && let Some(composed) =
                    compose_dnf_condition(&*ctx.expr_ctx, &conds_to_push_for_item)
            {
                conds_to_push.push(composed);
            }
            if !ret_for_item.is_empty()
                && let Some(composed) = compose_dnf_condition(&*ctx.expr_ctx, &ret_for_item)
            {
                ret.push(composed);
            }
        }
        (conds_to_push, ret)
    }

    /// pushDownDNFPredicatesForAggregation splits a DNF condition into two parts:
    /// those that can be pushed down and those that cannot be pushed down below aggregation.
    // go: LogicalAggregation.pushDownDNFPredicatesForAggregation L1 [M-CTX]
    fn push_down_dnf_predicates_for_aggregation(
        &self,
        ctx: &PlanCtx<'_>,
        cond: ExprRef,
        group_by_columns: &Schema,
        exprs_original: &[ExprRef],
    ) -> (Vec<ExprRef>, Vec<ExprRef>) {
        let mut conds_to_push = Vec::new();
        let mut ret = Vec::new();
        let sub_dnf_item = split_dnf_items(&cond);
        if sub_dnf_item.len() == 1 {
            return self.push_down_predicates_for_aggregation(
                ctx,
                sub_dnf_item.into_iter().next().expect("one item"),
                group_by_columns,
                exprs_original,
            );
        }
        for item in sub_dnf_item {
            let (conds_to_push_for_item, ret_for_item) = self
                .push_down_cnf_predicates_for_aggregation(
                    ctx,
                    item,
                    group_by_columns,
                    exprs_original,
                );
            if conds_to_push_for_item.is_empty() {
                return (Vec::new(), vec![cond]);
            }
            if let Some(composed) = compose_cnf_condition(&*ctx.expr_ctx, &conds_to_push_for_item) {
                conds_to_push.push(composed);
            }
            if !ret_for_item.is_empty()
                && let Some(composed) = compose_cnf_condition(&*ctx.expr_ctx, &ret_for_item)
            {
                ret.push(composed);
            }
        }
        if ret.is_empty() {
            // All the condition can be pushed down.
            return (vec![cond], Vec::new());
        }
        let dnf_push_down_cond = compose_dnf_condition(&*ctx.expr_ctx, &conds_to_push);
        // Some condition can't be pushed down, we need to keep all the condition.
        (dnf_push_down_cond.into_iter().collect(), vec![cond])
    }

    /// splitCondForAggregation splits the condition into those that can be pushed and others.
    // go: LogicalAggregation.splitCondForAggregation L1 [M-CTX]
    pub fn split_cond_for_aggregation(
        &self,
        ctx: &PlanCtx<'_>,
        predicates: Vec<ExprRef>,
    ) -> (Vec<ExprRef>, Vec<ExprRef>) {
        let mut conds_to_push = Vec::new();
        let mut ret = Vec::new();
        let mut exprs_original = Vec::with_capacity(self.agg_funcs.len());
        for fun in &self.agg_funcs {
            exprs_original.push(Arc::clone(&fun.base.args[0]));
        }
        let group_by_columns = new_schema(self.get_group_by_cols());
        // It's almost the same as pushDownCNFPredicatesForAggregation, except that the condition is a slice.
        for cond in predicates {
            let (sub_conds_to_push, sub_ret) = self.push_down_dnf_predicates_for_aggregation(
                ctx,
                cond,
                &group_by_columns,
                &exprs_original,
            );
            if !sub_conds_to_push.is_empty() {
                conds_to_push.extend(sub_conds_to_push);
            }
            if !sub_ret.is_empty() {
                ret.extend(sub_ret);
            }
        }
        (conds_to_push, ret)
    }

    /// pushDownPredicatesForAggregation splits a condition into two parts:
    /// those that can be pushed down and those that cannot be pushed down below aggregation.
    // go: LogicalAggregation.pushDownPredicatesForAggregation L1 [M-CTX]
    fn push_down_predicates_for_aggregation(
        &self,
        ctx: &PlanCtx<'_>,
        cond: ExprRef,
        group_by_columns: &Schema,
        exprs_original: &[ExprRef],
    ) -> (Vec<ExprRef>, Vec<ExprRef>) {
        let mut conds_to_push = Vec::new();
        let mut ret = Vec::new();
        match &*cond {
            Expr::Constant(_) => {
                conds_to_push.push(Arc::clone(&cond));
                // Consider SQL "select sum(b) from t group by a having 1=0". "1=0" is a constant predicate
                // which should be retained and pushed down at the same time. Because we will get a wrong
                // query result that contains one column with value 0 rather than an empty query result.
                ret.push(cond);
            }
            Expr::ScalarFunction(_) => {
                let extracted_cols = extract_columns(&cond);
                let mut ok = true;
                for col in &extracted_cols {
                    if !group_by_columns.contains(col) {
                        ok = false;
                        break;
                    }
                }
                if ok {
                    let new_func = column_substitute(
                        &*ctx.expr_ctx,
                        &cond,
                        self.schema(&ctx.arena),
                        exprs_original,
                    );
                    conds_to_push.push(new_func);
                } else {
                    ret.push(cond);
                }
            }
            _ => {
                ret.push(cond);
            }
        }
        (conds_to_push, ret)
    }

    /// CanPullUp checks if an aggregation can be pulled up. An aggregate function like count(*)
    /// cannot be pulled up. Returns `Err` only if evaluating hits a `blocked` boundary.
    // go: LogicalAggregation.CanPullUp L1 [M-CTX]
    pub fn can_pull_up(&self, ctx: &PlanCtx<'_>) -> Result<bool, Error> {
        if !self.group_by_items.is_empty() {
            return Ok(false);
        }
        let child_schema = ctx.arena.get(self.base.children[0]).schema(&ctx.arena);
        for f in &self.agg_funcs {
            for arg in &f.base.args {
                let expr = match evaluate_expr_with_null(&*ctx.expr_ctx, child_schema, arg) {
                    Ok(e) => e,
                    Err(e) if blocked_by(&e).is_some() => return Err(e),
                    Err(_) => return Ok(false),
                };
                match &*expr {
                    Expr::Constant(con) if con.value.is_null() => {}
                    _ => return Ok(false),
                }
            }
        }
        Ok(true)
    }

    // go: LogicalAggregation.getGroupNDVs L1
    fn get_group_ndvs(
        &self,
        col_groups: &[Vec<Arc<Column>>],
        child_profile: &StatsInfo,
        gby_cols: &[Arc<Column>],
    ) -> Vec<GroupNDV> {
        if col_groups.is_empty() {
            return Vec::new();
        }
        // Check if the child profile provides GroupNDV for the GROUP BY columns.
        // Note that gbyCols may not be the exact GROUP BY columns, e.g, GROUP BY a+b,
        // but we have no other approaches for the NDV estimation of these cases
        // except for using the independent assumption, unless we can use stats of expression index.
        let Some(group_ndv) = StatsInfo::get_group_ndv4_cols(Some(child_profile), gby_cols) else {
            return Vec::new();
        };
        vec![group_ndv.clone()]
    }
}

impl LogicalPlan for LogicalAggregation {
    crate::logical_plan_boilerplate!(Aggregation, producer);

    /// ExplainInfo implements base.Plan.<4th> interface.
    // glue: stub of LogicalAggregation.ExplainInfo, blocked-by:D10.9 (SortedExplainExpressionList and ExplainAggFunc)
    fn explain_info(&self, _ctx: &PlanCtx<'_>) -> Result<String, Error> {
        Err(blocked(
            "D10.9",
            "LogicalAggregation.ExplainInfo (SortedExplainExpressionList / ExplainAggFunc)",
        ))
    }

    /// ReplaceExprColumns implements base.Plan.<5th> interface.
    // go: LogicalAggregation.ReplaceExprColumns L1 [M-EXPR]
    fn replace_expr_columns(&mut self, replace: &ReplaceMap) {
        for agg in &mut self.agg_funcs {
            for agg_expr in &mut agg.base.args {
                resolve_expr_and_replace(agg_expr, replace);
            }
            for order_expr in &mut agg.order_by_items {
                resolve_expr_and_replace(&mut order_expr.expr, replace);
            }
        }
        for gby_item in &mut self.group_by_items {
            resolve_expr_and_replace(gby_item, replace);
        }
    }

    /// PredicatePushDown implements base.LogicalPlan.<1st> interface.
    // go: LogicalAggregation.PredicatePushDown L1 [M-PLAN]
    fn predicate_push_down(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        this: PlanId,
        predicates: Vec<ExprRef>,
        opt: &LogicalOptimizeOp,
    ) -> Result<(Vec<ExprRef>, PlanId), Error> {
        let (conds_to_push, ret) = self.split_cond_for_aggregation(ctx, predicates);
        crate::logicalop::base_logical_plan::predicate_push_down(
            self,
            ctx,
            this,
            conds_to_push,
            opt,
        )?;
        Ok((ret, this))
    }

    /// PruneColumns implements base.LogicalPlan.<2nd> interface.
    // go: LogicalAggregation.PruneColumns L1 [M-PLAN, M-CTX]
    fn prune_columns(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        this: PlanId,
        parent_used_cols: Vec<Arc<Column>>,
        opt: &LogicalOptimizeOp,
    ) -> Result<PlanId, Error> {
        let child = self.children()[0];
        let used = {
            let ectx = ctx.eval_ctx();
            get_used_list(&*ectx, &parent_used_cols, self.schema(&ctx.arena))
        };
        let mut pruned_columns = Vec::new();
        let mut pruned_functions = Vec::new();
        let mut pruned_group_by_items = Vec::new();

        let mut all_first_row = true;
        let mut all_remain_first_row = true;
        for i in (0..used.len()).rev() {
            if self.agg_funcs[i].base.name != ast_funcs::AggFuncFirstRow {
                all_first_row = false;
            }
            if !used[i] && !exprs_has_side_effects(&self.agg_funcs[i].base.args) {
                let col = self.schema_mut(&mut ctx.arena).columns.remove(i);
                pruned_columns.push(col);
                let func = self.agg_funcs.remove(i);
                pruned_functions.push(func);
            } else if self.agg_funcs[i].base.name != ast_funcs::AggFuncFirstRow {
                all_remain_first_row = false;
            }
        }
        logicaltrace::append_column_prune_trace_step(self, ctx, &pruned_columns, opt);
        logicaltrace::append_function_prune_trace_step(self, ctx, &pruned_functions, opt);

        let mut self_used_cols = Vec::new();
        // Prune by-items per agg func; take &self trace id before mutating agg_funcs.
        for idx in 0..self.agg_funcs.len() {
            self_used_cols = extract_columns_from_expressions(
                self_used_cols,
                &self.agg_funcs[idx].base.args,
                None,
            );
            let (new_by_items, cols) =
                prune_by_items(self, ctx, &self.agg_funcs[idx].order_by_items, opt);
            self.agg_funcs[idx].order_by_items = new_by_items;
            self_used_cols.extend(cols);
        }

        if self.agg_funcs.is_empty() || (!all_first_row && all_remain_first_row) {
            // If all the aggregate functions are pruned, we should add an aggregate function to maintain
            // the info of row numbers. For all the aggregate functions except `first_row`, if we have an
            // empty table defined as t(a,b), `select agg(a) from t` would always return one row, while
            // `select agg(a) from t group by b` would return empty. For `first_row` which is only used
            // internally by tidb, `first_row(a)` would always return empty for empty input now.
            let one = Expr::Constant(new_one()).into_ref();
            let new_agg = if all_first_row {
                new_agg_func_desc(&*ctx.expr_ctx, ast_funcs::AggFuncFirstRow, vec![one], false)?
            } else {
                new_agg_func_desc(&*ctx.expr_ctx, ast_funcs::AggFuncCount, vec![one], false)?
            };
            let ret_tp = Arc::clone(&new_agg.base.ret_tp);
            self.agg_funcs.push(new_agg);
            let col = Arc::new(Column {
                unique_id: ctx.vars.alloc_plan_column_id(),
                ret_type: ret_tp,
                ..Column::default()
            });
            self.schema_mut(&mut ctx.arena).columns.push(col);
        }

        if !self.group_by_items.is_empty() {
            for i in (0..self.group_by_items.len()).rev() {
                let cols = extract_columns(&self.group_by_items[i]);
                if cols.is_empty() && !expr_has_set_var_or_sleep(&self.group_by_items[i]) {
                    pruned_group_by_items.push(self.group_by_items.remove(i));
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
        logicaltrace::append_group_by_items_prune_trace_step(
            trace_id(self),
            ctx,
            &pruned_group_by_items,
            opt,
        );
        let new_child = base::prune_columns(ctx, child, self_used_cols, opt)?;
        self.children_mut()[0] = new_child;
        Ok(this)
    }

    /// DeriveStats implements base.LogicalPlan.<11th> interface.
    // go: LogicalAggregation.DeriveStats L1 [M-PLAN]
    fn derive_stats(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        _this: PlanId,
        child_stats: &[Arc<StatsInfo>],
        col_groups: &[Vec<Arc<Column>>],
    ) -> Result<Arc<StatsInfo>, Error> {
        let child_profile = &child_stats[0];
        let mut gby_cols = Vec::with_capacity(self.group_by_items.len());
        for gby_expr in &self.group_by_items {
            gby_cols.extend(extract_columns(gby_expr));
        }
        if let Some(existing) = self.base.plan.stats_info() {
            // Reload GroupNDVs since colGroups may have changed.
            let mut stats = (**existing).clone();
            stats.group_ndvs = self.get_group_ndvs(col_groups, child_profile, &gby_cols);
            let stats = Arc::new(stats);
            self.base.plan.set_stats(Some(Arc::clone(&stats)));
            return Ok(stats);
        }
        let child_schema = ctx.arena.get(self.base.children[0]).schema(&ctx.arena);
        let self_schema = self.sp.schema(&self.base.children, &ctx.arena);
        let (ndv, _) = crate::cardinality::ndv::estimate_cols_ndv_with_matched_len(
            &gby_cols,
            child_schema,
            child_profile,
        );
        let mut col_ndvs = HashMap::with_capacity(self_schema.len());
        // We cannot estimate the ColNDVs for every output, so we use a conservative strategy.
        for col in &self_schema.columns {
            col_ndvs.insert(col.unique_id, ndv);
        }
        self.input_count = child_profile.row_count;
        let group_ndvs = self.get_group_ndvs(col_groups, child_profile, &gby_cols);
        let stats = Arc::new(StatsInfo {
            row_count: ndv,
            col_ndvs,
            group_ndvs,
            ..Default::default()
        });
        self.base.plan.set_stats(Some(Arc::clone(&stats)));
        Ok(stats)
    }

    /// ExtractColGroups implements base.LogicalPlan.<12th> interface.
    // go: LogicalAggregation.ExtractColGroups L1
    fn extract_col_groups(
        &self,
        _arena: &PlanArena,
        _col_groups: &[Vec<Arc<Column>>],
    ) -> Vec<Vec<Arc<Column>>> {
        // Parent colGroups would be discarded, because aggregation would make NDV of colGroups
        // which does not match GroupByItems invalid.
        let mut gby_cols = Vec::with_capacity(self.group_by_items.len());
        for gby_expr in &self.group_by_items {
            gby_cols.extend(extract_columns(gby_expr));
        }
        if gby_cols.len() > 1 {
            return vec![tidb_expression::column::sort_columns(&gby_cols)];
        }
        Vec::new()
    }

    /// PreparePossibleProperties implements base.LogicalPlan.<13th> interface.
    // go: LogicalAggregation.PreparePossibleProperties L1
    fn prepare_possible_properties(
        &mut self,
        _arena: &PlanArena,
        mut children_properties: Vec<Vec<Vec<Arc<Column>>>>,
    ) -> Vec<Vec<Arc<Column>>> {
        let child_props = children_properties.swap_remove(0);
        // If there's no group-by item, the stream aggregation could have no order property.
        // So we can add an empty property when its group-by item is empty.
        if self.group_by_items.is_empty() {
            self.possible_properties = vec![Vec::new()];
            return Vec::new();
        }
        let mut result_properties = Vec::with_capacity(child_props.len());
        let group_by_cols = self.get_group_by_cols();
        for possible_child_property in child_props {
            let sort_col_offsets = get_max_sort_prefix(&possible_child_property, &group_by_cols);
            if sort_col_offsets.len() == group_by_cols.len() {
                let prop = possible_child_property[..group_by_cols.len()].to_vec();
                result_properties.push(prop);
            }
        }
        self.possible_properties = result_properties.clone();
        result_properties
    }

    /// ExhaustPhysicalPlans implements base.LogicalPlan.<14th> interface.
    // glue: stub of LogicalAggregation.ExhaustPhysicalPlans, blocked-by:D10.3 (physical aggregation enumeration)
    fn exhaust_physical_plans(
        &mut self,
        _ctx: &mut PlanCtx<'_>,
        _this: PlanId,
        _prop: &PhysicalProperty,
    ) -> Result<(Vec<PhysicalPlanRef>, bool), Error> {
        Err(blocked(
            "D10.3",
            "LogicalAggregation.ExhaustPhysicalPlans (physical aggregation enumeration)",
        ))
    }

    /// ExtractCorrelatedCols implements base.LogicalPlan.<15th> interface.
    // go: LogicalAggregation.ExtractCorrelatedCols L1
    fn extract_correlated_cols(&self) -> Vec<Arc<CorrelatedColumn>> {
        let mut cor_cols = Vec::with_capacity(self.group_by_items.len() + self.agg_funcs.len());
        for expr in &self.group_by_items {
            cor_cols.extend(extract_cor_columns(expr));
        }
        for fun in &self.agg_funcs {
            for arg in &fun.base.args {
                cor_cols.extend(extract_cor_columns(arg));
            }
            for item in &fun.order_by_items {
                cor_cols.extend(extract_cor_columns(&item.expr));
            }
        }
        cor_cols
    }

    /// CanPushToCop implements base.LogicalPlan.<21st> interface.
    // go: LogicalAggregation.CanPushToCop L1 [M-CTX]
    fn can_push_to_cop(&self, ctx: &PlanCtx<'_>, store_tp: StoreType) -> bool {
        crate::logicalop::logical_plans_misc::can_push_to_cop_impl(self, ctx, store_tp, false)
            && !self.no_cop_push_down
    }
}
