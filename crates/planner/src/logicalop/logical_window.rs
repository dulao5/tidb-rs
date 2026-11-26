//! go-file: pkg/planner/core/operator/logicalop/logical_window.go
//!
//! `LogicalWindow`, `WindowFrame`, and `FrameBound`: the logical window operator (block D09.4
//! part B, plus its co-ported logical rule methods from D09.7 / D09.8 / D10.6 / D10.2 / D09.6).
//!
//! - `WindowFuncDescs` are stored by value (`Vec<WindowFuncDesc>`, matching D08.11 and D09.4).
//! - Cascades-only methods (`EqualPartitionBy`, `EqualOrderBy`, `EqualFrame`) are not ported
//!   (`EXC.B`).
//! - `ExhaustPhysicalPlans` (`blocked-by:D10.3`) is a temporary boundary stub.

#![allow(non_upper_case_globals)]

use std::collections::HashMap;
use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_expr_pushdown::expr_to_pb::expressions_to_pb_list;
use tidb_expression::aggregation::WindowFuncDesc;
use tidb_expression::builtin_compare::{CompareFunc, get_accurate_cmp_type};
use tidb_expression::expression::EvalState;
use tidb_expression::scalar_function::new_function_base;
use tidb_expression::schema::expr_from_schema;
use tidb_expression::util::{extract_columns, extract_cor_columns};
use tidb_expression::{Column, CorrelatedColumn, Expr, ExprRef, new_schema};
use tidb_parser::ast::{self, funcs as ast_funcs};
use tidb_parser::types::{ETDatetime, ETDecimal, ETDuration, ETInt, ETReal, ETTimestamp, EvalType};
use tidb_sessionctx::exprctx::BuildContext;
use tidb_sessionctx::planctx::BuildPBContext;

use crate::arena::{PlanArena, PlanId};
use crate::base::{self, LogicalPlan, ReplaceMap};
use crate::core::rule_util::{resolve_column_and_replace, resolve_expr_and_replace};
use crate::logicalop::base_logical_plan::{BaseLogicalPlan, new_base_logical_plan};
use crate::logicalop::logical_schema_producer::LogicalSchemaProducer;
use crate::physicalop::PhysicalPlanRef;
use crate::planctx::PlanCtx;
use crate::property::{
    GroupNDV, MPPPartitionColumn, PhysicalProperty, SortItem, StatsInfo,
    get_collate_id_by_name_for_partition,
};
use crate::util::optimizetrace::LogicalOptimizeOp;

/// `plancodec.TypeWindow` (D14.2).
const TYPE_WINDOW: &str = "Window";

/// LogicalWindow represents a logical window function plan.
#[derive(Debug)]
pub struct LogicalWindow {
    /// Embedded `BaseLogicalPlan` (via `LogicalSchemaProducer`).
    pub base: BaseLogicalPlan,
    /// Embedded `LogicalSchemaProducer`.
    pub sp: LogicalSchemaProducer,

    /// Window function descriptors.
    pub window_func_descs: Vec<WindowFuncDesc>,
    /// PARTITION BY sort items.
    pub partition_by: Vec<SortItem>,
    /// ORDER BY sort items.
    pub order_by: Vec<SortItem>,
    /// Window frame specification, if any.
    pub frame: Option<WindowFrame>,
}

/// WindowFrame represents a window function frame.
#[derive(Clone, Debug)]
pub struct WindowFrame {
    /// Frame type (`ROWS`, `RANGE`, `GROUPS`).
    pub tp: ast::FrameType,
    /// Start frame bound.
    pub start: Option<FrameBound>,
    /// End frame bound.
    pub end: Option<FrameBound>,
}

impl WindowFrame {
    /// Clone copies a window frame totally.
    // go: WindowFrame.Clone L1 [M-NAME]
    pub fn clone_frame(&self) -> WindowFrame {
        WindowFrame {
            tp: self.tp,
            start: self.start.as_ref().map(FrameBound::clone_bound),
            end: self.end.as_ref().map(FrameBound::clone_bound),
        }
    }
}

