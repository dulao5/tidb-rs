//! go-file: pkg/planner/core/operator/logicalop/logical_datasource.go
//!
//! `DataSource`, a table scan before access paths are chosen. Fields that only the plan
//! builder's hint and statistics handling fill (`AstIndexHints`, `IndexHints`,
//! `IndexMergeHints`, `StatisticTable`, `TblColHists`, `PreferPartitions`, `SampleInfo`,
//! `IS`) join with the builder (VS1-21) and the statistics (VS1-29): no function here reads
//! them. `Table table.Table` is read only for `Type().IsClusterTable()`, kept as the flag
//! `is_cluster_table` (the plan builder sets it from the table).

use std::collections::HashMap;
use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_base::kv;
use tidb_base::mysql::{NotNullFlag, PriKeyFlag, TypeLonglong, has_pri_key_flag};
use tidb_expr_pushdown::infer_pushdown::push_down_exprs;
use tidb_expression::constant_propagation::propagate_constant;
use tidb_expression::util::{extract_columns_from_expressions, extract_cor_columns};
use tidb_expression::{
    Column, CorrelatedColumn, ExprRef, Schema, column2_exprs, gc_column_expr_is_tidb_shard,
    get_used_list, new_schema,
};
use tidb_model::{ColumnInfo, ExtraHandleID, ExtraHandleName, TableInfo, new_extra_handle_col_info};
use tidb_parser::model::CIStr;
use tidb_types::field_type::new_field_type;

use crate::arena::PlanId;
use crate::base::LogicalPlan;
use crate::core::constraint::delete_true_exprs;
use crate::core::rule_predicate_push_down::add_prefix4_shard_indexes;
use crate::core::rule_predicate_simplification::apply_predicate_simplification;
use crate::logicalop::{
    BaseLogicalPlan, LogicalProjection, LogicalSchemaProducer, new_base_logical_plan,
};
use crate::planctx::PlanCtx;
use crate::util::logicaltrace;
use crate::util::optimizetrace::LogicalOptimizeOp;
use crate::util::{AccessPath, HandleCols};

/// DataSource represents a tableScan without condition push down.
pub struct DataSource {
    /// The embedded `BaseLogicalPlan`.
    pub base: BaseLogicalPlan,
    /// The embedded `LogicalSchemaProducer`.
    pub sp: LogicalSchemaProducer,
    /// `Table.Type().IsClusterTable()` (see the module doc).
    pub is_cluster_table: bool,
    /// TableInfo.
    pub table_info: Arc<TableInfo>,
    /// Columns.
    pub columns: Vec<Arc<ColumnInfo>>,
    /// DBName.
    pub db_name: CIStr,
    /// TableAsName.
    pub table_as_name: Option<CIStr>,
    /// PushedDownConds are the conditions that will be pushed down to coprocessor.
    pub pushed_down_conds: Vec<ExprRef>,
    /// AllConds contains all the filters on this table. For now it's maintained
    /// in predicate push down and used in partition pruning/index merge.
    pub all_conds: Vec<ExprRef>,
    /// PossibleAccessPaths stores all the possible access path for one specific logical
    /// alternative.
    pub possible_access_paths: Vec<AccessPath>,
    /// AllPossibleAccessPaths stores all the possible access paths before pruning.
    pub all_possible_access_paths: Vec<AccessPath>,
    /// The data source may be a partition, rather than a real table.
    pub partition_def_idx: Option<usize>,
    /// PhysicalTableID.
    pub physical_table_id: i64,
    /// PartitionNames.
    pub partition_names: Vec<CIStr>,
    /// handleCol represents the handle column for the datasource, either the
    /// int primary key column or extra handle column.
    pub handle_cols: Option<HandleCols>,
    /// UnMutableHandleCols.
    pub un_mutable_handle_cols: Option<HandleCols>,
    /// TblCols contains the original columns of table before being pruned, and it
    /// is used for estimating table scan cost.
    pub tbl_cols: Vec<Arc<Column>>,
    /// TblColsByID.
    pub tbl_cols_by_id: HashMap<i64, Arc<Column>>,
    /// CommonHandleCols and CommonHandleLens save the info of primary key which is the
    /// clustered index.
    pub common_handle_cols: Vec<Arc<Column>>,
    /// CommonHandleLens.
    pub common_handle_lens: Vec<isize>,
    /// PreferStoreType means the DataSource is enforced to which storage.
    pub prefer_store_type: isize,
    /// IsForUpdateRead should be true in either of the following situations
    /// 1. use `inside insert`, `update`, `delete` or `select for update` statement
    /// 2. isolation level is RC
    pub is_for_update_read: bool,
    /// contain unique index and the first field is tidb_shard(),
    /// such as (tidb_shard(a), a ...), the fields are more than 2
    pub contain_expr_prefix_uk: bool,
    /// ColsRequiringFullLen is the columns that must be fetched with full length.
    pub cols_requiring_full_len: Vec<Arc<Column>>,
    /// AccessPathMinSelectivity is the minimal selectivity among the access paths.
    pub access_path_min_selectivity: f64,
    /// InterestingColumns stores columns from this DataSource that are used in the query.
    pub interesting_columns: Vec<Arc<Column>>,
}

