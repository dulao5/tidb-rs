//! go-file: pkg/planner/core/operator/logicalop/logical_projection.go
//!
//! `LogicalProjection`. Its schema has one column per expression (`Exprs[i]` produces
//! `Schema().Columns[i]`).

use std::collections::HashMap;
use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_base::mysql::{ParseToJSONFlag, TypeJSON};
use tidb_expression::chunk_executor::{has_assign_set_var_func, has_get_set_var_func};
use tidb_expression::constant::new_zero;
use tidb_expression::constant_fold::fold_constant;
use tidb_expression::util::{
    column_substitute, column_substitute_impl, expr_has_set_var_or_sleep, extract_columns,
    extract_columns_from_expressions, extract_cor_columns, is_immutable_func,
};
use tidb_expression::{
    Column, CorrelatedColumn, Expr, ExprRef, Schema, column2_exprs, get_used_list, new_schema,
};

use crate::arena::PlanId;
use crate::base::{self, LogicalKind, LogicalPlan, ReplaceMap};
use crate::core::rule_util::resolve_expr_and_replace;
use crate::logicalop::{
    BaseLogicalPlan, LogicalSchemaProducer, LogicalTopN, new_base_logical_plan,
};
use crate::planctx::PlanCtx;
use crate::property::SortItem;
use crate::util::logicaltrace;
use crate::util::optimizetrace::LogicalOptimizeOp;

/// LogicalProjection represents a select fields plan.
pub struct LogicalProjection {
    /// The embedded `BaseLogicalPlan`.
    pub base: BaseLogicalPlan,
    /// The embedded `LogicalSchemaProducer`.
    pub sp: LogicalSchemaProducer,
    /// Exprs.
    pub exprs: Vec<ExprRef>,
    /// CalculateNoDelay indicates this Projection is the root Plan and should be
    /// calculated without delay and will not return any result to client.
    /// Currently it is "true" only when the current sql query is a "DO" statement.
    pub calculate_no_delay: bool,
    /// Proj4Expand is used for expand to project same column reference, while these
    /// col may be filled with null so we couldn't just eliminate this projection itself.
    pub proj4_expand: bool,
}

impl LogicalProjection {
    /// Init initializes LogicalProjection (the struct literal's `Exprs`; the schema is set
    /// right after, `SetSchema`).
    // go: LogicalProjection.Init L1 [M-SELF]
    pub fn init(exprs: Vec<ExprRef>, ctx: &PlanCtx<'_>, qb_offset: isize) -> LogicalProjection {
        LogicalProjection {
            base: new_base_logical_plan(ctx, "Projection", qb_offset),
            sp: LogicalSchemaProducer::new(None),
            exprs,
            calculate_no_delay: false,
            proj4_expand: false,
        }
    }

    /// GetUsedCols extracts all of the Columns used by proj.
    // go: LogicalProjection.GetUsedCols L1
    pub fn get_used_cols(&self) -> Vec<Arc<Column>> {
        let mut used_cols = Vec::new();
        for expr in &self.exprs {
            used_cols.extend(extract_columns(expr));
        }
        used_cols
    }

    /// A bijection exists between columns of a projection's schema and this projection's
    /// Exprs. Sometimes we need a schema made by expr of Exprs to convert a column in child's
    /// schema to a column in this projection's Schema.
    // go: LogicalProjection.buildSchemaByExprs L1 [M-CTX]
    fn build_schema_by_exprs(&self, ctx: &PlanCtx<'_>, self_schema: &Schema) -> Schema {
        let mut schema = new_schema(Vec::with_capacity(self_schema.len()));
        let ectx = ctx.eval_ctx();
        for expr in &self.exprs {
            if let Expr::Column(col) = &**expr {
                schema.append(vec![Arc::new(col.clone())]);
            } else {
                // If the expression is not a column, we add a column to occupy the position.
                schema.append(vec![Arc::new(Column {
                    unique_id: ctx.vars.alloc_plan_column_id(),
                    ret_type: Arc::new(expr.get_type(&*ectx).into_owned()),
                    ..Column::default()
                })]);
            }
        }
        schema
    }