/// FrameBound is the boundary of a frame.
#[derive(Clone, Debug)]
pub struct FrameBound {
    /// Bound type (`FOLLOWING`, `PRECEDING`, `CURRENT ROW`).
    pub tp: ast::BoundType,
    /// Whether this bound is `UNBOUNDED`.
    pub unbounded: bool,
    /// Numeric offset for `ROWS` frames.
    pub num: u64,
    /// CalcFuncs is used for range framed windows.
    /// We will build the date_add or date_sub functions for frames like `INTERVAL '2:30' MINUTE_SECOND FOLLOWING`,
    /// and plus or minus for frames like `1 preceding`.
    pub calc_funcs: Vec<ExprRef>,
    /// Sometimes we need to cast order by column to a specific type when frame type is range.
    pub compare_cols: Vec<ExprRef>,
    /// CmpFuncs is used to decide whether one row is included in the current frame.
    pub cmp_funcs: Vec<Option<CompareFunc>>,
    /// This field is used for passing information to tiflash.
    pub cmp_data_type: tipb::RangeCmpDataType,
    /// IsExplicitRange marks if this range explicitly appears in the sql.
    pub is_explicit_range: bool,
}

impl Default for FrameBound {
    // glue: Go zero value for FrameBound
    fn default() -> Self {
        Self {
            tp: ast::BoundType::Following,
            unbounded: false,
            num: 0,
            calc_funcs: Vec::new(),
            compare_cols: Vec::new(),
            cmp_funcs: Vec::new(),
            cmp_data_type: tipb::RangeCmpDataType::Int,
            is_explicit_range: false,
        }
    }
}

impl FrameBound {
    /// Clone copies a frame bound totally.
    // go: FrameBound.Clone L1 [M-NAME, M-EXPR]
    pub fn clone_bound(&self) -> FrameBound {
        FrameBound {
            tp: self.tp,
            unbounded: self.unbounded,
            num: self.num,
            calc_funcs: self.calc_funcs.clone(),
            compare_cols: self.compare_cols.clone(),
            cmp_funcs: self.cmp_funcs.clone(),
            cmp_data_type: self.cmp_data_type,
            is_explicit_range: self.is_explicit_range,
        }
    }

    /// UpdateCmpFuncsAndCmpDataType updates CmpFuncs and CmpDataType.
    // go: FrameBound.UpdateCmpFuncsAndCmpDataType L1
    pub fn update_cmp_funcs_and_cmp_data_type(&mut self, cmp_data_type: EvalType) {
        // When cmpDataType can't match to any condition, we can ignore it.
        //
        // For example:
        //   `create table test.range_test(p int not null,o text not null,v int not null);`
        //   `select *, first_value(v) over (partition by p order by o) as a from range_test;`
        //   The sql's frame type is range, but the cmpDataType is ETString and when the user explicitly use range frame
        //   the sql will raise error before generating logical plan, so it's ok to ignore it.
        match cmp_data_type {
            ETInt => {
                self.cmp_funcs[0] = Some(CompareFunc::Int);
                self.cmp_data_type = tipb::RangeCmpDataType::Int;
            }
            ETDatetime | ETTimestamp => {
                self.cmp_funcs[0] = Some(CompareFunc::Time);
                self.cmp_data_type = tipb::RangeCmpDataType::DateTime;
            }
            ETDuration => {
                self.cmp_funcs[0] = Some(CompareFunc::Duration);
                self.cmp_data_type = tipb::RangeCmpDataType::Duration;
            }
            ETReal => {
                self.cmp_funcs[0] = Some(CompareFunc::Real);
                self.cmp_data_type = tipb::RangeCmpDataType::Float;
            }
            ETDecimal => {
                self.cmp_funcs[0] = Some(CompareFunc::Decimal);
                self.cmp_data_type = tipb::RangeCmpDataType::Decimal;
            }
            _ => {}
        }
    }

    /// ToPB converts FrameBound to tipb structure.
    // go: FrameBound.ToPB L1 [M-CTX]
    pub fn to_pb(&self, ctx: &BuildPBContext) -> Result<tipb::WindowFrameBound, Error> {
        let mut pb_bound = tipb::WindowFrameBound::default();
        pb_bound.set_type(match self.tp {
            ast::BoundType::Following => tipb::WindowBoundType::Following,
            ast::BoundType::Preceding => tipb::WindowBoundType::Preceding,
            ast::BoundType::CurrentRow => tipb::WindowBoundType::CurrentRow,
        });
        pb_bound.set_unbounded(self.unbounded);
        pb_bound.set_offset(self.num);

        if self.is_explicit_range {
            let expr_ctx = ctx.get_expr_ctx();
            let eval_ctx = expr_ctx.get_eval_ctx();
            let client = ctx.get_client();
            let mut range_frame =
                expressions_to_pb_list(&*eval_ctx, &EvalState::new(), &self.calc_funcs, &*client)?;
            pb_bound.set_frame_range(range_frame.swap_remove(0));
            pb_bound.set_cmp_data_type(self.cmp_data_type);
        }

        Ok(pb_bound)
    }