impl DataSource {
    /// Init initializes DataSource. The fields are the struct literal Go's builder writes
    /// before `Init`; the schema is set right after (`SetSchema`).
    // go: DataSource.Init L1 [M-SELF]
    pub fn init(
        ctx: &PlanCtx<'_>,
        table_info: Arc<TableInfo>,
        db_name: CIStr,
        columns: Vec<Arc<ColumnInfo>>,
        schema: Schema,
        offset: isize,
    ) -> DataSource {
        DataSource {
            base: new_base_logical_plan(ctx, "DataSource", offset),
            sp: LogicalSchemaProducer::new(Some(schema)),
            is_cluster_table: false,
            physical_table_id: table_info.id,
            table_info,
            columns,
            db_name,
            table_as_name: None,
            pushed_down_conds: Vec::new(),
            all_conds: Vec::new(),
            possible_access_paths: Vec::new(),
            all_possible_access_paths: Vec::new(),
            partition_def_idx: None,
            partition_names: Vec::new(),
            handle_cols: None,
            un_mutable_handle_cols: None,
            tbl_cols: Vec::new(),
            tbl_cols_by_id: HashMap::new(),
            common_handle_cols: Vec::new(),
            common_handle_lens: Vec::new(),
            prefer_store_type: 0,
            is_for_update_read: false,
            contain_expr_prefix_uk: false,
            cols_requiring_full_len: Vec::new(),
            access_path_min_selectivity: 0.0,
            interesting_columns: Vec::new(),
        }
    }

    /// GetPKIsHandleCol gets the handle column if the PKIsHandle is true, otherwise,
    /// returns the ExtraHandleColumn.
    // go: DataSource.GetPKIsHandleCol L1
    pub fn get_pk_is_handle_col(&self, ctx: &PlanCtx<'_>) -> Option<Arc<Column>> {
        get_pk_is_handle_col_from_schema(
            &self.columns,
            self.schema(&ctx.arena),
            self.table_info.pk_is_handle,
        )
    }

    /// NewExtraHandleSchemaCol creates a new column for extra handle.
    // go: DataSource.NewExtraHandleSchemaCol L1 [M-CTX]
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

    /// AppendTableCol appends a column to the original columns of the table before pruning,
    /// accessed through ds.TblCols and ds.TblColsByID.
    // go: DataSource.AppendTableCol L1
    pub fn append_table_col(&mut self, col: Arc<Column>) {
        self.tbl_cols_by_id.insert(col.id, Arc::clone(&col));
        self.tbl_cols.push(col);
    }
}

// go-exception: EXC.B DataSource.Hash64 (cascades base.HashEquals; D08.1 spec §4 item 2)
// go-exception: EXC.B DataSource.Equals (cascades base.HashEquals; D08.1 spec §4 item 2)
// go-exception: EXC.B3 DataSource.Convert2Gathers (only the old cascades planner calls it, exception B3)
// go-exception: EXC.B3 DataSource.buildTableGather (only the old cascades planner reaches it, exception B3)
// go-exception: EXC.B3 DataSource.buildIndexGather (only the old cascades planner reaches it, exception B3)
impl LogicalPlan for DataSource {
    crate::logical_plan_boilerplate!(DataSource, producer);

    /// ExplainInfo implements Plan interface.
    // go: DataSource.ExplainInfo L1
    fn explain_info(&self, _ctx: &PlanCtx<'_>) -> Result<String, Error> {
        let mut buffer = String::new();
        let mut tbl_name = &self.table_info.name.o;
        if let Some(as_name) = &self.table_as_name
            && !as_name.o.is_empty()
        {
            tbl_name = &as_name.o;
        }
        buffer.push_str(&format!("table:{tbl_name}"));
        if let Some(idx) = self.partition_def_idx
            && let Some(pi) = self.table_info.get_partition_info()
        {
            let defs = pi.definitions.as_deref().unwrap_or_default();
            buffer.push_str(&format!(", partition:{}", defs[idx].name.o));
        }
        Ok(buffer)
    }