    /// BuildKeyInfo implements base.LogicalPlan.<4th> interface: the projection's own
    /// schema is `selfSchema`, the child's keys are mapped through the expressions.
    // go: LogicalProjection.BuildKeyInfo L1 [M-PLAN]
    pub fn build_key_info(&mut self, ctx: &mut PlanCtx<'_>) {
        // `LogicalProjection` use schema from `Exprs` to build key info. See `buildSchemaByExprs`.
        // So call `baseLogicalPlan.BuildKeyInfo` here to avoid duplicated building key info.
        crate::logicalop::base_logical_plan::build_key_info(self, &ctx.arena);
        self.schema_mut(&mut ctx.arena).keys = Vec::new();
        let self_schema = self.schema(&ctx.arena).clone();
        let schema = self.build_schema_by_exprs(ctx, &self_schema);
        let child_keys = ctx
            .arena
            .get(self.children()[0])
            .schema(&ctx.arena)
            .keys
            .clone();
        let mut keys = Vec::new();
        for key in &child_keys {
            let Some(indices) = schema.columns_indices(key) else {
                continue;
            };
            let new_key: Vec<Arc<Column>> = indices
                .iter()
                .map(|&i| Arc::clone(&self_schema.columns[i as usize]))
                .collect();
            keys.push(new_key);
        }
        self.schema_mut(&mut ctx.arena).keys = keys;
    }

    /// TryToGetChildProp will check if this sort property can be pushed or not.
    /// When a sort column will be replaced by scalar function, we refuse it.
    /// When a sort column will be replaced by a constant, we just remove it.
    ///
    /// Go's `(nil, false)` is `None`.
    // go: LogicalProjection.TryToGetChildProp L1
    pub fn try_to_get_child_prop(
        &self,
        ctx: &PlanCtx<'_>,
        prop: &crate::property::PhysicalProperty,
    ) -> Option<crate::property::PhysicalProperty> {
        let mut new_prop = prop.clone_essential_fields();
        if !prop.sort_items.is_empty() {
            let new_sort_items = self.try_transform_sort_items(ctx, &prop.sort_items)?;
            new_prop.sort_items = new_sort_items;
        }
        if let Some(partial_order_info) = &prop.partial_order_info {
            let new_partial_order_items =
                self.try_transform_sort_item_ptrs(ctx, &partial_order_info.sort_items)?;
            new_prop.partial_order_info = Some(Arc::new(crate::property::PartialOrderInfo {
                sort_items: new_partial_order_items,
            }));
        }
        Some(new_prop)
    }

    // go: LogicalProjection.tryTransformSortItems L1
    fn try_transform_sort_items(
        &self,
        ctx: &PlanCtx<'_>,
        items: &[SortItem],
    ) -> Option<Vec<SortItem>> {
        let mut new_items = Vec::with_capacity(items.len());
        for item in items {
            let idx = self.schema(&ctx.arena).column_index(&item.col);
            match &*self.exprs[idx as usize] {
                Expr::Column(expr) => new_items.push(SortItem {
                    col: Arc::new(expr.clone()),
                    desc: item.desc,
                }),
                Expr::ScalarFunction(_) => return None,
                _ => {}
            }
        }
        Some(new_items)
    }

    /// `tryTransformSortItemPtrs`: `tryTransformSortItems` over Go's `[]*SortItem` of the
    /// partial order property (a `Vec<SortItem>` here).
    // go: LogicalProjection.tryTransformSortItemPtrs L1
    fn try_transform_sort_item_ptrs(
        &self,
        ctx: &PlanCtx<'_>,
        items: &[SortItem],
    ) -> Option<Vec<SortItem>> {
        let mut new_items = Vec::with_capacity(items.len());
        for item in items {
            let idx = self.schema(&ctx.arena).column_index(&item.col);
            match &*self.exprs[idx as usize] {
                Expr::Column(expr) => new_items.push(SortItem {
                    col: Arc::new(expr.clone()),
                    desc: item.desc,
                }),
                Expr::ScalarFunction(_) => return None,
                _ => {}
            }
        }
        Some(new_items)
    }

