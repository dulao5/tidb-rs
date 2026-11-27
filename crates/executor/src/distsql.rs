//! go-file: pkg/executor/distsql.go
//!
//! DistSQL reader helpers (`closeAll`, `getPhysicalPlanIDs`, block D12.1 Part 2A) and
//! `IndexReaderExecutor` (block D12.1 Part 2B: `rebuildIndexRanges`,
//! `newIndexReaderExecutorContext`, `IndexReaderExecutor.{Table,setDummy,Close,Next,Open,
//! buildKVRangesForIndexReader,buildKVReq,open}`, `buildKeyRanges`, `needMergeSort`).
//! `IndexLookUpExecutor` (Part 2C) will be added here.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_chunk::chunk::Chunk;
use tidb_expression::context::BuildContext;
use tidb_expression::util::extract_cor_columns;
use tidb_expression::{Column, Expr, ExprRef};
use tidb_infoschema::context::MetaOnlyInfoSchema;
use tidb_infoschema::table::{PhysicalTable, TableRef};
use tidb_kv::key::KeyRange;
use tidb_kv::kv::Request;
use tidb_memory::tracker::{Tracker, new_tracker};
use tidb_model::{ColumnInfo, IndexInfo};
use tidb_parser::types::FieldType;
use tidb_planner::core::find_best_task::group_ranges_by_cols;
use tidb_planner::core::physical_plans::PhysicalIndexScan;
use tidb_planner::ranger::detacher::detach_simple_cond_and_build_range_for_index;
use tidb_planner::util::byitem::ByItems;
use tidb_sessionctx::distsql_ctx::DistSQLContext;
use tidb_sessionctx::exprctx::ExprContextRef;
use tidb_sessionctx::planctx::BuildPBContext;
use tidb_sessionctx::ranger::Range;
use tidb_sessionctx::ranger::context::RangerContext;
use tidb_store::distsql::{
    RequestBuilder, SelectResult, common_handle_ranges_to_kv_ranges,
    index_ranges_to_kv_ranges_with_interrupt_signal, new_sorted_select_results,
};

use crate::builder::new_closest_read_adjuster;
use crate::exec::ExecCtx;
use crate::glue::{Plan, SessionContext};
use crate::internal::builder::builder_utils::construct_list_based_dist_exec;
use crate::internal::exec::executor::{BaseExecutorV2, Executor};
use crate::internal::exec::indexusage::IndexUsageReporter;
use crate::table_reader::{SelectResultHook, kv_ctx_from_dctx};

/// Go's `Closeable` interface (`distsql.go:134`).
pub trait Closeable {
    // glue: Go interface method Closeable.Close
    fn close(&mut self) -> Result<(), Error>;
}

impl Closeable for Box<dyn SelectResult> {
    // glue: Closeable for Box<dyn SelectResult>
    fn close(&mut self) -> Result<(), Error> {
        SelectResult::close(self.as_mut())
    }
}