    /// UpdateCompareCols will update CompareCols.
    // go: FrameBound.UpdateCompareCols L1 [M-CTX]
    pub fn update_compare_cols(
        &mut self,
        ctx: &dyn BuildContext,
        order_by_cols: &[Arc<Column>],
    ) -> Result<(), Error> {
        if !self.calc_funcs.is_empty() {
            let need_cast = {
                let ectx = ctx.get_eval_ctx();
                self.calc_funcs[0].get_type(&*ectx).eval_type()
                    != order_by_cols[0].get_static_type().eval_type()
            };
            if need_cast {
                let target_tp = self.calc_funcs[0]
                    .get_type(&*ctx.get_eval_ctx())
                    .into_owned();
                let cast_expr = new_function_base(
                    ctx,
                    ast_funcs::Cast,
                    target_tp,
                    vec![Expr::Column(Column::clone(&order_by_cols[0])).into_ref()],
                )?;
                self.compare_cols = vec![cast_expr];
            } else {
                self.compare_cols = order_by_cols
                    .iter()
                    .map(|col| Expr::Column(Column::clone(col)).into_ref())
                    .collect();
            }

            let cmp_data_type = get_accurate_cmp_type(
                &*ctx.get_eval_ctx(),
                &self.compare_cols[0],
                &self.calc_funcs[0],
            );
            self.update_cmp_funcs_and_cmp_data_type(cmp_data_type);
        }
        Ok(())
    }
}

// go-exception: EXC.B LogicalWindow.EqualPartitionBy (only called by legacy cascades transformation rules)
// go-exception: EXC.B LogicalWindow.EqualOrderBy (only called by legacy cascades transformation rules)
// go-exception: EXC.B LogicalWindow.EqualFrame (only called by legacy cascades transformation rules)

impl LogicalWindow {
    /// Init initializes LogicalWindow.
    // go: LogicalWindow.Init L1 [M-PLAN, M-CTX]
    pub fn init(
        window_func_descs: Vec<WindowFuncDesc>,
        partition_by: Vec<SortItem>,
        order_by: Vec<SortItem>,
        frame: Option<WindowFrame>,
        ctx: &mut PlanCtx<'_>,
        offset: isize,
    ) -> LogicalWindow {
        LogicalWindow {
            base: new_base_logical_plan(ctx, TYPE_WINDOW, offset),
            sp: LogicalSchemaProducer::default(),
            window_func_descs,
            partition_by,
            order_by,
            frame,
        }
    }

    /// GetPartitionBy returns partition by fields.
    // go: LogicalWindow.GetPartitionBy L1
    pub fn get_partition_by(&self) -> &[SortItem] {
        &self.partition_by
    }

    /// GetWindowResultColumns returns the columns storing the result of the window function.
    // go: LogicalWindow.GetWindowResultColumns L1 [M-PLAN]
    pub fn get_window_result_columns<'a>(&'a self, arena: &'a PlanArena) -> &'a [Arc<Column>] {
        let schema = self.schema(arena);
        &schema.columns[schema.len() - self.window_func_descs.len()..]
    }

    /// GetPartitionKeys gets partition keys for a logical window, it will assign column id for expressions.
    // go: LogicalWindow.GetPartitionKeys L1
    pub fn get_partition_keys(&self) -> Vec<Arc<MPPPartitionColumn>> {
        let mut partition_by_cols = Vec::with_capacity(self.partition_by.len());
        for item in &self.partition_by {
            partition_by_cols.push(Arc::new(MPPPartitionColumn {
                col: Arc::clone(&item.col),
                collate_id: get_collate_id_by_name_for_partition(
                    item.col.get_static_type().get_collate(),
                ),
            }));
        }
        partition_by_cols
    }