    /// PredicatePushDown implements base.LogicalPlan.<1st> interface.
    // go: DataSource.PredicatePushDown L1
    fn predicate_push_down(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        this: PlanId,
        mut predicates: Vec<ExprRef>,
        opt: &LogicalOptimizeOp,
    ) -> Result<(Vec<ExprRef>, PlanId), Error> {
        predicates = propagate_constant(&ctx.expr_ctx, &predicates);
        predicates = delete_true_exprs(ctx, &predicates);
        // Add tidb_shard() prefix to the condtion for shard index in some scenarios
        // TODO: remove it to the place building logical plan
        predicates = add_prefix4_shard_indexes(self, ctx, predicates);
        self.all_conds = predicates.clone();
        let pctx = ctx.push_down_ctx()?;
        (self.pushed_down_conds, predicates) =
            push_down_exprs(&pctx, &predicates, kv::UnSpecified);
        append_data_source_predicate_push_down_trace_step(self, ctx, opt);
        Ok((predicates, this))
    }

    /// PruneColumns implements base.LogicalPlan.<2nd> interface.
    // go: DataSource.PruneColumns L1 [M-PLAN]
    fn prune_columns(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        this: PlanId,
        parent_used_cols: Vec<Arc<Column>>,
        opt: &LogicalOptimizeOp,
    ) -> Result<PlanId, Error> {
        let ectx = ctx.eval_ctx();
        let used = get_used_list(&*ectx, &parent_used_cols, self.schema(&ctx.arena));

        let expr_cols = extract_columns_from_expressions(Vec::new(), &self.all_conds, None);
        let expr_used = get_used_list(&*ectx, &expr_cols, self.schema(&ctx.arena));
        let mut pruned_columns = Vec::new();

        // Go keeps the two slice headers; the in-place `append(s[:i], s[i+1:]...)` below
        // shifts elements of the shared backing arrays, but `preferKeyColumnFromTable` reads
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
                self.cols_requiring_full_len.push(Arc::clone(col));
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
                pruned_columns.push(Arc::clone(&self.schema(&ctx.arena).columns[i]));
                self.schema_mut(&mut ctx.arena).columns.remove(i);
                self.columns.remove(i);
            }
        }
        logicaltrace::append_column_prune_trace_step(self, ctx, &pruned_columns, opt);
        let mut add_one_handle = false;
        // For SQL like `select 1 from t`, tikv's response will be empty if no column is in schema.
        // So we'll force to push one if schema doesn't have any column.
        if self.schema(&ctx.arena).is_empty() {
            let (handle_col, handle_col_info) =
                prefer_key_column_from_table(self, ctx, &origin_schema_columns, &origin_columns);
            self.columns.push(handle_col_info);
            self.schema_mut(&mut ctx.arena).append(vec![handle_col]);
            add_one_handle = true;
        }
        // ref: `44579` (upstream issue)
        // when first entering columnPruner, we kept a column-a in datasource since upper agg function count(a) is used.
        //		then we mark the HandleCols as nil here.
        // when second entering columnPruner, the count(a) is eliminated since it always not null. we should fill another
        // 		extra col, in this way, handle col is useful again, otherwise, _tidb_rowid will be filled.
        if let Some(handle_cols) = &self.handle_cols
            && handle_cols.is_int()
            && let Some(c) = handle_cols.get_col(0)
            && self.schema(&ctx.arena).column_index(c) == -1
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
                ctx,
                self.query_block_offset(),
            );
            proj.base.plan.set_stats(self.base.plan.stats_info().cloned());
            proj.sp.set_schema(Some(new_schema(parent_used_cols)));
            proj.set_children(vec![this]);
            return Ok(ctx.arena.alloc(Box::new(proj)));
        }
        Ok(this)
    }

    /// PredicateSimplification implements the base.LogicalPlan.<7th> interface.
    // go: DataSource.PredicateSimplification L1
    fn predicate_simplification(
        &mut self,
        ctx: &mut PlanCtx<'_>,
        this: PlanId,
        _opt: &LogicalOptimizeOp,
    ) -> Result<PlanId, Error> {
        self.pushed_down_conds = apply_predicate_simplification(ctx, &self.pushed_down_conds)?;
        self.all_conds = apply_predicate_simplification(ctx, &self.all_conds)?;
        Ok(this)
    }

    /// ExtractCorrelatedCols implements base.LogicalPlan.<15th> interface.
    // go: DataSource.ExtractCorrelatedCols L1
    fn extract_correlated_cols(&self) -> Vec<Arc<CorrelatedColumn>> {
        let mut cor_cols = Vec::with_capacity(self.pushed_down_conds.len());
        for expr in &self.pushed_down_conds {
            cor_cols.extend(extract_cor_columns(expr));
        }
        cor_cols
    }
}