    /// AppendExpr adds the expression to the projection.
    // go: LogicalProjection.AppendExpr L1 [M-CTX, M-FT]
    pub fn append_expr(&mut self, ctx: &mut PlanCtx<'_>, mut expr: ExprRef) -> Arc<Column> {
        if let Expr::Column(col) = &*expr {
            return Arc::new(col.clone());
        }
        expr = column_substitute(&*ctx.expr_ctx, &expr, self.schema(&ctx.arena), &self.exprs);
        self.exprs.push(Arc::clone(&expr));
        let ectx = ctx.eval_ctx();
        let mut col = Column {
            unique_id: ctx.vars.alloc_plan_column_id(),
            ret_type: Arc::new(expr.get_type(&*ectx).into_owned()),
            ..Column::default()
        };
        col.collation_info.set_coercibility(expr.coercibility());
        col.collation_info.set_repertoire(expr.repertoire());
        // reset ParseToJSONFlag in order to keep the flag away from json column
        // (Go appends the column first and changes its fresh type afterwards: same result).
        if col.ret_type.get_type() == TypeJSON {
            Arc::make_mut(&mut col.ret_type).del_flag(ParseToJSONFlag);
        }
        let col = Arc::new(col);
        self.schema_mut(&mut ctx.arena)
            .append(vec![Arc::clone(&col)]);
        col
    }
}

// go-exception: EXC.B LogicalProjection.Hash64 (cascades base.HashEquals; D08.1 spec §4 item 2)
// go-exception: EXC.B LogicalProjection.Equals (cascades base.HashEquals; D08.1 spec §4 item 2)
// glue: stub of LogicalProjection.HashCode, blocked-by:D14.2 (plancodec.TypeStringToPhysicalID, util/plancodec)
// glue: stub of LogicalProjection.ExtractFD, blocked-by:D09.14 (pkg/planner/funcdep is not ported)
/// The boundary of the stubbed `LogicalProjection` functions above.
pub fn projection_boundary(block: &str, what: &str) -> Error {
    blocked(block, what)
}

impl LogicalPlan for LogicalProjection {
    crate::logical_plan_boilerplate!(Projection, producer);

    /// DeriveStats implements base.LogicalPlan.<11th> interface.
    // go: LogicalProjection.DeriveStats L1 [M-PLAN]
    fn derive_stats(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        _this: PlanId,
        child_stats: &[Arc<crate::property::StatsInfo>],
        col_groups: &[Vec<Arc<Column>>],
    ) -> Result<Arc<crate::property::StatsInfo>, Error> {
        let child_profile = &child_stats[0];
        if let Some(s) = self.base.plan.stats_info() {
            // Reload GroupNDVs since colGroups may have changed.
            let mut stats = crate::property::StatsInfo::clone(s);
            stats.group_ndvs =
                self.get_group_ndvs(col_groups, child_profile, self.schema(&ctx.arena));
            let stats = Arc::new(stats);
            self.base.plan.set_stats(Some(Arc::clone(&stats)));
            return Ok(stats);
        }
        let mut stats = crate::property::StatsInfo {
            row_count: child_profile.row_count,
            col_ndvs: HashMap::with_capacity(self.exprs.len()),
            ..Default::default()
        };
        let child_schema = ctx.arena.get(self.base.children[0]).schema(&ctx.arena);
        let self_schema = self.sp.schema(&self.base.children, &ctx.arena);
        let mut cols = Vec::with_capacity(8);
        for (i, expr) in self.exprs.iter().enumerate() {
            cols = tidb_expression::util::extract_all_columns_from_expressions_in_used_slices(
                cols,
                None,
                std::slice::from_ref(expr),
            );
            let (ndv, _) = crate::cardinality::ndv::estimate_cols_ndv_with_matched_len(
                &cols,
                child_schema,
                child_profile,
            );
            stats.col_ndvs.insert(self_schema.columns[i].unique_id, ndv);
            cols.clear();
        }
        stats.group_ndvs = self.get_group_ndvs(col_groups, child_profile, self_schema);
        let stats = Arc::new(stats);
        self.base.plan.set_stats(Some(Arc::clone(&stats)));
        Ok(stats)
    }

