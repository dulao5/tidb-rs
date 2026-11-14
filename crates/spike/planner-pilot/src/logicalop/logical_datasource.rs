//! go-file: pkg/planner/core/operator/logicalop/logical_datasource.go (spike subset)

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::mysql::{NotNullFlag, PriKeyFlag, TypeLonglong};
use tidb_expression::util::extract_columns_from_expressions;
use tidb_expression::{
    Column, ExprRef, Schema, column2_exprs, gc_column_expr_is_tidb_shard, get_used_list, new_schema,
};
use tidb_model::{
    ColumnInfo, ExtraHandleID, ExtraHandleName, TableInfo, new_extra_handle_col_info,
};
use tidb_parser::model::CIStr;
use tidb_types::field_type::new_field_type;

use crate::arena::PlanId;
use crate::base::LogicalPlan;
use crate::logicalop::{
    BaseLogicalPlan, LogicalProjection, LogicalSchemaProducer, logical_plan_boilerplate,
};
use crate::optimizetrace::{LogicalOptimizeOp, logicaltrace};
use crate::planctx::PlanCtx;
use crate::util::HandleCols;

/// DataSource represents a tableScan without condition push down.
pub struct DataSource {
    /// BaseLogicalPlan.
    pub base: BaseLogicalPlan,
    /// LogicalSchemaProducer.
    pub sp: LogicalSchemaProducer,
    /// DBName.
    pub db_name: CIStr,
    /// TableInfo (Go: `*model.TableInfo`).
    pub table_info: Arc<TableInfo>,
    /// `ds.Table.Type().IsClusterTable()`: `table.Table` is D06.x; a flag here.
    pub is_cluster_table: bool,
    /// Columns.
    pub columns: Vec<ColumnInfo>,
    /// AllConds contains all the filters on this table. For now it's maintained in
    /// predicate push down and used in partition pruning/index merge.
    pub all_conds: Vec<ExprRef>,
    /// ColsRequiringFullLen.
    pub cols_requiring_full_len: Vec<Arc<Column>>,
    /// ContainExprPrefixUk.
    pub contain_expr_prefix_uk: bool,
    /// HandleCols (blocked-by:D09.x, see `util::HandleCols`).
    pub handle_cols: Option<HandleCols>,
    /// UnMutableHandleCols.
    pub un_mutable_handle_cols: Option<HandleCols>,
}

impl DataSource {
    /// Init initializes DataSource (plus `SetSchema`).
    // go: pkg/planner/core/operator/logicalop/logical_datasource.go DataSource.Init L1 [M-PLAN]
    pub fn init(
        db_name: CIStr,
        table_info: Arc<TableInfo>,
        columns: Vec<ColumnInfo>,
        schema: Schema,
        ctx: &PlanCtx<'_>,
        qb_offset: i32,
    ) -> DataSource {
        DataSource {
            base: BaseLogicalPlan::new(ctx, "DataSource", qb_offset),
            sp: LogicalSchemaProducer::new(Some(schema)),
            db_name,
            table_info,
            is_cluster_table: false,
            columns,
            all_conds: Vec::new(),
            cols_requiring_full_len: Vec::new(),
            contain_expr_prefix_uk: false,
            handle_cols: None,
            un_mutable_handle_cols: None,
        }
    }

    /// NewExtraHandleSchemaCol creates a new column for extra handle.
    // go: pkg/planner/core/operator/logicalop/logical_datasource.go (*DataSource).NewExtraHandleSchemaCol L1 [M-CTX]
    pub fn new_extra_handle_schema_col(&self, ctx: &PlanCtx<'_>) -> Column {
        let mut tp = new_field_type(TypeLonglong);
        tp.set_flag(NotNullFlag | PriKeyFlag);
        Column {
            ret_type: Arc::new(tp),
            unique_id: ctx.vars.alloc_plan_column_id(),
            id: ExtraHandleID as i64,
            orig_name: format!(
                "{}.{}.{}",
                self.db_name.o, self.table_info.name.o, ExtraHandleName.o
            ),
            ..Column::default()
        }
    }
}

impl LogicalPlan for DataSource {
    logical_plan_boilerplate!(DataSource, producer);