    /// CheckComparisonForTiFlash check Duration vs Datetime is invalid comparison as TiFlash can't handle it so far.
    // go: LogicalWindow.CheckComparisonForTiFlash L1 [M-CTX]
    pub fn check_comparison_for_tiflash(
        &self,
        ctx: &PlanCtx<'_>,
        frame_bound: &FrameBound,
    ) -> bool {
        if !frame_bound.compare_cols.is_empty() {
            let order_by_eval_type = self.order_by[0].col.get_static_type().eval_type();
            let cal_func_eval_type = frame_bound.calc_funcs[0]
                .get_type(&*ctx.eval_ctx())
                .eval_type();

            if (order_by_eval_type == ETDuration
                && (cal_func_eval_type == ETDatetime || cal_func_eval_type == ETTimestamp))
                || (cal_func_eval_type == ETDuration
                    && (order_by_eval_type == ETDatetime || order_by_eval_type == ETTimestamp))
            {
                return false;
            }
        }
        true
    }

    // go: LogicalWindow.extractUsedCols L1
    fn extract_used_cols(&self, mut parent_used_cols: Vec<Arc<Column>>) -> Vec<Arc<Column>> {
        for desc in &self.window_func_descs {
            for arg in &desc.base.args {
                parent_used_cols.extend(extract_columns(arg));
            }
        }
        for by in &self.partition_by {
            parent_used_cols.push(Arc::clone(&by.col));
        }
        for by in &self.order_by {
            parent_used_cols.push(Arc::clone(&by.col));
        }
        parent_used_cols
    }

    /// GetPartitionByCols extracts 'partition by' columns from the Window.
    // go: LogicalWindow.GetPartitionByCols L1
    pub fn get_partition_by_cols(&self) -> Vec<Arc<Column>> {
        let mut partition_cols = Vec::with_capacity(self.partition_by.len());
        for partition_item in &self.partition_by {
            partition_cols.push(Arc::clone(&partition_item.col));
        }
        partition_cols
    }

    /// GetGroupNDVs gets the GroupNDVs of the LogicalWindow.
    // go: LogicalWindow.GetGroupNDVs L1
    pub fn get_group_ndvs(
        &self,
        col_groups: &[Vec<Arc<Column>>],
        child_stats: &[Arc<StatsInfo>],
    ) -> Vec<GroupNDV> {
        if !col_groups.is_empty() {
            return child_stats[0].group_ndvs.clone();
        }
        Vec::new()
    }
}

impl LogicalPlan for LogicalWindow {
    crate::logical_plan_boilerplate!(Window, producer);

    /// ReplaceExprColumns implements base.LogicalPlan interface.
    // go: LogicalWindow.ReplaceExprColumns L1 [M-EXPR]
    fn replace_expr_columns(&mut self, replace: &ReplaceMap) {
        for desc in &mut self.window_func_descs {
            for arg in &mut desc.base.args {
                resolve_expr_and_replace(arg, replace);
            }
        }
        for item in &mut self.partition_by {
            item.col = resolve_column_and_replace(&item.col, replace);
        }
        for item in &mut self.order_by {
            item.col = resolve_column_and_replace(&item.col, replace);
        }
    }

    /// PredicatePushDown implements base.LogicalPlan.<1st> interface.
    // go: LogicalWindow.PredicatePushDown L1 [M-PLAN]
    fn predicate_push_down(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        this: PlanId,
        predicates: Vec<ExprRef>,
        opt: &LogicalOptimizeOp,
    ) -> Result<(Vec<ExprRef>, PlanId), Error> {
        let mut can_be_pushed = Vec::with_capacity(predicates.len());
        let mut can_not_be_pushed = Vec::with_capacity(predicates.len());
        let partition_cols = new_schema(self.get_partition_by_cols());
        for cond in predicates {
            // We can push predicate beneath Window, only if all of the
            // extractedCols are part of partitionBy columns.
            if expr_from_schema(&cond, &partition_cols) {
                can_be_pushed.push(cond);
            } else {
                can_not_be_pushed.push(cond);
            }
        }
        crate::logicalop::base_logical_plan::predicate_push_down(
            self,
            ctx,
            this,
            can_be_pushed,
            opt,
        )?;
        Ok((can_not_be_pushed, this))
    }