    /// ExtractColGroups implements base.LogicalPlan.<12th> interface.
    // go: LogicalProjection.ExtractColGroups L1 [M-PLAN]
    fn extract_col_groups(
        &self,
        arena: &crate::PlanArena,
        col_groups: &[Vec<Arc<Column>>],
    ) -> Vec<Vec<Arc<Column>>> {
        if col_groups.is_empty() {
            return Vec::new();
        }
        let (ext_col_groups, _) = self.schema(arena).extract_col_groups(col_groups);
        let ext_col_groups = ext_col_groups.unwrap_or_default();
        if ext_col_groups.is_empty() {
            return Vec::new();
        }
        let mut extracted = Vec::with_capacity(ext_col_groups.len());
        for cols in &ext_col_groups {
            let mut exprs = Vec::with_capacity(cols.len());
            let mut all_cols = true;
            for &offset in cols {
                // TODO: for functional dependent projections like `col1 + 1` -> `col2`, we can maintain GroupNDVs actually.
                let Expr::Column(col) = &*self.exprs[offset as usize] else {
                    all_cols = false;
                    break;
                };
                exprs.push(Arc::new(col.clone()));
            }
            if all_cols {
                extracted.push(tidb_expression::column::sort_columns(&exprs));
            }
        }
        extracted
    }

    /// PreparePossibleProperties implements base.LogicalPlan.<13th> interface.
    // go: LogicalProjection.PreparePossibleProperties L1 [M-PLAN]
    fn prepare_possible_properties(
        &self,
        arena: &crate::PlanArena,
        mut children_properties: Vec<Vec<Vec<Arc<Column>>>>,
    ) -> Vec<Vec<Arc<Column>>> {
        let child_properties = children_properties.swap_remove(0);
        let schema = self.schema(arena);
        let mut old_cols = Vec::with_capacity(schema.len());
        let mut new_cols = Vec::with_capacity(schema.len());
        for (i, expr) in self.exprs.iter().enumerate() {
            if let Expr::Column(col) = &**expr {
                new_cols.push(Arc::clone(&schema.columns[i]));
                old_cols.push(Arc::new(col.clone()));
            }
        }
        let tmp_schema = tidb_expression::new_schema(old_cols);
        let mut new_properties = Vec::with_capacity(child_properties.len());
        for child_property in &child_properties {
            let mut new_child_property = Vec::with_capacity(child_property.len());
            for col in child_property {
                let pos = tmp_schema.column_index(col);
                if pos < 0 {
                    break;
                }
                new_child_property.push(Arc::clone(&new_cols[pos as usize]));
            }
            if !new_child_property.is_empty() {
                new_properties.push(new_child_property);
            }
        }
        new_properties
    }

    /// ExhaustPhysicalPlans implements base.LogicalPlan.<14th> interface.
    // go: LogicalProjection.ExhaustPhysicalPlans L1 [M-PLAN]
    fn exhaust_physical_plans(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        _this: PlanId,
        prop: &crate::property::PhysicalProperty,
    ) -> Result<(Vec<crate::PhysicalPlanRef>, bool), Error> {
        crate::core::exhaust_physical_plans::exhaust_physical_plans4_logical_projection(
            self, ctx, prop,
        )
    }