/// closeAll closes all objects even if an object returns an error.
/// If multiple objects return an error, the first error will be returned.
// go: closeAll L1
pub fn close_all(objs: &mut [Option<&mut dyn Closeable>]) -> Result<(), Error> {
    let mut err: Option<Error> = None;
    for obj in objs {
        if let Some(obj) = obj.as_deref_mut()
            && let Err(err1) = obj.close()
            && err.is_none()
        {
            err = Some(err1);
        }
    }
    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

// glue: helper to detect deferred constants before SubstituteCorCol2Constant (re-homed to D10.1)
fn has_deferred_constant(expr: &ExprRef) -> bool {
    match expr.as_ref() {
        Expr::Constant(c) => c.deferred_expr.is_some(),
        Expr::ScalarFunction(sf) => sf.get_args().iter().any(has_deferred_constant),
        _ => false,
    }
}

/// rebuildIndexRanges will be called if there's correlated column in access conditions.
/// We will rebuild the range by substituting correlated column with the constant.
// go: rebuildIndexRanges L2 (expression.SubstituteCorCol2Constant stops at D10.1, matching PhysicalTableScan.ResolveCorrelatedColumns)
pub fn rebuild_index_ranges(
    _ectx: &dyn BuildContext,
    rctx: &RangerContext,
    is: &PhysicalIndexScan,
    idx_cols: &[Arc<Column>],
    col_lens: &[isize],
) -> Result<Vec<Range>, Error> {
    let mut access = Vec::with_capacity(is.access_condition.len());
    for cond in &is.access_condition {
        if !extract_cor_columns(cond).is_empty() || has_deferred_constant(cond) {
            return Err(blocked(
                "D10.1",
                "expression.SubstituteCorCol2Constant in rebuildIndexRanges",
            ));
        }
        access.push(Arc::clone(cond));
    }
    // All of access conditions must be used to build ranges, so we don't limit range memory usage.
    let (ranges, _, remained_conds) =
        detach_simple_cond_and_build_range_for_index(rctx, &access, idx_cols, col_lens, 0)?;
    // The planner only admits an AccessCondition into the correlated-access path when the
    // detacher can fully encode it as ranges.
    debug_assert!(
        remained_conds.is_empty(),
        "rebuildIndexRanges: detacher returned residuals on correlated-access path"
    );
    Ok(ranges)
}

/// indexReaderExecutorContext is the execution context for [`IndexReaderExecutor`].
#[derive(Clone)]
pub struct IndexReaderExecutorContext {
    pub rctx: Arc<RangerContext>,
    pub dctx: Arc<DistSQLContext>,
    pub ectx: ExprContextRef,
    pub info_schema: Option<Arc<dyn MetaOnlyInfoSchema>>,
    pub build_pb_ctx: Arc<BuildPBContext>,
    pub stmt_mem_tracker: Option<Arc<Tracker>>,
}

/// newIndexReaderExecutorContext builds the frozen execution context for [`IndexReaderExecutor`].
// go: newIndexReaderExecutorContext L2 (N2 freezes session contexts; get_build_pb_ctx returns Result)
pub fn new_index_reader_executor_context(
    sctx: &dyn SessionContext,
) -> Result<IndexReaderExecutorContext, Error> {
    let build_pb_ctx = sctx.get_build_pb_ctx()?;
    let dctx = sctx.get_dist_sql_ctx();
    let rctx = sctx.get_ranger_ctx();
    let ectx = sctx.get_expr_ctx();
    let stmt_mem_tracker = sctx
        .get_session_vars()
        .stmt_ctx()
        .mem_tracker
        .as_ref()
        .map(Arc::clone);
    let info_schema = sctx
        .get_info_schema()
        .map(|is| is as Arc<dyn MetaOnlyInfoSchema>);
    Ok(IndexReaderExecutorContext {
        rctx,
        dctx,
        ectx,
        info_schema,
        build_pb_ctx,
        stmt_mem_tracker,
    })
}

/// IndexReaderExecutor sends DAG request and reads index data from kv layer.
pub struct IndexReaderExecutor {
    pub ctx: IndexReaderExecutorContext,
    pub base: BaseExecutorV2,
    pub index_usage_reporter: Option<IndexUsageReporter>,

    /// For a partitioned table, the IndexReaderExecutor works on a partition, so
    /// the table is a `table::PhysicalTable`.
    pub table: Option<TableRef>,
    pub index: Arc<IndexInfo>,
    pub physical_table_id: i64,
    pub ranges: Vec<Range>,
    /// grouped_ranges is from `AccessPath.grouped_ranges`. When set, we access each group
    /// respectively and use a merge sort to combine them.
    pub grouped_ranges: Vec<Vec<Range>>,
    pub partitions: Vec<Arc<dyn PhysicalTable>>,
    pub part_range_map: HashMap<i64, Vec<Range>>,

    /// kv_ranges are only used for union scan.
    pub kv_ranges: Vec<KeyRange>,
    pub dag_pb: tipb::DagRequest,
    pub start_ts: u64,
    pub txn_scope: String,
    pub read_replica_scope: String,
    pub is_staleness: bool,
    pub net_data_size: f64,
    /// result returns one or more partial results from TiKV regions.
    pub result: Option<Box<dyn SelectResult>>,
    /// columns are only required by union scan.
    pub columns: Vec<Arc<ColumnInfo>>,
    /// output_columns are only required by union scan.
    pub output_columns: Vec<Arc<Column>>,
    /// partition_id_map is only required by union scan with global index.
    pub partition_id_map: HashSet<i64>,

    pub paging: bool,

    pub keep_order: bool,
    pub desc: bool,
    /// by_items only for partition table / grouped ranges with orderBy + pushedLimit.
    pub by_items: Vec<ByItems>,

    pub cor_col_in_filter: bool,
    pub cor_col_in_access: bool,
    pub idx_cols: Vec<Arc<Column>>,
    pub col_lens: Vec<isize>,
    pub plans: Vec<Plan>,

    pub mem_tracker: Option<Arc<Tracker>>,

    pub select_result_hook: SelectResultHook,

    /// If dummy flag is set, this is not a real IndexReader, it just provides KV ranges for
    /// UnionScan. Used by temporary tables and cached tables.
    pub dummy: bool,
}

impl Drop for IndexReaderExecutor {
    /// Safety net when `IndexReaderExecutor` is dropped without `close()`: closes the
    /// underlying `SelectResult` so no coprocessor worker or mock resource leaks (spec §10.3).
    // glue: Drop safety net (N3 §3.5, spec §10.3)
    fn drop(&mut self) {
        if let Some(mut res) = self.result.take() {
            let _ = res.close();
        }
    }
}

impl IndexReaderExecutor {
    /// Table implements the dataSourceExecutor interface.
    // go: IndexReaderExecutor.Table L1
    pub fn table(&self) -> Option<&TableRef> {
        self.table.as_ref()
    }

    // go: IndexReaderExecutor.setDummy L1
    pub fn set_dummy(&mut self) {
        self.dummy = true;
    }

    /// Close clears all resources held by current object.
    // go: IndexReaderExecutor.Close L1
    pub async fn close_impl(&mut self) -> Result<(), Error> {
        if let Some(reporter) = &self.index_usage_reporter
            && let Some(tbl) = &self.table
            && let Some(first_plan) = self.plans.first()
        {
            let phys_id = tbl.as_physical_table().map(|p| p.get_physical_id());
            reporter.report_cop_index_usage_for_table(
                tbl.meta(),
                phys_id,
                self.index.id,
                first_plan.id() as isize,
            );
        }

        let mut err: Result<(), Error> = Ok(());
        if let Some(mut res) = self.result.take() {
            err = res.close();
        }
        self.kv_ranges.clear();
        if self.dummy {
            return Ok(());
        }
        err
    }

    /// Next implements the Executor Next interface.
    // go: IndexReaderExecutor.Next L1 [M-GOCTX]
    pub async fn next_impl(&mut self, _ctx: &ExecCtx, req: &mut Chunk) -> Result<(), Error> {
        if self.dummy {
            req.reset();
            return Ok(());
        }

        let kv_ctx = kv_ctx_from_dctx(&self.ctx.dctx);
        self.result
            .as_mut()
            .expect("IndexReaderExecutor.Next called before Open")
            .next(&kv_ctx, req)
            .await
    }

    /// Open implements the Executor Open interface.
    // go: IndexReaderExecutor.Open L1 [M-GOCTX]
    pub async fn open_impl(&mut self, _ctx: &ExecCtx) -> Result<(), Error> {
        if self.cor_col_in_access {
            let is = self.plans[0]
                .as_any()
                .downcast_ref::<PhysicalIndexScan>()
                .expect("IndexReaderExecutor.plans[0] must be PhysicalIndexScan");
            self.ranges = rebuild_index_ranges(
                &*self.ctx.ectx,
                &self.ctx.rctx,
                is,
                &self.idx_cols,
                &self.col_lens,
            )?;
            // Rebuild groupedRanges if it was originally set.
            if !is.group_by_col_idxs.is_empty() {
                self.grouped_ranges = group_ranges_by_cols(&self.ranges, &is.group_by_col_idxs)?;
            }
        }

        // partRangeMap comes from the index join code path, while groupedRanges will not be set in that case.
        // They are two different sources of ranges, and should not appear together.
        debug_assert!(
            !(!self.part_range_map.is_empty() && !self.grouped_ranges.is_empty()),
            "partRangeMap and groupedRanges should not appear together"
        );

        // Build kvRanges considering both partitions and groupedRanges.
        let kv_ranges = self.build_kv_ranges_for_index_reader()?;
        let kv_ctx = kv_ctx_from_dctx(&self.ctx.dctx);
        self.open_with_kv_ranges(&kv_ctx, kv_ranges).await
    }

    /// buildKVRangesForIndexReader builds kvRanges for IndexReaderExecutor considering both
    /// partitions and groupedRanges.
    // go: IndexReaderExecutor.buildKVRangesForIndexReader L1
    pub(crate) fn build_kv_ranges_for_index_reader(&self) -> Result<Vec<KeyRange>, Error> {
        let mut table_ids = Vec::with_capacity(self.partitions.len().max(1));
        for p in &self.partitions {
            table_ids.push(p.get_physical_id());
        }
        if self.partitions.is_empty() {
            table_ids.push(self.physical_table_id);
        }

        let single_group;
        let grouped_ranges: &[Vec<Range>] = if self.grouped_ranges.is_empty() {
            single_group = [self.ranges.clone()];
            &single_group
        } else {
            &self.grouped_ranges
        };

        let mut results = Vec::with_capacity(grouped_ranges.len());
        for ranges in grouped_ranges {
            let kv_ranges = build_key_ranges(
                &self.ctx.dctx,
                ranges,
                &self.part_range_map,
                &table_ids,
                self.index.id,
                None,
            )?;
            for kv_range in kv_ranges {
                results.extend(kv_range);
            }
        }
        Ok(results)
    }

    // go: IndexReaderExecutor.buildKVReq L1
    pub(crate) fn build_kv_req(&self, r: Vec<KeyRange>) -> Result<Request, Error> {
        let mut builder = RequestBuilder::default();
        builder
            .set_key_ranges(r)
            .set_dag_request(self.dag_pb.clone())
            .set_start_ts(self.start_ts)
            .set_desc(self.desc)
            .set_keep_order(self.keep_order)
            .set_txn_scope(self.txn_scope.clone())
            .set_read_replica_scope(self.read_replica_scope.clone())
            .set_is_staleness(self.is_staleness)
            .set_from_session_vars(&self.ctx.dctx);
        if let Some(is) = &self.ctx.info_schema {
            builder.set_from_info_schema(Arc::clone(is));
        }
        builder.set_mem_tracker(self.mem_tracker.as_ref().map(Arc::clone));
        let adjuster =
            new_closest_read_adjuster(&self.ctx.dctx, &builder.request, self.net_data_size);
        builder
            .set_closest_replica_read_adjuster(adjuster)
            .set_conn_id_and_conn_alias(
                self.ctx.dctx.connection_id,
                self.ctx.dctx.session_alias.clone(),
            );
        builder.build()
    }

    /// open sends the DAG request(s) for precomputed `kv_ranges` (also called directly by IndexJoin).
    // go: IndexReaderExecutor.open L2 (closes previously opened SelectResults on mid-loop error, spec §10.3)
    pub async fn open_with_kv_ranges(
        &mut self,
        ctx: &tidb_kv::Context,
        mut kv_ranges: Vec<KeyRange>,
    ) -> Result<(), Error> {
        if self.cor_col_in_filter {
            let execs = construct_list_based_dist_exec(&self.ctx.build_pb_ctx, &self.plans)?;
            self.dag_pb.set_executors(execs.into());
        }

        if self.base.stats.runtime_stats().is_some() {
            self.dag_pb.set_collect_execution_summaries(true);
        }

        // Treat temporary table as dummy table, avoid sending distsql request to TiKV.
        // In a test case IndexReaderExecutor is mocked and e.table is nil.
        // Avoid sending distsql request to TiKV.
        if self.dummy {
            self.kv_ranges = kv_ranges;
            return Ok(());
        }

        if let Some(t) = &self.mem_tracker {
            t.reset()?;
        } else {
            self.mem_tracker = Some(new_tracker(self.base.meta.id(), -1));
        }
        if let (Some(t), Some(stmt_t)) = (&self.mem_tracker, &self.ctx.stmt_mem_tracker) {
            t.attach_to(stmt_t)?;
        }

        kv_ranges.sort_by(|i, j| i.start_key.0.cmp(&j.start_key.0));
        self.kv_ranges = kv_ranges;

        let ret_types: Vec<Arc<FieldType>> = self.base.meta.ret_field_types().to_vec();
        let cop_plan_ids = get_physical_plan_ids(&self.plans);
        let root_plan_id = self.base.meta.id();

        if !need_merge_sort(&self.by_items, self.kv_ranges.len()) {
            let kv_req = self.build_kv_req(self.kv_ranges.clone())?;
            let result = self
                .select_result_hook
                .select_result(
                    ctx,
                    Arc::clone(&self.ctx.dctx),
                    &kv_req,
                    ret_types,
                    cop_plan_ids,
                    root_plan_id,
                )
                .await?;
            self.result = Some(result);
        } else {
            // Use sortedSelectResults for merge sort.
            let mut kv_reqs = Vec::with_capacity(self.kv_ranges.len());
            for kv_range in &self.kv_ranges {
                let kv_req = self.build_kv_req(vec![kv_range.clone()])?;
                kv_reqs.push(kv_req);
            }
            let mut results: Vec<Box<dyn SelectResult>> = Vec::with_capacity(kv_reqs.len());
            for kv_req in &kv_reqs {
                match self
                    .select_result_hook
                    .select_result(
                        ctx,
                        Arc::clone(&self.ctx.dctx),
                        kv_req,
                        ret_types.clone(),
                        cop_plan_ids.clone(),
                        root_plan_id,
                    )
                    .await
                {
                    Ok(result) => results.push(result),
                    Err(err) => {
                        for sr in &mut results {
                            let _ = sr.close();
                        }
                        return Err(err);
                    }
                }
            }
            let schema = self.base.meta.schema();
            let sorted = new_sorted_select_results(
                &*self.ctx.ectx.get_eval_ctx(),
                results,
                Some(&schema),
                self.by_items.clone(),
                self.mem_tracker.as_ref().map(Arc::clone),
            )?;
            self.result = Some(sorted);
        }
        Ok(())
    }
}

#[async_trait]
impl Executor for IndexReaderExecutor {
    // glue: M-SELF
    fn base(&self) -> &BaseExecutorV2 {
        &self.base
    }
    // glue: M-SELF
    fn base_mut(&mut self) -> &mut BaseExecutorV2 {
        &mut self.base
    }
    // glue: Go interface method Executor.Open
    async fn open(&mut self, ctx: &ExecCtx) -> Result<(), Error> {
        self.open_impl(ctx).await
    }
    // glue: Go interface method Executor.Next
    async fn next(&mut self, ctx: &ExecCtx, req: &mut Chunk) -> Result<(), Error> {
        self.next_impl(ctx, req).await
    }
    // glue: Go interface method Executor.Close
    async fn close(&mut self) -> Result<(), Error> {
        self.close_impl().await
    }
    // glue: Go interface method Executor.Detach
    fn detach(&self) -> Option<Box<dyn Executor>> {
        crate::detach::index_reader_executor_detach(self)
    }
}

/// buildKeyRanges encodes logical ranges into per-physical-table KV key ranges for either
/// a common handle (`index_id == -1`) or a secondary index (`index_id >= 0`).
// go: buildKeyRanges L1
pub fn build_key_ranges(
    dctx: &DistSQLContext,
    ranges: &[Range],
    range_override_for_partition_id: &HashMap<i64, Vec<Range>>,
    physical_ids: &[i64],
    index_id: i64,
    mem_tracker: Option<&Arc<Tracker>>,
) -> Result<Vec<Vec<KeyRange>>, Error> {
    let mut results = Vec::with_capacity(physical_ids.len());
    for &physical_id in physical_ids {
        let cur_ranges: &[Range] = range_override_for_partition_id
            .get(&physical_id)
            .map_or(ranges, Vec::as_slice);
        if index_id == -1 {
            let r_ranges =
                common_handle_ranges_to_kv_ranges(Some(dctx), &[physical_id], cur_ranges)?;
            results.push(r_ranges.first_partition_range().to_vec());
        } else {
            let single_ranges = index_ranges_to_kv_ranges_with_interrupt_signal(
                Some(dctx),
                physical_id,
                index_id,
                cur_ranges,
                mem_tracker,
                None,
            )?;
            results.push(single_ranges.first_partition_range().to_vec());
        }
    }
    Ok(results)
}

/// getPhysicalPlanIDs returns the plan IDs of all pushed-down physical plans.
// go: getPhysicalPlanIDs L1
pub fn get_physical_plan_ids(plans: &[Plan]) -> Vec<isize> {
    let mut plan_ids = Vec::with_capacity(plans.len());
    for p in plans {
        plan_ids.push(p.id() as isize);
    }
    plan_ids
}

/// needMergeSort reports whether an index reader needs a merge sort across multiple KV ranges.
// go: needMergeSort L1
pub fn need_merge_sort(by_items: &[ByItems], kv_ranges_count: usize) -> bool {
    !by_items.is_empty() && kv_ranges_count > 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use tidb_base::mysql::{NotNullFlag, PriKeyFlag, TypeLonglong};
    use tidb_expression::CorrelatedColumn;
    use tidb_expression::constant::Constant;
    use tidb_expression::evaluator::new_evaluator_suite;
    use tidb_expression::expression::EvalState;
    use tidb_expression::scalar_function::ast_func as ast;
    use tidb_expression::scalar_function::new_function;
    use tidb_expression::schema::new_schema;
    use tidb_memory::sqlkiller::QueryInterrupted;
    use tidb_model::{
        DBInfo, IndexColumn, PartitionInfo, StatePublic, TableCacheStatusEnable, TableInfo,
    };
    use tidb_parser::model::CIStr;
    use tidb_parser::types::UnspecifiedLength;
    use tidb_planner::core::physical_plans::{PhysicalIndexReader, PhysicalLimit};
    use tidb_planner::property::StatsInfo;
    use tidb_planner::{PhysicalPlan, PhysicalPlanRef};
    use tidb_store::distsql::SelectResultIter;
    use tidb_types::datum::new_int_datum;
    use tidb_types::field_type::new_field_type;

    use crate::adapter::TelemetryInfo;
    use crate::adapter::tests::MockSession;
    use crate::builder::new_executor_builder;
    use crate::internal::exec::executor::{self as exec, new_base_executor, new_base_executor_v2};
    use crate::projection::{ProjectionExec, new_projection_executor_context};
    use crate::select::{LimitExec, SelectionExec, new_selection_executor_context};
    use crate::sortexec::sort::new_sort_exec;
    use crate::sortexec::topn::new_top_n_exec;

    struct TrackedMockSelectResult {
        rows: Vec<Vec<i64>>,
        pos: usize,
        closed: Arc<AtomicBool>,
        next_err: Option<String>,
        close_err: Option<String>,
    }

    #[async_trait]
    impl SelectResult for TrackedMockSelectResult {
        async fn next_raw(&mut self, _ctx: &tidb_kv::Context) -> Result<Option<Vec<u8>>, Error> {
            Ok(None)
        }

        async fn next(&mut self, _ctx: &tidb_kv::Context, chk: &mut Chunk) -> Result<(), Error> {
            chk.reset();
            if let Some(err) = self.next_err.take() {
                return Err(Error::plain(err));
            }
            if self.pos >= self.rows.len() {
                return Ok(());
            }
            let take = chk.required_rows().min(self.rows.len() - self.pos);
            for i in 0..take {
                let row = &self.rows[self.pos + i];
                for (c, &v) in row.iter().enumerate() {
                    chk.append_int64(c, v);
                }
            }
            self.pos += take;
            Ok(())
        }

        fn into_iter(
            &mut self,
            _intermediate_field_types: Vec<Vec<Arc<FieldType>>>,
        ) -> Result<Box<dyn SelectResultIter>, Error> {
            Err(Error::plain("not implemented"))
        }

        fn close(&mut self) -> Result<(), Error> {
            self.closed.store(true, Ordering::SeqCst);
            if let Some(err) = &self.close_err {
                return Err(Error::plain(err.clone()));
            }
            Ok(())
        }
    }

    async fn make_info_schema(tbl: Arc<TableInfo>) -> Arc<tidb_infoschema::InfoSchemaV2> {
        let mut db = DBInfo {
            id: 1,
            name: CIStr::new("test"),
            state: StatePublic,
            ..Default::default()
        };
        db.deprecated.tables = Some(vec![tbl]);
        let mut b = tidb_infoschema::new_builder(None, None, tidb_infoschema::new_data());
        b.init_with_db_infos(vec![db], vec![], vec![], 1)
            .await
            .unwrap();
        Arc::new(b.build(1).await)
    }

    fn int_ft() -> FieldType {
        new_field_type(TypeLonglong)
    }

    fn make_index_table_and_plan(
        ranges: Vec<Range>,
        grouped_ranges: Vec<Vec<Range>>,
        by_items: Vec<ByItems>,
        output_col_indices: &[isize],
    ) -> (Arc<TableInfo>, PhysicalIndexReader) {
        let mut pk_ft = int_ft();
        pk_ft.add_flag(PriKeyFlag | NotNullFlag);
        let c_id = Arc::new(ColumnInfo {
            id: 1,
            name: CIStr::new("id"),
            offset: 0,
            field_type: pk_ft.clone(),
            state: StatePublic,
            ..Default::default()
        });
        let c_k = Arc::new(ColumnInfo {
            id: 2,
            name: CIStr::new("k"),
            offset: 1,
            field_type: int_ft(),
            state: StatePublic,
            ..Default::default()
        });
        let idx = Arc::new(IndexInfo {
            id: 1,
            name: CIStr::new("idx_k"),
            table: CIStr::new("t"),
            columns: Some(vec![IndexColumn {
                name: CIStr::new("k"),
                offset: 1,
                length: UnspecifiedLength,
                ..Default::default()
            }]),
            state: StatePublic,
            ..Default::default()
        });
        let tbl = Arc::new(TableInfo {
            id: 42,
            name: CIStr::new("t"),
            columns: Some(vec![Arc::clone(&c_id), Arc::clone(&c_k)]),
            indices: Some(vec![Arc::clone(&idx)]),
            pk_is_handle: true,
            state: StatePublic,
            ..Default::default()
        });

        let col_k = Arc::new(Column {
            id: 2,
            unique_id: 2,
            index: 0,
            ret_type: Arc::new(int_ft()),
            ..Default::default()
        });
        let col_id = Arc::new(Column {
            id: 1,
            unique_id: 1,
            index: 1,
            ret_type: Arc::new(pk_ft),
            ..Default::default()
        });
        let scan_schema = new_schema(vec![Arc::clone(&col_k), Arc::clone(&col_id)]);
        let stats = Arc::new(StatsInfo {
            row_count: 100.0,
            ..Default::default()
        });
        let mut scan = PhysicalIndexScan {
            table: Arc::clone(&tbl),
            index: Arc::clone(&idx),
            columns: vec![c_k, c_id],
            ranges,
            grouped_ranges,
            by_items,
            keep_order: true,
            data_source_schema: Some(scan_schema.clone()),
            ..Default::default()
        };
        scan.set_id(10);
        scan.set_schema(scan_schema.clone());
        scan.set_stats(Some(Arc::clone(&stats)));
        let scan_ref: PhysicalPlanRef = Arc::new(scan);

        let output_columns: Vec<Arc<Column>> = output_col_indices
            .iter()
            .map(|&idx_pos| {
                let base_col = if idx_pos == 0 { &col_k } else { &col_id };
                Arc::new(Column {
                    index: idx_pos,
                    ..(**base_col).clone()
                })
            })
            .collect();
        let reader_schema = new_schema(
            output_columns
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    Arc::new(Column {
                        index: i as isize,
                        ..(**c).clone()
                    })
                })
                .collect(),
        );
        let mut reader = PhysicalIndexReader {
            index_plan: Some(Arc::clone(&scan_ref)),
            index_plans: vec![scan_ref],
            output_columns,
            ..Default::default()
        };
        reader.set_id(11);
        reader.sp.set_schema(Some(reader_schema));
        reader.set_stats(Some(stats));
        (tbl, reader)
    }

    fn int_range(lo: i64, hi: i64) -> Range {
        Range {
            low_val: vec![new_int_datum(lo)],
            high_val: vec![new_int_datum(hi)],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn drop_without_close_closes_select_result() {
        let (tbl, plan) = make_index_table_and_plan(vec![int_range(1, 10)], vec![], vec![], &[0]);
        let is = make_info_schema(tbl).await;
        let mut s = MockSession::new(100);
        let mut b = new_executor_builder(&s, Some(is), Arc::new(TelemetryInfo::default()));
        let mut ir = b.build_index_reader(&mut s, &plan).await.expect("built");

        let closed = Arc::new(AtomicBool::new(false));
        let closed_cb = Arc::clone(&closed);
        ir.select_result_hook = SelectResultHook {
            select_result_func: Some(Arc::new(move |_, _, _, _, _| {
                let c = Arc::clone(&closed_cb);
                Box::pin(async move {
                    Ok(Box::new(TrackedMockSelectResult {
                        rows: vec![vec![1], vec![2]],
                        pos: 0,
                        closed: c,
                        next_err: None,
                        close_err: None,
                    }) as Box<dyn SelectResult>)
                })
            })),
        };

        let ctx = ExecCtx::new(s.vars.stmt_ctx_arc());
        ir.open(&ctx).await.unwrap();
        assert!(!closed.load(Ordering::SeqCst));
        drop(ir);
        assert!(
            closed.load(Ordering::SeqCst),
            "Drop safety net must close underlying SelectResult"
        );
    }

    #[tokio::test]
    async fn mid_open_kth_failure_closes_prior_select_results() {
        let col0 = Arc::new(Column {
            id: 2,
            unique_id: 2,
            index: 0,
            ret_type: Arc::new(int_ft()),
            ..Default::default()
        });
        let by_items = vec![ByItems {
            expr: Expr::Column((*col0).clone()).into_ref(),
            desc: false,
        }];
        let (tbl, plan) = make_index_table_and_plan(
            vec![],
            vec![
                vec![int_range(1, 5)],
                vec![int_range(10, 15)],
                vec![int_range(20, 25)],
            ],
            by_items,
            &[0],
        );
        let is = make_info_schema(tbl).await;
        let mut s = MockSession::new(100);
        let mut b = new_executor_builder(&s, Some(is), Arc::new(TelemetryInfo::default()));
        let mut ir = b.build_index_reader(&mut s, &plan).await.expect("built");

        let closed0 = Arc::new(AtomicBool::new(false));
        let closed1 = Arc::new(AtomicBool::new(false));
        let call_num = Arc::new(AtomicUsize::new(0));
        let c0 = Arc::clone(&closed0);
        let c1 = Arc::clone(&closed1);
        let cn = Arc::clone(&call_num);
        ir.select_result_hook = SelectResultHook {
            select_result_func: Some(Arc::new(move |_, _, _, _, _| {
                let idx = cn.fetch_add(1, Ordering::SeqCst);
                let c = if idx == 0 {
                    Arc::clone(&c0)
                } else {
                    Arc::clone(&c1)
                };
                Box::pin(async move {
                    if idx == 2 {
                        return Err(Error::plain("3rd select result boom"));
                    }
                    Ok(Box::new(TrackedMockSelectResult {
                        rows: vec![vec![idx as i64]],
                        pos: 0,
                        closed: c,
                        next_err: None,
                        close_err: None,
                    }) as Box<dyn SelectResult>)
                })
            })),
        };

        let ctx = ExecCtx::new(s.vars.stmt_ctx_arc());
        let err = ir.open(&ctx).await.unwrap_err();
        assert!(err.to_string().contains("3rd select result boom"), "{err}");
        assert!(
            closed0.load(Ordering::SeqCst),
            "1st opened SelectResult must be closed on 3rd failure"
        );
        assert!(
            closed1.load(Ordering::SeqCst),
            "2nd opened SelectResult must be closed on 3rd failure"
        );
        assert!(ir.result.is_none());
    }

    #[tokio::test]
    async fn need_merge_sort_merges_multiple_streams_and_respects_output_offsets() {
        let col_k = Arc::new(Column {
            id: 2,
            unique_id: 2,
            index: 0,
            ret_type: Arc::new(int_ft()),
            ..Default::default()
        });
        // Reverse output columns: [id (offset 1), k (offset 0)] and sort by k (schema col 1) DESC.
        let by_items = vec![ByItems {
            expr: Expr::Column(Column {
                id: 2,
                unique_id: 2,
                index: 1,
                ..(*col_k).clone()
            })
            .into_ref(),
            desc: true,
        }];
        let (tbl, plan) = make_index_table_and_plan(
            vec![],
            vec![vec![int_range(1, 5)], vec![int_range(10, 15)]],
            by_items,
            &[1, 0],
        );
        let is = make_info_schema(tbl).await;
        let mut s = MockSession::new(100);
        let mut b = new_executor_builder(&s, Some(is), Arc::new(TelemetryInfo::default()));
        let mut ir = b.build_index_reader(&mut s, &plan).await.expect("built");

        // Verify OutputOffsets preserve PhysicalIndexReader.output_columns indices [1, 0].
        assert_eq!(ir.dag_pb.get_output_offsets(), &[1, 0]);
        assert!(need_merge_sort(&ir.by_items, 2));
        assert!(!need_merge_sort(&ir.by_items, 1));
        assert!(!need_merge_sort(&[], 2));

        let closed0 = Arc::new(AtomicBool::new(false));
        let closed1 = Arc::new(AtomicBool::new(false));
        let call_num = Arc::new(AtomicUsize::new(0));
        let c0 = Arc::clone(&closed0);
        let c1 = Arc::clone(&closed1);
        let cn = Arc::clone(&call_num);
        ir.select_result_hook = SelectResultHook {
            select_result_func: Some(Arc::new(move |_, _, _, _, _| {
                let idx = cn.fetch_add(1, Ordering::SeqCst);
                let (rows, c) = if idx == 0 {
                    // Each row is [id, k], sorted DESC by k.
                    (vec![vec![5, 50], vec![2, 20]], Arc::clone(&c0))
                } else {
                    (vec![vec![6, 60], vec![3, 30], vec![1, 10]], Arc::clone(&c1))
                };
                Box::pin(async move {
                    Ok(Box::new(TrackedMockSelectResult {
                        rows,
                        pos: 0,
                        closed: c,
                        next_err: None,
                        close_err: None,
                    }) as Box<dyn SelectResult>)
                })
            })),
        };

        let ctx = ExecCtx::new(s.vars.stmt_ctx_arc());
        ir.open(&ctx).await.unwrap();
        assert_eq!(call_num.load(Ordering::SeqCst), 2);
        let mut chk = exec::new_first_chunk(&ir);
        let mut got = Vec::new();
        loop {
            ir.next(&ctx, &mut chk).await.unwrap();
            if chk.num_rows() == 0 {
                break;
            }
            for r in 0..chk.num_rows() {
                let row = chk.get_row(r);
                got.push((row.get_int64(0), row.get_int64(1)));
            }
        }
        assert_eq!(got, vec![(6, 60), (5, 50), (3, 30), (2, 20), (1, 10)]);
        ir.close().await.unwrap();
        assert!(closed0.load(Ordering::SeqCst));
        assert!(closed1.load(Ordering::SeqCst));
        assert!(ir.kv_ranges.is_empty());
    }

    #[tokio::test]
    async fn compose_with_selection_projection_limit_sort_topn_and_detach_and_kill() {
        let (tbl, plan) = make_index_table_and_plan(vec![int_range(1, 100)], vec![], vec![], &[0]);
        let is = make_info_schema(tbl).await;
        let mut s = MockSession::new(100);
        let mut b = new_executor_builder(&s, Some(is), Arc::new(TelemetryInfo::default()));
        let mut ir = b.build_index_reader(&mut s, &plan).await.expect("built");

        let rows_data = vec![vec![30], vec![10], vec![50], vec![20], vec![40]];
        let hook_rows = rows_data.clone();
        ir.select_result_hook = SelectResultHook {
            select_result_func: Some(Arc::new(move |_, _, _, _, _| {
                let rows = hook_rows.clone();
                Box::pin(async move {
                    Ok(Box::new(TrackedMockSelectResult {
                        rows,
                        pos: 0,
                        closed: Arc::new(AtomicBool::new(false)),
                        next_err: None,
                        close_err: None,
                    }) as Box<dyn SelectResult>)
                })
            })),
        };

        // 1. Detach works and both original and detached executors execute independently.
        let mut detached = crate::detach::detach(&ir).expect("IndexReader must detach");
        let ctx = ExecCtx::new(s.vars.stmt_ctx_arc());
        detached.open(&ctx).await.unwrap();
        let mut chk = exec::new_first_chunk(detached.as_ref());
        detached.next(&ctx, &mut chk).await.unwrap();
        assert_eq!(chk.num_rows(), 5);
        detached.close().await.unwrap();

        // 2. Compose: TopN(2, DESC) > Selection(col0 > 15) > Projection(col0) > IndexReader.
        let schema = ir.schema();
        let col0 = Arc::clone(&schema.columns[0]);
        let proj = Box::new(ProjectionExec::new(
            new_projection_executor_context(&s),
            new_base_executor_v2(
                &s.vars,
                &s.sql_killer(),
                Some(schema.clone()),
                20,
                vec![Box::new(ir)],
            ),
            0,
            new_evaluator_suite(&[Expr::Column((*col0).clone()).into_ref()], false),
            false,
        ));
        let gt_filter = new_function(
            &*s.get_expr_ctx(),
            ast::GT,
            int_ft(),
            vec![
                Expr::Column((*col0).clone()).into_ref(),
                Expr::Constant(Constant::new(new_int_datum(15), int_ft())).into_ref(),
            ],
        )
        .unwrap();
        let sel = Box::new(SelectionExec {
            ctx: new_selection_executor_context(&s),
            base: new_base_executor_v2(
                &s.vars,
                &s.sql_killer(),
                Some(schema.clone()),
                21,
                vec![proj],
            ),
            batched: false,
            filters: vec![gt_filter],
            selected: Vec::new(),
            input_row: None,
            child_result: None,
            mem_tracker: None,
            eval_state: EvalState::new(),
            rpn: tidb_expression::eval::RpnState::default(),
        });
        let sort_base = new_sort_exec(
            new_base_executor(
                &s.vars,
                &s.sql_killer(),
                Some(schema.clone()),
                22,
                vec![sel],
            ),
            vec![ByItems {
                expr: Expr::Column((*col0).clone()).into_ref(),
                desc: true,
            }],
            schema.clone(),
            s.get_expr_ctx(),
            s.vars.state.mem_tracker_bytes_limit,
            1,
        );
        let topn = Box::new(new_top_n_exec(
            sort_base,
            PhysicalLimit {
                offset: 1,
                count: 2,
                ..Default::default()
            },
            1,
        ));
        let mut lim = LimitExec {
            base: new_base_executor(&s.vars, &s.sql_killer(), Some(schema), 23, vec![topn]),
            begin: 0,
            end: 2,
            cursor: 0,
            meet_first_batch: false,
            child_result: None,
            column_idxs_used_by_child: None,
            column_swap_helper: None,
        };
        lim.open(&ctx).await.unwrap();
        let mut chk = exec::new_first_chunk(&lim);
        let mut got = Vec::new();
        loop {
            exec::next(&ctx, &mut lim, &mut chk).await.unwrap();
            if chk.num_rows() == 0 {
                break;
            }
            for r in 0..chk.num_rows() {
                got.push(chk.get_row(r).get_int64(0));
            }
        }
        // Filtered (>15): [30, 50, 20, 40]; DESC: [50, 40, 30, 20]; TopN(offset 1, count 2): [40, 30].
        assert_eq!(got, vec![40, 30]);
        lim.close().await.unwrap();

        // 3. KILL cancellation via exec::next stops cleanly with QueryInterrupted.
        let mut ir_kill = crate::detach::detach(detached.as_ref()).unwrap();
        ir_kill.open(&ctx).await.unwrap();
        s.sql_killer().send_kill_signal(QueryInterrupted);
        let mut chk_kill = exec::new_first_chunk(ir_kill.as_ref());
        let kill_err = exec::next(&ctx, ir_kill.as_mut(), &mut chk_kill)
            .await
            .unwrap_err();
        assert!(
            kill_err
                .to_string()
                .contains("Query execution was interrupted"),
            "{kill_err}"
        );
        ir_kill.close().await.unwrap();
    }

    #[tokio::test]
    async fn pre_open_boundary_guards_leave_stmt_ctx_untouched() {
        // 1. Static partition -> blocked-by:D06.4
        {
            let (tbl, mut plan) =
                make_index_table_and_plan(vec![int_range(1, 10)], vec![], vec![], &[0]);
            let mut scan = plan.index_plans[0]
                .as_any()
                .downcast_ref::<PhysicalIndexScan>()
                .unwrap()
                .clone();
            scan.is_partition = true;
            scan.physical_table_id = 999;
            let scan_ref: PhysicalPlanRef = Arc::new(scan);
            plan.index_plan = Some(Arc::clone(&scan_ref));
            plan.index_plans = vec![scan_ref];

            let is = make_info_schema(tbl).await;
            let mut s = MockSession::new(100);
            let mut b = new_executor_builder(&s, Some(is), Arc::new(TelemetryInfo::default()));
            assert!(b.build_index_reader(&mut s, &plan).await.is_none());
            assert_eq!(
                tidb_base::blocked::blocked_by(&b.err.unwrap()),
                Some("D06.4")
            );
            assert!(!s.vars.stmt_ctx().is_tikv.load(Ordering::SeqCst));
            assert!(s.vars.stmt_ctx().index_names.is_empty());
        }

        // 2. Dynamic partition pruning -> blocked-by:D09.12
        {
            let (tbl_orig, mut plan) =
                make_index_table_and_plan(vec![int_range(1, 10)], vec![], vec![], &[0]);
            let mut tbl = (*tbl_orig).clone();
            tbl.partition = Some(Box::new(PartitionInfo {
                enable: true,
                ..Default::default()
            }));
            let tbl = Arc::new(tbl);
            let mut scan = plan.index_plans[0]
                .as_any()
                .downcast_ref::<PhysicalIndexScan>()
                .unwrap()
                .clone();
            scan.table = Arc::clone(&tbl);
            let scan_ref: PhysicalPlanRef = Arc::new(scan);
            plan.index_plan = Some(Arc::clone(&scan_ref));
            plan.index_plans = vec![scan_ref];

            let mut s = MockSession::new(100);
            s.vars.stmt_ctx_mut().use_dynamic_prune_mode = true;
            let mut b = new_executor_builder(&s, None, Arc::new(TelemetryInfo::default()));
            assert!(b.build_index_reader(&mut s, &plan).await.is_none());
            assert_eq!(
                tidb_base::blocked::blocked_by(&b.err.unwrap()),
                Some("D09.12")
            );
            assert!(!s.vars.stmt_ctx().is_tikv.load(Ordering::SeqCst));
            assert!(s.vars.stmt_ctx().index_names.is_empty());
        }

        // 3. Cached table -> blocked-by:D06.4
        {
            let (tbl_orig, mut plan) =
                make_index_table_and_plan(vec![int_range(1, 10)], vec![], vec![], &[0]);
            let mut tbl = (*tbl_orig).clone();
            tbl.table_cache_status_type = TableCacheStatusEnable;
            let tbl = Arc::new(tbl);
            let mut scan = plan.index_plans[0]
                .as_any()
                .downcast_ref::<PhysicalIndexScan>()
                .unwrap()
                .clone();
            scan.table = Arc::clone(&tbl);
            let scan_ref: PhysicalPlanRef = Arc::new(scan);
            plan.index_plan = Some(Arc::clone(&scan_ref));
            plan.index_plans = vec![scan_ref];

            let is = make_info_schema(tbl).await;
            let mut s = MockSession::new(100);
            let mut b = new_executor_builder(&s, Some(is), Arc::new(TelemetryInfo::default()));
            assert!(b.build_index_reader(&mut s, &plan).await.is_none());
            assert_eq!(
                tidb_base::blocked::blocked_by(&b.err.unwrap()),
                Some("D06.4")
            );
            assert!(!s.vars.stmt_ctx().is_tikv.load(Ordering::SeqCst));
            assert!(s.vars.stmt_ctx().index_names.is_empty());
        }

        // 4. Correlated column in access condition -> blocked-by:D10.1
        {
            let (tbl, mut plan) =
                make_index_table_and_plan(vec![int_range(1, 10)], vec![], vec![], &[0]);
            let mut scan = plan.index_plans[0]
                .as_any()
                .downcast_ref::<PhysicalIndexScan>()
                .unwrap()
                .clone();
            scan.access_condition = vec![
                Expr::CorrelatedColumn(CorrelatedColumn::new(Column {
                    id: 2,
                    unique_id: 2,
                    ret_type: Arc::new(int_ft()),
                    ..Default::default()
                }))
                .into_ref(),
            ];
            let scan_ref: PhysicalPlanRef = Arc::new(scan);
            plan.index_plan = Some(Arc::clone(&scan_ref));
            plan.index_plans = vec![scan_ref];

            let is = make_info_schema(tbl).await;
            let mut s = MockSession::new(100);
            let mut b = new_executor_builder(&s, Some(is), Arc::new(TelemetryInfo::default()));
            assert!(b.build_index_reader(&mut s, &plan).await.is_none());
            assert_eq!(
                tidb_base::blocked::blocked_by(&b.err.unwrap()),
                Some("D10.1")
            );
            assert!(!s.vars.stmt_ctx().is_tikv.load(Ordering::SeqCst));
            assert!(s.vars.stmt_ctx().index_names.is_empty());
        }
    }
}