    /// PruneColumns implements base.LogicalPlan.<2nd> interface.
    // go: LogicalWindow.PruneColumns L1 [M-PLAN]
    fn prune_columns(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        this: PlanId,
        mut parent_used_cols: Vec<Arc<Column>>,
        opt: &LogicalOptimizeOp,
    ) -> Result<PlanId, Error> {
        let window_columns = self.get_window_result_columns(&ctx.arena).to_vec();
        parent_used_cols.retain(|col| {
            !window_columns
                .iter()
                .any(|wc| wc.unique_id == col.unique_id)
        });
        parent_used_cols = self.extract_used_cols(parent_used_cols);
        let child = self.children()[0];
        let new_child = base::prune_columns(ctx, child, parent_used_cols, opt)?;
        self.children_mut()[0] = new_child;

        let mut new_schema = ctx
            .arena
            .get(self.children()[0])
            .schema(&ctx.arena)
            .clone_schema();
        new_schema.append(window_columns);
        self.sp.set_schema(Some(new_schema));
        Ok(this)
    }

    /// DeriveStats implements base.LogicalPlan.<11th> interface.
    // go: LogicalWindow.DeriveStats L1 [M-PLAN]
    fn derive_stats(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        _this: PlanId,
        child_stats: &[Arc<StatsInfo>],
        col_groups: &[Vec<Arc<Column>>],
    ) -> Result<Arc<StatsInfo>, Error> {
        if let Some(existing) = self.base.plan.stats_info() {
            // Reload GroupNDVs since colGroups may have changed.
            let mut stats = (**existing).clone();
            stats.group_ndvs = self.get_group_ndvs(col_groups, child_stats);
            let stats = Arc::new(stats);
            self.base.plan.set_stats(Some(Arc::clone(&stats)));
            return Ok(stats);
        }
        let child_profile = &child_stats[0];
        let self_schema = self.sp.schema(&self.base.children, &ctx.arena);
        let mut col_ndvs = HashMap::with_capacity(self_schema.len());
        let child_len = self_schema.len() - self.window_func_descs.len();
        for col in &self_schema.columns[..child_len] {
            let id = col.unique_id;
            col_ndvs.insert(id, child_profile.col_ndvs.get(&id).copied().unwrap_or(0.0));
        }
        for col in &self_schema.columns[child_len..] {
            col_ndvs.insert(col.unique_id, child_profile.row_count);
        }
        let group_ndvs = self.get_group_ndvs(col_groups, child_stats);
        let stats = Arc::new(StatsInfo {
            row_count: child_profile.row_count,
            col_ndvs,
            group_ndvs,
            ..Default::default()
        });
        self.base.plan.set_stats(Some(Arc::clone(&stats)));
        Ok(stats)
    }

    /// ExtractColGroups implements base.LogicalPlan.<12th> interface.
    // go: LogicalWindow.ExtractColGroups L1
    fn extract_col_groups(
        &self,
        arena: &PlanArena,
        col_groups: &[Vec<Arc<Column>>],
    ) -> Vec<Vec<Arc<Column>>> {
        if col_groups.is_empty() {
            return Vec::new();
        }
        let child_schema = arena.get(self.children()[0]).schema(arena);
        let (_, offsets) = child_schema.extract_col_groups(col_groups);
        let Some(offsets) = offsets else {
            return Vec::new();
        };
        if offsets.is_empty() {
            return Vec::new();
        }
        offsets
            .into_iter()
            .map(|offset| col_groups[offset].clone())
            .collect()
    }

    /// PreparePossibleProperties implements base.LogicalPlan.<13th> interface.
    // go: LogicalWindow.PreparePossibleProperties L1
    fn prepare_possible_properties(
        &mut self,
        _arena: &PlanArena,
        _children_properties: Vec<Vec<Vec<Arc<Column>>>>,
    ) -> Vec<Vec<Arc<Column>>> {
        let mut result = Vec::with_capacity(self.partition_by.len() + self.order_by.len());
        for item in &self.partition_by {
            result.push(Arc::clone(&item.col));
        }
        for item in &self.order_by {
            result.push(Arc::clone(&item.col));
        }
        vec![result]
    }