    /// ExplainInfo implements Plan interface.
    // go: LogicalProjection.ExplainInfo L2 (expression.ExplainExpressionList is blocked-by:D10.9)
    fn explain_info(&self, _ctx: &PlanCtx<'_>) -> Result<String, Error> {
        Err(blocked(
            "D10.9",
            "the EXPLAIN text of an expression list (ExplainExpressionList)",
        ))
    }

    /// ReplaceExprColumns implements base.LogicalPlan interface.
    // go: LogicalProjection.ReplaceExprColumns L1 [M-EXPR]
    fn replace_expr_columns(&mut self, replace: &ReplaceMap) {
        for expr in self.exprs.iter_mut() {
            resolve_expr_and_replace(expr, replace);
        }
    }

    /// HashCode implements base.LogicalPlan.<0th> interface.
    // glue: dispatch to the HashCode boundary stub above
    fn hash_code(&self) -> Result<Vec<u8>, Error> {
        Err(projection_boundary(
            "D14.2",
            "plancodec.TypeStringToPhysicalID (LogicalProjection.HashCode)",
        ))
    }

    /// PredicatePushDown implements base.LogicalPlan.<1st> interface.
    // go: LogicalProjection.PredicatePushDown L1
    fn predicate_push_down(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        this: PlanId,
        predicates: Vec<ExprRef>,
        opt: &LogicalOptimizeOp,
    ) -> Result<(Vec<ExprRef>, PlanId), Error> {
        for expr in &self.exprs {
            if has_assign_set_var_func(expr) {
                let (_, child) = crate::logicalop::base_logical_plan::predicate_push_down(
                    self,
                    ctx,
                    this,
                    Vec::new(),
                    opt,
                )?;
                return Ok((predicates, child));
            }
        }
        let (can_be_pushed, can_not_be_pushed) = break_down_predicates(self, ctx, &predicates);
        let (mut remained, child) = crate::logicalop::base_logical_plan::predicate_push_down(
            self,
            ctx,
            this,
            can_be_pushed,
            opt,
        )?;
        remained.extend(can_not_be_pushed);
        Ok((remained, child))
    }

    /// PruneColumns implements base.LogicalPlan.<2nd> interface.
    /// If any expression has SetVar function or Sleep function, we do not prune it.
    // go: LogicalProjection.PruneColumns L1 [M-PLAN, M-FT]
    fn prune_columns(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        this: PlanId,
        parent_used_cols: Vec<Arc<Column>>,
        opt: &LogicalOptimizeOp,
    ) -> Result<PlanId, Error> {
        let ectx = ctx.eval_ctx();
        let mut used = get_used_list(&*ectx, &parent_used_cols, self.schema(&ctx.arena));
        let mut pruned_columns = Vec::new();
        let mut all_pruned = true;
        for (i, b) in used.iter_mut().enumerate() {
            if *b || expr_has_set_var_or_sleep(&self.exprs[i]) {
                // Set to true to avoid the ExprHasSetVarOrSleep be called multiple times.
                *b = true;
                all_pruned = false;
                break;
            }
        }
        if all_pruned && ctx.arena.get(self.children()[0]).kind() == LogicalKind::TableDual {
            // If the child is dual. The proj should not be eliminated.
            // When the dual is generated by `select ....;`` directly(No source SELECT), the dual is created without any output cols.
            // The dual only holds a RowCount, and the proj is used to generate the output cols.
            // But we need reset the used columns in its expression since there can be correlated columns in the expression.
            // e.g. SELECT 1 FROM t1 WHERE TRUE OR ( SELECT 1 FROM (SELECT a) q ) = 1;
            //   The `SELECT a` will create a projection whose expression is the correlated column `a` from outer table t1.
            //   We need to eliminate it so that our decorrelation can work as expected.
            self.exprs.truncate(1);
            self.schema_mut(&mut ctx.arena).columns.truncate(1);
            self.exprs[0] = Expr::Constant(new_zero()).into_ref();
            let col = Column {
                unique_id: ctx.vars.alloc_plan_column_id(),
                // Go shares the constant's *FieldType; a copy (M-FT: neither is changed later).
                ret_type: Arc::new(self.exprs[0].get_type(&*ectx).into_owned()),
                ..Column::default()
            };
            self.schema_mut(&mut ctx.arena).columns[0] = Arc::new(col);
            return Ok(this);
        }
        // for implicit projected cols, once the ancestor doesn't use it, the implicit expr will be automatically pruned here.
        for i in (0..used.len()).rev() {
            if !used[i] && !expr_has_set_var_or_sleep(&self.exprs[i]) {
                pruned_columns.push(Arc::clone(&self.schema(&ctx.arena).columns[i]));
                self.schema_mut(&mut ctx.arena).columns.remove(i);
                self.exprs.remove(i);
            }
        }
        logicaltrace::append_column_prune_trace_step(self, ctx, &pruned_columns, opt);
        let self_used_cols = extract_columns_from_expressions(
            Vec::with_capacity(self.exprs.len()),
            &self.exprs,
            None,
        );
        let child = self.children()[0];
        self.children_mut()[0] = base::prune_columns(ctx, child, self_used_cols, opt)?;
        // If its columns are all pruned, we directly use its child. The child will output at least one column.
        if self.schema(&ctx.arena).is_empty() {
            return Ok(self.children()[0]);
        }
        Ok(this)
    }