    /// PruneColumns implements base.LogicalPlan.<2nd> interface.
    // go: pkg/planner/core/operator/logicalop/logical_datasource.go (*DataSource).PruneColumns L1 [M-PLAN]
    fn prune_columns(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        this: PlanId,
        parent_used_cols: Vec<Arc<Column>>,
        opt: &mut LogicalOptimizeOp,
    ) -> Result<PlanId, Error> {
        let ectx = ctx.eval_ctx();
        let used = get_used_list(&*ectx, &parent_used_cols, self.schema(&ctx.arena));

        let expr_cols = extract_columns_from_expressions(Vec::new(), &self.all_conds, None);
        let expr_used = get_used_list(&*ectx, &expr_cols, self.schema(&ctx.arena));
        let mut pruned_columns = Vec::new();

        // Go keeps the slice headers; the in-place `append(s[:i], s[i+1:]...)` below can
        // shift elements of the shared backing array, but `preferKeyColumnFromTable` reads
        // them only when every column was removed, and removing from the back of a slice
        // whose tail is already gone moves nothing. A copy is equivalent.
        let origin_schema_columns = self.schema(&ctx.arena).columns.clone();
        let origin_columns = self.columns.clone();

        self.cols_requiring_full_len = Vec::with_capacity(used.len());
        // N1 (borrow B2): `self.schema()` borrows all of `self`; the loop pushes into another
        // field, so the schema is reached through the embedded producer (a disjoint field).
        for (i, col) in self
            .sp
            .schema(&self.base.children, &ctx.arena)
            .columns
            .iter()
            .enumerate()
        {
            if used[i]
                || (self.contain_expr_prefix_uk
                    && gc_column_expr_is_tidb_shard(col.virtual_expr.as_deref()))
            {
                self.cols_requiring_full_len.push(col.clone());
            }
        }

        for i in (0..used.len()).rev() {
            if !used[i] && !expr_used[i] {
                // If ds has a shard index, and the column is generated column by `tidb_shard()`
                // it can't prune the generated column of shard index
                if self.contain_expr_prefix_uk
                    && gc_column_expr_is_tidb_shard(
                        self.schema(&ctx.arena).columns[i].virtual_expr.as_deref(),
                    )
                {
                    continue;
                }
                pruned_columns.push(self.schema(&ctx.arena).columns[i].clone());
                self.schema_mut(&mut ctx.arena).columns.remove(i);
                self.columns.remove(i);
            }
        }
        logicaltrace::append_column_prune_trace_step(self, &pruned_columns, opt);
        let mut add_one_handle = false;
        // For SQL like `select 1 from t`, tikv's response will be empty if no column is in schema.
        // So we'll force to push one if schema doesn't have any column.
        if self.schema(&ctx.arena).is_empty() {
            let (handle_col, handle_col_info) =
                prefer_key_column_from_table(self, ctx, &origin_schema_columns, &origin_columns);
            self.columns.push(handle_col_info);
            self.schema_mut(&mut ctx.arena).append([handle_col]);
            add_one_handle = true;
        }
        // ref: https://github.com/pingcap/tidb/issues/44579
        // when first entering columnPruner, we kept a column-a in datasource since upper agg function count(a) is used.
        //		then we mark the HandleCols as nil here.
        // when second entering columnPruner, the count(a) is eliminated since it always not null. we should fill another
        // 		extra col, in this way, handle col is useful again, otherwise, _tidb_rowid will be filled.
        if let Some(handle_cols) = &self.handle_cols
            && handle_cols.is_int()
            && self
                .schema(&ctx.arena)
                .column_index(&handle_cols.get_col(0))
                == -1
        {
            self.handle_cols = None;
        }
        // Current DataSource operator contains all the filters on this table, and the columns used by these filters are always included
        // in the output schema. Even if they are not needed by DataSource's parent operator. Thus add a projection here to prune useless columns
        // Limit to MPP tasks, because TiKV can't benefit from this now(projection can't be pushed down to TiKV now).
        // If the parent operator need no columns from the DataSource, we return the smallest column. Don't add the empty proj.
        if !add_one_handle
            && self.schema(&ctx.arena).len() > parent_used_cols.len()
            && !parent_used_cols.is_empty()
            && ctx.vars.is_mpp_enforced()
            && self.table_info.tiflash_replica.is_some()
        {
            let mut proj = LogicalProjection::init(
                column2_exprs(&parent_used_cols),
                new_schema(parent_used_cols),
                ctx,
                self.base.qb_offset,
            );
            // proj.SetStats(ds.StatsInfo()): blocked-by:D10.6 (no stats in the pilot).
            proj.set_children(vec![this]);
            return Ok(ctx.arena.alloc(Box::new(proj)));
        }
        Ok(this)
    }
}

/// preferKeyColumnFromTable. Go's parameter names are swapped relative to the call site
/// (`originColumns []*expression.Column, originSchemaColumns []*model.ColumnInfo` receive
/// `originSchemaColumns, originColumns`); the Rust names follow the types.
// go: pkg/planner/core/operator/logicalop/logical_datasource.go preferKeyColumnFromTable L1 [M-CTX]
fn prefer_key_column_from_table(
    data_source: &DataSource,
    ctx: &PlanCtx<'_>,
    origin_schema_columns: &[Arc<Column>],
    origin_columns: &[ColumnInfo],
) -> (Arc<Column>, ColumnInfo) {
    let result_column_info;
    let result_column;
    if data_source.is_cluster_table && !origin_schema_columns.is_empty() {
        // use the first column.
        result_column_info = origin_columns[0].clone();
        result_column = origin_schema_columns[0].clone();
    } else if let Some(handle_cols) = &data_source.handle_cols {
        result_column = handle_cols.get_col(0);
        result_column_info = result_column.to_info();
    } else if data_source.table_info.pk_is_handle {
        // dataSource.HandleCols = nil doesn't mean datasource doesn't have a intPk handle.
        // since datasource.HandleCols will be cleared in the first columnPruner.
        result_column = data_source
            .un_mutable_handle_cols
            .as_ref()
            .expect("UnMutableHandleCols of a PKIsHandle table")
            .get_col(0);
        result_column_info = result_column.to_info();
    } else {
        result_column = Arc::new(data_source.new_extra_handle_schema_col(ctx));
        result_column_info = new_extra_handle_col_info();
    }
    (result_column, result_column_info)
}