    /// ExhaustPhysicalPlans implements base.LogicalPlan.<14th> interface.
    // glue: stub of LogicalWindow.ExhaustPhysicalPlans, blocked-by:D10.3 (physical window enumeration)
    fn exhaust_physical_plans(
        &mut self,
        _ctx: &mut PlanCtx<'_>,
        _this: PlanId,
        _prop: &PhysicalProperty,
    ) -> Result<(Vec<PhysicalPlanRef>, bool), Error> {
        Err(blocked(
            "D10.3",
            "LogicalWindow.ExhaustPhysicalPlans (physical window enumeration)",
        ))
    }

    /// ExtractCorrelatedCols implements base.LogicalPlan.<15th> interface.
    // go: LogicalWindow.ExtractCorrelatedCols L1
    fn extract_correlated_cols(&self) -> Vec<Arc<CorrelatedColumn>> {
        let mut cor_cols = Vec::with_capacity(self.window_func_descs.len());
        for window_func in &self.window_func_descs {
            for arg in &window_func.base.args {
                cor_cols.extend(extract_cor_columns(arg));
            }
        }
        if let Some(frame) = &self.frame {
            if let Some(start) = &frame.start {
                for expr in &start.calc_funcs {
                    cor_cols.extend(extract_cor_columns(expr));
                }
            }
            if let Some(end) = &frame.end {
                for expr in &end.calc_funcs {
                    cor_cols.extend(extract_cor_columns(expr));
                }
            }
        }
        cor_cols
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tidb_expression::aggregation::BaseFuncDesc;
    use tidb_parser::types::FieldType;
    use tidb_sessionctx::exprstatic as es;
    use tidb_sessionctx::variable::session::{GlobalVarAccessor, SessionVars};

    struct TestAcc;
    impl GlobalVarAccessor for TestAcc {
        fn get_global_sys_var(&self, _: &str) -> Result<String, Error> {
            Ok(String::new())
        }
        fn set_global_sys_var_only(&mut self, _: &str, _: &str, _: bool) -> Result<(), Error> {
            Ok(())
        }
        fn get_tidb_table_value(&self, _: &str) -> Result<String, Error> {
            Ok(String::new())
        }
        fn set_tidb_table_value(&mut self, _: &str, _: &str, _: &str) -> Result<(), Error> {
            Ok(())
        }
    }

    #[test]
    fn logical_window_extract_correlated_cols_and_frame_clone() {
        let mut vars = SessionVars::new(Box::new(TestAcc));
        let expr_ctx = es::new_expr_context(vec![
            es::with_eval_ctx(es::new_eval_context(vec![])),
            es::with_charset("utf8mb4".to_owned(), "utf8mb4_bin".to_owned()),
            es::with_default_collation_for_utf8mb4("utf8mb4_bin".to_owned()),
        ]);
        let mut ctx = PlanCtx::new(&mut vars, expr_ctx);
        let mk_cor = |uid: i64| {
            Expr::CorrelatedColumn(CorrelatedColumn::new(Column {
                unique_id: uid,
                ..Column::default()
            }))
            .into_ref()
        };
        let desc = WindowFuncDesc {
            base: BaseFuncDesc::from_parts(
                "sum".to_owned(),
                vec![mk_cor(10)],
                Some(Arc::new(FieldType::default())),
            ),
        };
        let frame = WindowFrame {
            tp: ast::FrameType::Ranges,
            start: Some(FrameBound {
                tp: ast::BoundType::Preceding,
                calc_funcs: vec![mk_cor(20)],
                ..FrameBound::default()
            }),
            end: Some(FrameBound {
                tp: ast::BoundType::Following,
                calc_funcs: vec![mk_cor(30)],
                ..FrameBound::default()
            }),
        };
        let cloned = frame.clone_frame();
        assert_eq!(cloned.tp, ast::FrameType::Ranges);
        assert_eq!(cloned.start.as_ref().unwrap().calc_funcs.len(), 1);
        assert_eq!(cloned.end.as_ref().unwrap().calc_funcs.len(), 1);

        let win = LogicalWindow::init(vec![desc], vec![], vec![], Some(frame), &mut ctx, 0);
        let uids: Vec<i64> = win
            .extract_correlated_cols()
            .iter()
            .map(|c| c.column.unique_id)
            .collect();
        assert_eq!(uids, vec![10, 20, 30]);
    }
}