    /// PushDownTopN implements base.LogicalPlan.<5th> interface.
    // go: LogicalProjection.PushDownTopN L1 [M-PLAN]
    fn push_down_top_n(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        this: PlanId,
        top_n: Option<PlanId>,
        opt: &LogicalOptimizeOp,
    ) -> PlanId {
        for expr in &self.exprs {
            if has_assign_set_var_func(expr) {
                return crate::logicalop::base_logical_plan::push_down_top_n(
                    self, ctx, this, top_n, opt,
                );
            }
        }
        if let Some(top_n) = top_n {
            let by_items = ctx
                .arena
                .downcast_ref::<LogicalTopN>(top_n)
                .expect("LogicalTopN")
                .by_items
                .clone();
            let mut substituted_exprs = Vec::with_capacity(by_items.len());
            for by in &by_items {
                let substituted = fold_constant(
                    &*ctx.expr_ctx,
                    column_substitute(
                        &*ctx.expr_ctx,
                        &by.expr,
                        self.schema(&ctx.arena),
                        &self.exprs,
                    ),
                );
                if !is_immutable_func(&substituted) {
                    // after substituting, if the order-by expression is un-deterministic like 'order by rand()', stop pushing down.
                    return crate::logicalop::base_logical_plan::push_down_top_n(
                        self,
                        ctx,
                        this,
                        Some(top_n),
                        opt,
                    );
                }
                // if topN.ByItems contains a column(with ID=0) generated by projection,
                // projection will prevent the optimizer from pushing topN down.
                let cols = extract_columns(&substituted);
                for col in &cols {
                    if col.id == 0 && self.schema(&ctx.arena).contains(col) {
                        // check whether the column is generated by projection
                        let child = self.children()[0];
                        if !ctx.arena.get(child).schema(&ctx.arena).contains(col) {
                            self.children_mut()[0] = base::push_down_top_n(ctx, child, None, opt);
                            return LogicalTopN::attach_child_to(ctx, top_n, this, self, opt);
                        }
                    }
                }
                substituted_exprs.push(substituted);
            }
            // Go writes `by.Expr` through the shared *ByItems (the sort's items when the
            // TopN came from a Limit over a Sort); the sort is no longer in the tree then, so
            // the TopN's own copy is enough (M-FT audit).
            let top_n_node = ctx
                .arena
                .downcast_mut::<LogicalTopN>(top_n)
                .expect("LogicalTopN");
            for (i, by) in top_n_node.by_items.iter_mut().enumerate() {
                by.expr = Arc::clone(&substituted_exprs[i]);
            }
            // remove meaningless constant sort items.
            for i in (0..top_n_node.by_items.len()).rev() {
                if matches!(
                    &*top_n_node.by_items[i].expr,
                    Expr::Constant(_) | Expr::CorrelatedColumn(_)
                ) {
                    top_n_node.by_items.remove(i);
                }
            }
            let child = self.children()[0];
            self.children_mut()[0] = base::push_down_top_n(ctx, child, Some(top_n), opt);
            return this;
        }
        let child = self.children()[0];
        self.children_mut()[0] = base::push_down_top_n(ctx, child, None, opt);
        this
    }