// glue: stub of DataSource.BuildKeyInfo, blocked-by:D09.13 (rule/util CheckIndexCanBeKey and domainmisc.GetLatestIndexInfo come with the key-info rule, VS1-24)
// glue: stub of DataSource.ExtractFD, blocked-by:D09.14 (pkg/planner/funcdep is not ported)
// glue: stub of DataSource.IsSingleScan, blocked-by:D10.2 (utilfuncp.IsSingleScan is the access path code)
// glue: stub of DataSource.CheckPartialIndexes, blocked-by:D10.2 (partial indexes: partidx and the access paths)
// glue: stub of DataSource.CheckPartialIndexByFilters, blocked-by:D10.2 (partial indexes: partidx and ParseSimpleExpr)
/// The boundary of the DataSource functions stubbed above (key info, FDs, partial indexes,
/// single scan): the error they return.
pub fn data_source_boundary(what: &str, block: &str) -> Error {
    blocked(block, what)
}

// go: getPKIsHandleColFromSchema L1
fn get_pk_is_handle_col_from_schema(
    cols: &[Arc<ColumnInfo>],
    schema: &Schema,
    pk_is_handle: bool,
) -> Option<Arc<Column>> {
    if !pk_is_handle {
        // If the PKIsHandle is false, return the ExtraHandleColumn.
        for (i, col) in cols.iter().enumerate() {
            if col.id == ExtraHandleID as i64 {
                return Some(Arc::clone(&schema.columns[i]));
            }
        }
        return None;
    }
    for (i, col) in cols.iter().enumerate() {
        if has_pri_key_flag(col.get_flag()) {
            return Some(Arc::clone(&schema.columns[i]));
        }
    }
    None
}

// go: appendDataSourcePredicatePushDownTraceStep L1 [M-CTX]
fn append_data_source_predicate_push_down_trace_step(
    ds: &DataSource,
    ctx: &PlanCtx<'_>,
    opt: &LogicalOptimizeOp,
) {
    if ds.pushed_down_conds.is_empty() {
        return;
    }
    let reason = String::new;
    let action = || {
        let ectx = ctx.eval_ctx();
        let mut buffer = String::from("The conditions[");
        for (i, cond) in ds.pushed_down_conds.iter().enumerate() {
            if i > 0 {
                buffer.push(',');
            }
            buffer.push_str(&String::from_utf8_lossy(&cond.string_with_ctx(
                &*ectx,
                tidb_base::errors::RedactLogDisable,
            )));
        }
        buffer.push_str(&format!("] are pushed down across {}_{}", ds.tp(), ds.id()));
        buffer
    };
    opt.append_step_to_current(ds.id() as isize, ds.tp(), reason, action);
}

/// preferKeyColumnFromTable. Go's parameter names are swapped relative to the call site
/// (`originColumns []*expression.Column, originSchemaColumns []*model.ColumnInfo` receive
/// `originSchemaColumns, originColumns`); the Rust names follow the types.
// go: preferKeyColumnFromTable L1 [M-CTX]
fn prefer_key_column_from_table(
    data_source: &DataSource,
    ctx: &PlanCtx<'_>,
    origin_schema_columns: &[Arc<Column>],
    origin_columns: &[Arc<ColumnInfo>],
) -> (Arc<Column>, Arc<ColumnInfo>) {
    let result_column_info;
    let result_column;
    if data_source.is_cluster_table && !origin_schema_columns.is_empty() {
        // use the first column.
        result_column_info = Arc::clone(&origin_columns[0]);
        result_column = Arc::clone(&origin_schema_columns[0]);
    } else if let Some(handle_cols) = &data_source.handle_cols {
        result_column = Arc::clone(handle_cols.get_col(0).expect("handle column"));
        result_column_info = Arc::new(result_column.to_info());
    } else if data_source.table_info.pk_is_handle {
        // dataSource.HandleCols = nil doesn't mean datasource doesn't have a intPk handle.
        // since datasource.HandleCols will be cleared in the first columnPruner.
        result_column = Arc::clone(
            data_source
                .un_mutable_handle_cols
                .as_ref()
                .and_then(|h| h.get_col(0))
                .expect("UnMutableHandleCols of a PKIsHandle table"),
        );
        result_column_info = Arc::new(result_column.to_info());
    } else {
        result_column = Arc::new(data_source.new_extra_handle_schema_col(ctx));
        result_column_info = Arc::new(new_extra_handle_col_info());
    }
    (result_column, result_column_info)
}