    /// PullUpConstantPredicates implements base.LogicalPlan.<9th> interface.
    // go: LogicalProjection.PullUpConstantPredicates L1 [M-EXPR]
    fn pull_up_constant_predicates(&self, ctx: &PlanCtx<'_>) -> Vec<ExprRef> {
        // projection has no column expr
        if !can_projection_be_eliminated_loose(self) {
            return Vec::new();
        }
        let candidate_constant_predicates = ctx
            .arena
            .get(self.children()[0])
            .pull_up_constant_predicates(ctx);
        // replace predicate by projection expr
        // candidate predicate : a=1
        // projection: a as a'
        // result predicate : a'=1
        let mut replace: ReplaceMap = HashMap::new();
        for (i, expr) in self.exprs.iter().enumerate() {
            replace.insert(
                expr.hash_code().to_vec(),
                Arc::clone(&self.schema(&ctx.arena).columns[i]),
            );
        }
        let mut result = Vec::with_capacity(candidate_constant_predicates.len());
        for predicate in candidate_constant_predicates {
            // The column of predicate must exist in projection exprs
            let columns = extract_columns(&predicate);
            // The number of columns in candidate predicate must be 1.
            if columns.len() != 1 {
                continue;
            }
            if !replace.contains_key(columns[0].hash_code()) {
                // The column of predicate will not appear on the upper level
                // This means that this predicate does not apply to the constant propagation optimization rule
                // For example: select * from t, (select b from s where s.a=1) tmp where t.b=s.b
                continue;
            }
            // clonePredicate := predicate.Clone(): the replacement below copies on write.
            let mut clone_predicate = predicate;
            resolve_expr_and_replace(&mut clone_predicate, &replace);
            result.push(clone_predicate);
        }
        result
    }

    /// ExtractCorrelatedCols implements base.LogicalPlan.<15th> interface.
    // go: LogicalProjection.ExtractCorrelatedCols L1
    fn extract_correlated_cols(&self) -> Vec<Arc<CorrelatedColumn>> {
        let mut cor_cols = Vec::with_capacity(self.exprs.len());
        for expr in &self.exprs {
            cor_cols.extend(extract_cor_columns(expr));
        }
        cor_cols
    }

    /// ConvertOuterToInnerJoin implements base.LogicalPlan.<24th> interface.
    // go: LogicalProjection.ConvertOuterToInnerJoin L1
    fn convert_outer_to_inner_join(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        this: PlanId,
        predicates: Vec<ExprRef>,
    ) -> PlanId {
        let (can_be_pushed, _) = break_down_predicates(self, ctx, &predicates);
        let mut child = self.children()[0];
        child = base::convert_outer_to_inner_join(ctx, child, can_be_pushed);
        self.set_children(vec![child]);
        this
    }
}

/// breakDownPredicates breaks down predicates into two sets: canBePushed and
/// cannotBePushed. It also maps columns to projection schema.
// go: breakDownPredicates L1 [M-CTX]
fn break_down_predicates(
    p: &LogicalProjection,
    ctx: &PlanCtx<'_>,
    predicates: &[ExprRef],
) -> (Vec<ExprRef>, Vec<ExprRef>) {
    let mut can_be_pushed = Vec::with_capacity(predicates.len());
    let mut can_not_be_pushed = Vec::with_capacity(predicates.len());
    for cond in predicates {
        let (substituted, has_failed, new_filter) =
            column_substitute_impl(&*ctx.expr_ctx, cond, p.schema(&ctx.arena), &p.exprs, true);
        if substituted && !has_failed && !has_get_set_var_func(&new_filter) {
            can_be_pushed.push(new_filter);
        } else {
            can_not_be_pushed.push(Arc::clone(cond));
        }
    }
    (can_be_pushed, can_not_be_pushed)
}

/// todo: merge with rule_eliminate_projection.go
// go: pkg/planner/core/operator/logicalop/logical_projection.go canProjectionBeEliminatedLoose L1
pub fn can_projection_be_eliminated_loose(p: &LogicalProjection) -> bool {
    // project for expand will assign a new col id for col ref, because these column should be
    // data cloned in the execution time and may be filled with null value at the same time.
    // so it's not a REAL column reference. Detect the column ref in projection here and do
    // the elimination here will restore the Expand's grouping sets column back to use the
    // original column ref again. (which is not right)
    if p.proj4_expand {
        return false;
    }
    for expr in &p.exprs {
        if !matches!(&**expr, Expr::Column(_)) {
            return false;
        }
    }
    true
}

/// InjectExpr injects the expr into a projection above p, and returns the new projection
/// and the new column.
// go: InjectExpr L1 [M-PLAN]
pub fn inject_expr(ctx: &mut PlanCtx<'_>, p: PlanId, expr: ExprRef) -> (PlanId, Arc<Column>) {
    let proj = if ctx.arena.get(p).kind() == LogicalKind::Projection {
        p
    } else {
        let node = ctx.arena.get(p);
        let schema = node.schema(&ctx.arena).clone_schema();
        let mut proj = LogicalProjection::init(
            column2_exprs(&schema.columns),
            ctx,
            node.query_block_offset(),
        );
        proj.sp.set_schema(Some(schema));
        proj.set_children(vec![p]);
        ctx.arena.alloc(Box::new(proj))
    };
    let col = crate::with_detached(ctx, proj, |node, ctx| {
        node.as_any_mut()
            .downcast_mut::<LogicalProjection>()
            .expect("LogicalProjection")
            .append_expr(ctx, expr)
    });
    (proj, col)
}

impl LogicalProjection {
    // go: LogicalProjection.getGroupNDVs L1
    fn get_group_ndvs(
        &self,
        col_groups: &[Vec<Arc<Column>>],
        child_profile: &crate::property::StatsInfo,
        self_schema: &tidb_expression::Schema,
    ) -> Vec<crate::property::GroupNDV> {
        if col_groups.is_empty() || child_profile.group_ndvs.is_empty() {
            return Vec::new();
        }
        let mut expr_col2_proj_col: HashMap<i64, i64> = HashMap::new();
        for (i, expr) in self.exprs.iter().enumerate() {
            let Expr::Column(expr_col) = &**expr else {
                continue;
            };
            expr_col2_proj_col.insert(expr_col.unique_id, self_schema.columns[i].unique_id);
        }
        let mut ndvs = Vec::with_capacity(child_profile.group_ndvs.len());
        for child_group_ndv in &child_profile.group_ndvs {
            let mut proj_cols = Some(vec![0i64; child_group_ndv.cols.len()]);
            for (i, col) in child_group_ndv.cols.iter().enumerate() {
                let Some(&proj_col) = expr_col2_proj_col.get(col) else {
                    proj_cols = None;
                    break;
                };
                proj_cols.as_mut().expect("set above")[i] = proj_col;
            }
            let Some(mut proj_cols) = proj_cols else {
                continue;
            };
            proj_cols.sort();
            ndvs.push(crate::property::GroupNDV {
                cols: proj_cols,
                ndv: child_group_ndv.ndv,
            });
        }
        ndvs
    }
}
