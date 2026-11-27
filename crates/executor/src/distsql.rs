//! go-file: pkg/executor/distsql.go
//!
//! DistSQL reader helpers (`closeAll`, `getPhysicalPlanIDs`, block D12.1 Part 2A),
//! `IndexReaderExecutor` (block D12.1 Part 2B), and `IndexLookUpExecutor` (block D12.1 Part 2C:
//! `IndexLookUpExecutor`, `indexWorker`, `tableWorker`, `lookupTableTask`,
//! `IndexLookUpRunTimeStats`, `GetLackHandles`, concurrency & batch-size helpers).

use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, AtomicI64, Ordering};
use std::time::Instant;

use async_trait::async_trait;
use parking_lot::Mutex as SyncMutex;
use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_base::gostd::sort as gosort;
use tidb_base::mysql::{TypeLonglong, has_binary_flag};
use tidb_chunk::ColAlloc;
use tidb_chunk::chunk::{self, Chunk};
use tidb_chunk::row::{Row, RowHandle};
use tidb_codec::codec::encode_key;
use tidb_execdetails::execdetails::{
    BasicRuntimeStats, RuntimeStats, RuntimeStatsColl, TpIndexLookUpRunTimeStats, format_duration,
};
use tidb_expression::context::BuildContext;
use tidb_expression::util::extract_cor_columns;
use tidb_expression::{Column, Expr, ExprRef, Schema};
use tidb_infoschema::context::MetaOnlyInfoSchema;
use tidb_infoschema::table::{PhysicalTable, TableRef};
use tidb_kv::copr::RateLimit;
use tidb_kv::key::{
    Handle, HandleMap, IntHandle, KeyRange, new_common_handle, new_handle_map, new_partition_handle,
};
use tidb_kv::kv::Request;
use tidb_memory::TrackedAlloc;
use tidb_memory::tracker::{LabelForIndexWorker, Tracker, new_tracker};
use tidb_model::{ColumnInfo, ExtraHandleID, ExtraPhysTblID, IndexInfo};
use tidb_parser::gocharset::CollationBin;
use tidb_parser::types::{ETString, FieldType};
use tidb_planner::core::find_best_task::group_ranges_by_cols;
use tidb_planner::core::physical_plans::{PhysicalIndexScan, PushedDownLimit};
use tidb_planner::ranger::detacher::detach_simple_cond_and_build_range_for_index;
use tidb_planner::util::byitem::ByItems;
use tidb_runtime::channel as runtime_channel;
use tidb_runtime::slice::CheckpointStride;
use tidb_sessionctx::distsql_ctx::DistSQLContext;
use tidb_sessionctx::exprctx::ExprContextRef;
use tidb_sessionctx::planctx::BuildPBContext;
use tidb_sessionctx::ranger::Range;
use tidb_sessionctx::ranger::context::RangerContext;
use tidb_store::distsql::{
    RequestBuilder, SelectResult, SelectResultIter, common_handle_ranges_to_kv_ranges,
    get_select_result_concurrency, index_ranges_to_kv_ranges_with_interrupt_signal,
    new_sorted_select_results,
};
use tidb_tablecodec::tablecodec::truncate_index_values;
use tidb_types::collate::new_collation_enabled;
use tidb_types::datum::Datum;
use tidb_types::field_type::new_field_type;
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::builder::{DataReaderBuilder, get_physical_table_id, new_closest_read_adjuster};
use crate::consistency::Reporter;
use crate::exec::ExecCtx;
use crate::glue::{Plan, SessionContext};
use crate::internal::builder::builder_utils::{
    construct_list_based_dist_exec, construct_list_based_dist_exec_for_un_nature_order_plans,
};
use crate::internal::exec::executor::{
    self as exec, BaseExecutorV2, Executor, ExecutorChunkAllocator, ExecutorKillerHandler,
    ExecutorStats, new_executor_meta,
};
use crate::internal::exec::indexusage::IndexUsageReporter;
use crate::sortexec::sort_util::get_recover_error;
use crate::table_reader::{
    SelectResultHook, TableReaderExecutor, TableReaderExecutorContext, kv_ctx_from_dctx,
    new_table_reader_executor_context,
};

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

// ============================================================================
// IndexLookUpExecutor (block D12.1 Part 2C)
// ============================================================================

/// LookupTableTaskChannelSize represents the channel size of the index double read taskChan.
#[allow(non_upper_case_globals)]
pub static LookupTableTaskChannelSize: AtomicI32 = AtomicI32::new(50);

/// `go f()` on the statement's `ExecPool` when present, else on the current tokio runtime.
// glue: go f() on the N3 ExecPool
fn spawn_lookup_worker<F>(ctx: &ExecCtx, name: &'static str, fut: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    match &ctx.pool {
        Some(pool) => pool.spawn(name, fut),
        None => tokio::spawn(fut),
    }
}

/// Completed task payload delivered from `tableWorker` (or pushdown hit) to `IndexLookUpExecutor.Next`
/// over a `oneshot` channel (`L2 [M-CH, M-ROW]`).
pub struct LookupTableTaskResult {
    /// Output rows stored as `(Arc<Chunk>, row_idx)` handles (`M-ROW`, N5a).
    pub rows: Vec<RowHandle>,
    /// Original index handle position for each row; only populated when `keep_order` is true.
    pub row_idx: Vec<usize>,
    /// Read cursor advanced by `IndexLookUpExecutor.Next`.
    pub cursor: usize,
    /// Instant when `buildTableReader` finished, for `Next` wait duration statistics.
    pub build_done_time: Option<Instant>,
    /// Keeps the A1 `TrackedAlloc` alive until this task is released so dropping it flushes memory.
    _alloc_guard: Option<TrackedAlloc>,
}

impl LookupTableTaskResult {
    /// Returns the number of rows in the task (Go `lookupTableTask.Len`).
    // go: lookupTableTask.Len L1
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    // glue: clippy len_without_is_empty companion
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Compares two rows by their original index order (Go `lookupTableTask.Less`).
    // go: lookupTableTask.Less L1
    pub fn less(&self, i: usize, j: usize) -> bool {
        self.row_idx[i] < self.row_idx[j]
    }

    /// Swaps two rows and their index-order positions in lockstep (Go `lookupTableTask.Swap`).
    // go: lookupTableTask.Swap L1
    pub fn swap(&mut self, i: usize, j: usize) {
        self.row_idx.swap(i, j);
        self.rows.swap(i, j);
    }

    /// Sorts `rows` and `row_idx` using Go's pdqsort order over `(len, less, swap)`.
    // glue: sort.Sort(task) over LookupTableTaskResult
    fn sort_by_index_order(&mut self) {
        let n = self.len();
        if n <= 1 {
            return;
        }
        let mut order: Vec<usize> = (0..n).collect();
        gosort::sort(&mut order, |&a, &b| self.less(a, b));
        // Apply permutation in-place via `swap`.
        let mut inv = vec![0usize; n];
        for (pos, &orig) in order.iter().enumerate() {
            inv[orig] = pos;
        }
        for i in 0..n {
            while inv[i] != i {
                let target = inv[i];
                self.swap(i, target);
                inv.swap(i, target);
            }
        }
    }
}

/// Input task dispatched from `indexWorker` to `tableWorker` over `work_ch`.
pub(crate) struct LookupTableWorkTask {
    handles: Vec<Handle>,
    index_order: Option<HandleMap<usize>>,
    #[allow(dead_code)]
    duplicated_index_order: Option<HandleMap<usize>>,
    #[allow(dead_code)]
    idx_rows: Option<Chunk>,
    partition_table: Option<Arc<dyn PhysicalTable>>,
    done_tx: oneshot::Sender<Result<LookupTableTaskResult, Error>>,
}

/// Channel item received by `IndexLookUpExecutor.Next` from `result_ch`.
type ResultTaskHandle = oneshot::Receiver<Result<LookupTableTaskResult, Error>>;

/// indexLookUpExecutorContext is the frozen execution context for [`IndexLookUpExecutor`].
#[derive(Clone)]
pub struct IndexLookUpExecutorContext {
    pub table_ctx: TableReaderExecutorContext,
    pub stmt_runtime_stats_coll: Option<Arc<RuntimeStatsColl>>,
    pub index_lookup_size: isize,
    pub index_lookup_concurrency: isize,
    pub enable_redact_log: String,
    pub weak_consistency: bool,
}

/// newIndexLookUpExecutorContext builds the frozen execution context for [`IndexLookUpExecutor`].
// go: newIndexLookUpExecutorContext L2 (N2 freezes session contexts; omits kv.Storage MVCC debug dump)
pub fn new_index_look_up_executor_context(
    sctx: &dyn SessionContext,
) -> Result<IndexLookUpExecutorContext, Error> {
    let table_ctx = new_table_reader_executor_context(sctx)?;
    let vars = sctx.get_session_vars();
    let stmt_ctx = vars.stmt_ctx();
    Ok(IndexLookUpExecutorContext {
        table_ctx,
        stmt_runtime_stats_coll: stmt_ctx.runtime_stats_coll.as_ref().map(Arc::clone),
        index_lookup_size: vars.index_lookup_size as isize,
        index_lookup_concurrency: vars.index_lookup_concurrency() as isize,
        enable_redact_log: vars.enable_redact_log.clone(),
        weak_consistency: stmt_ctx.weak_consistency,
    })
}

/// KV ranges grouped with their physical table ID.
#[derive(Clone, Debug)]
pub struct KvRangesWithPhysicalTblID {
    pub physical_table_id: i64,
    pub key_ranges: Vec<KeyRange>,
}

/// Whether `get_handle` is decoding from an index scan row or a table scan row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i8)]
pub enum GetHandleType {
    FromIndex = 0,
    FromTable = 1,
}

/// Consistency check payload used only by `ADMIN CHECK TABLE/INDEX` (D12.14).
#[derive(Clone)]
pub struct CheckIndexValue {
    pub idx_col_tps: Vec<Arc<FieldType>>,
}

/// IndexLookUpRunTimeStats records runtime statistics for [`IndexLookUpExecutor`].
#[derive(Debug)]
pub struct IndexLookUpRunTimeStats {
    index_scan_basic_stats: SyncMutex<Arc<BasicRuntimeStats>>,
    pub fetch_handle_total: AtomicI64,
    pub fetch_handle: AtomicI64,
    pub task_wait: AtomicI64,
    pub table_row_scan: AtomicI64,
    pub table_task_num: AtomicI64,
    pub concurrency: isize,
    pub next_wait_index_scan: AtomicI64,
    pub next_wait_table_lookup_build: AtomicI64,
    pub next_wait_table_lookup_resp: AtomicI64,
}

impl IndexLookUpRunTimeStats {
    // glue: constructor for IndexLookUpRunTimeStats
    pub fn new(concurrency: isize) -> Self {
        Self {
            index_scan_basic_stats: SyncMutex::new(Arc::new(BasicRuntimeStats::default())),
            fetch_handle_total: AtomicI64::new(0),
            fetch_handle: AtomicI64::new(0),
            task_wait: AtomicI64::new(0),
            table_row_scan: AtomicI64::new(0),
            table_task_num: AtomicI64::new(0),
            concurrency,
            next_wait_index_scan: AtomicI64::new(0),
            next_wait_table_lookup_build: AtomicI64::new(0),
            next_wait_table_lookup_resp: AtomicI64::new(0),
        }
    }

    // glue: snapshot value copy for registration and Clone
    pub fn snapshot(&self) -> Self {
        let basic = Arc::clone(&self.index_scan_basic_stats.lock());
        Self {
            index_scan_basic_stats: SyncMutex::new(basic),
            fetch_handle_total: AtomicI64::new(self.fetch_handle_total.load(Ordering::Relaxed)),
            fetch_handle: AtomicI64::new(self.fetch_handle.load(Ordering::Relaxed)),
            task_wait: AtomicI64::new(self.task_wait.load(Ordering::Relaxed)),
            table_row_scan: AtomicI64::new(self.table_row_scan.load(Ordering::Relaxed)),
            table_task_num: AtomicI64::new(self.table_task_num.load(Ordering::Relaxed)),
            concurrency: self.concurrency,
            next_wait_index_scan: AtomicI64::new(self.next_wait_index_scan.load(Ordering::Relaxed)),
            next_wait_table_lookup_build: AtomicI64::new(
                self.next_wait_table_lookup_build.load(Ordering::Relaxed),
            ),
            next_wait_table_lookup_resp: AtomicI64::new(
                self.next_wait_table_lookup_resp.load(Ordering::Relaxed),
            ),
        }
    }

    // glue: lock-guarded setter for index_scan_basic_stats
    fn set_index_scan_basic_stats(&self, stats: Arc<BasicRuntimeStats>) {
        *self.index_scan_basic_stats.lock() = stats;
    }

    // glue: lock-guarded getter for index_scan_basic_stats
    fn get_index_scan_basic_stats(&self) -> Arc<BasicRuntimeStats> {
        Arc::clone(&self.index_scan_basic_stats.lock())
    }
}

impl RuntimeStats for IndexLookUpRunTimeStats {
    // go: IndexLookUpRunTimeStats.String L1
    fn string(&self) -> String {
        use std::fmt::Write as _;
        let mut buf = String::new();
        let fetch_handle = self.fetch_handle_total.load(Ordering::Relaxed);
        let index_scan = self.fetch_handle.load(Ordering::Relaxed);
        let task_wait = self.task_wait.load(Ordering::Relaxed);
        let table_scan = self.table_row_scan.load(Ordering::Relaxed);
        let table_task_num = self.table_task_num.load(Ordering::Relaxed);
        let concurrency = self.concurrency;
        let next_wait_index_scan = self.next_wait_index_scan.load(Ordering::Relaxed);
        let next_wait_table_lookup_build =
            self.next_wait_table_lookup_build.load(Ordering::Relaxed);
        let next_wait_table_lookup_resp = self.next_wait_table_lookup_resp.load(Ordering::Relaxed);

        if index_scan != 0 {
            let _ = write!(
                buf,
                "index_task: {{total_time: {}, fetch_handle: {}, build: {}, wait: {}}}",
                format_duration(fetch_handle),
                format_duration(index_scan),
                format_duration(fetch_handle - index_scan - task_wait),
                format_duration(task_wait),
            );
        }
        if table_scan != 0 {
            if !buf.is_empty() {
                buf.push(',');
            }
            let _ = write!(
                buf,
                " table_task: {{total_time: {}, num: {}, concurrency: {}}}",
                format_duration(table_scan),
                table_task_num,
                concurrency,
            );
        }
        if next_wait_index_scan > 0
            || next_wait_table_lookup_build > 0
            || next_wait_table_lookup_resp > 0
        {
            // Keep Go's `if buf.Len() > 0` nesting verbatim (spec §14 item 5).
            if !buf.is_empty() {
                buf.push(',');
                let _ = write!(
                    buf,
                    " next: {{wait_index: {}, wait_table_lookup_build: {}, wait_table_lookup_resp: {}}}",
                    format_duration(next_wait_index_scan),
                    format_duration(next_wait_table_lookup_build),
                    format_duration(next_wait_table_lookup_resp),
                );
            }
        }
        buf
    }

    // go: IndexLookUpRunTimeStats.Clone L1
    fn clone_stats(&self) -> Box<dyn RuntimeStats> {
        Box::new(self.snapshot())
    }

    // go: IndexLookUpRunTimeStats.Merge L1
    fn merge(&mut self, other: &dyn RuntimeStats) {
        let Some(tmp) = other.as_any().downcast_ref::<IndexLookUpRunTimeStats>() else {
            return;
        };
        self.fetch_handle_total.fetch_add(
            tmp.fetch_handle_total.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        self.fetch_handle
            .fetch_add(tmp.fetch_handle.load(Ordering::Relaxed), Ordering::Relaxed);
        self.task_wait
            .fetch_add(tmp.task_wait.load(Ordering::Relaxed), Ordering::Relaxed);
        self.table_row_scan.fetch_add(
            tmp.table_row_scan.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        self.table_task_num.fetch_add(
            tmp.table_task_num.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        self.next_wait_index_scan.fetch_add(
            tmp.next_wait_index_scan.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        self.next_wait_table_lookup_build.fetch_add(
            tmp.next_wait_table_lookup_build.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        self.next_wait_table_lookup_resp.fetch_add(
            tmp.next_wait_table_lookup_resp.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
    }

    // go: IndexLookUpRunTimeStats.Tp L1
    fn tp(&self) -> i64 {
        TpIndexLookUpRunTimeStats
    }

    // glue: type assertion support for RuntimeStats
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// `Send + Sync` snapshot of [`BaseExecutorV2`] fields needed by background workers to
/// construct per-task `TableReaderExecutor` bases via `BaseExecutorV2::build_new_base_executor_v2`.
#[derive(Clone)]
struct IndexLookUpBaseSnapshot {
    id: isize,
    schema: Schema,
    ret_field_types: Arc<[Arc<FieldType>]>,
    killer: ExecutorKillerHandler,
    stats: ExecutorStats,
    alloc: ExecutorChunkAllocator,
}

impl IndexLookUpBaseSnapshot {
    // glue: Send + Sync snapshot of BaseExecutorV2 fields for Arc<IndexLookUpShared>
    fn from_base(base: &BaseExecutorV2) -> Self {
        Self {
            id: base.meta.id(),
            schema: base.meta.schema(),
            ret_field_types: Arc::from(base.meta.ret_field_types()),
            killer: base.killer.clone(),
            stats: base.stats.clone(),
            alloc: base.alloc.clone(),
        }
    }

    // glue: delegates to BaseExecutorV2::build_new_base_executor_v2 from Sync snapshot
    fn build_new_base_executor_v2(
        &self,
        stmt_runtime_stats_coll: Option<&Arc<RuntimeStatsColl>>,
        schema: Option<Schema>,
        id: isize,
    ) -> BaseExecutorV2 {
        let template = BaseExecutorV2 {
            meta: new_executor_meta(Some(self.schema.clone()), self.id, Vec::new()),
            killer: self.killer.clone(),
            stats: self.stats.clone(),
            alloc: self.alloc.clone(),
        };
        template.build_new_base_executor_v2(stmt_runtime_stats_coll, schema, id, Vec::new())
    }
}

/// Immutable/atomic state shared between `indexWorker` and `tableWorker` tasks (`L2 [M-GO]`).
pub(crate) struct IndexLookUpShared {
    ctx: IndexLookUpExecutorContext,
    base: IndexLookUpBaseSnapshot,
    table: Option<TableRef>,
    index: Arc<IndexInfo>,
    /// Post-strip local copy of `dag_pb` (does not mutate the executor's own template across `Open` calls).
    dag_pb: tipb::DagRequest,
    /// Post-strip local copy of `by_items` (`empty` when `kv_ranges.len() == 1`).
    by_items: Vec<ByItems>,
    start_ts: u64,
    txn_scope: String,
    read_replica_scope: String,
    is_staleness: bool,
    handle_idx: Vec<usize>,
    handle_cols: Vec<Arc<Column>>,
    primary_key_index: Option<Arc<IndexInfo>>,
    table_request: tipb::DagRequest,
    columns: Vec<Arc<ColumnInfo>>,
    data_reader_builder: DataReaderBuilder,
    idx_net_data_size: f64,
    avg_row_size: f64,
    partition_table_mode: bool,
    pruned_partitions: Vec<Arc<dyn PhysicalTable>>,
    keep_order: bool,
    desc: bool,
    index_paging: bool,
    cor_col_in_tbl_side: bool,
    idx_plans: Vec<Plan>,
    tbl_plans: Vec<Plan>,
    pushed_limit: Option<PushedDownLimit>,
    index_look_up_push_down: bool,
    check_index_value: Option<CheckIndexValue>,
    mem_tracker: Arc<Tracker>,
    stats: Option<Arc<IndexLookUpRunTimeStats>>,
    select_result_hook: SelectResultHook,
}

impl IndexLookUpShared {
    // go: IndexLookUpExecutor.needPartitionHandle L1
    fn need_partition_handle(&self, tp: GetHandleType) -> Result<bool, Error> {
        if self.index_look_up_push_down {
            debug_assert!(!self.index.global && !self.keep_order);
            return Ok(false);
        }

        let (col, need_partition_handle) = if tp == GetHandleType::FromIndex {
            let cols = &self.idx_plans[0].schema().columns;
            let output_offsets = self.dag_pb.get_output_offsets();
            let last_off = *output_offsets.last().unwrap_or(&0) as usize;
            let col = &cols[last_off];
            let need = self.index.global || (self.partition_table_mode && self.keep_order);
            (col, need)
        } else {
            let cols = &self.tbl_plans[0].schema().columns;
            let output_offsets = self.table_request.get_output_offsets();
            let last_off = *output_offsets.last().unwrap_or(&0) as usize;
            let col = &cols[last_off];
            let need = ((self.index.global || self.partition_table_mode) && self.keep_order)
                || (self.index.global && self.check_index_value.is_some());
            (col, need)
        };
        let has_extra_col = col.id == ExtraPhysTblID as i64;
        if need_partition_handle && !has_extra_col {
            return Err(Error::plain(format!(
                "Internal error, needPartitionHandle != ret, tp({})",
                tp as i8
            )));
        }
        Ok(need_partition_handle)
    }

    // go: IndexLookUpExecutor.isCommonHandle L1
    fn is_common_handle(&self) -> bool {
        let single_extra =
            self.handle_cols.len() == 1 && self.handle_cols[0].id == ExtraHandleID as i64;
        !single_extra
            && self
                .table
                .as_ref()
                .is_some_and(|t| t.meta().is_common_handle)
    }

    // go: IndexLookUpExecutor.getRetTpsForIndexReader L1 [M-SHARE]
    fn get_ret_tps_for_index_reader(&self) -> Vec<Arc<FieldType>> {
        if let Some(check) = &self.check_index_value {
            return check.idx_col_tps.clone();
        }
        let mut tps = Vec::new();
        if !self.by_items.is_empty() {
            let eval_ctx = self.ctx.table_ctx.ectx.get_eval_ctx();
            for item in &self.by_items {
                tps.push(Arc::new(item.expr.get_type(eval_ctx.as_ref()).into_owned()));
            }
        }
        if self.is_common_handle() {
            for handle_col in &self.handle_cols {
                tps.push(Arc::clone(&handle_col.ret_type));
            }
        } else {
            tps.push(Arc::new(new_field_type(TypeLonglong)));
        }
        if self
            .need_partition_handle(GetHandleType::FromIndex)
            .unwrap_or(false)
        {
            tps.push(Arc::new(new_field_type(TypeLonglong)));
        }
        tps
    }

    // go: IndexLookUpExecutor.calculateBatchSize L1
    fn calculate_batch_size(&self, init_batch_size: isize, max_batch_size: isize) -> isize {
        if self.index_paging {
            return init_batch_size.min(max_batch_size);
        }
        let est_rows = self
            .idx_plans
            .first()
            .map_or(0, |p| p.stats_count() as isize);
        calculate_batch_size(est_rows, init_batch_size, max_batch_size)
    }

    // go: IndexLookUpExecutor.getIndexPlanRootID L1
    fn get_index_plan_root_id(&self) -> isize {
        if let Some(last) = self.idx_plans.last() {
            return last.id() as isize;
        }
        self.base.id
    }

    // go: IndexLookUpExecutor.getTableRootPlanID L1
    fn get_table_root_plan_id(&self) -> isize {
        if let Some(last) = self.tbl_plans.last() {
            return last.id() as isize;
        }
        self.base.id
    }

    // go: IndexLookUpExecutor.buildIndexSelectResultForRange L2 (operates on local cloned dag_pb; async select_with_runtime_stats, M-GOCTX)
    #[allow(clippy::too_many_arguments)]
    async fn build_index_select_result_for_range(
        &self,
        kv_ctx: &tidb_kv::Context,
        range_idx: usize,
        mut kv_range: Vec<KeyRange>,
        tbl_scan_idx_for_rewrite_partition_id: Option<usize>,
        tps: Vec<Arc<FieldType>>,
        idx_id: isize,
        tracker: Arc<Tracker>,
        total_ranges: usize,
        batch_size: isize,
        index_scan_concurrency: isize,
        shared_copr_request_rate_limit: Option<Arc<RateLimit>>,
    ) -> Result<Box<dyn SelectResult>, Error> {
        let mut dag_pb = self.dag_pb.clone();
        if let Some(tbl_scan_idx) = tbl_scan_idx_for_rewrite_partition_id
            && let Some(exec_pb) = dag_pb.mut_executors().get_mut(tbl_scan_idx)
        {
            exec_pb
                .mut_tbl_scan()
                .set_table_id(self.pruned_partitions[range_idx].get_physical_id());
        }

        let mut builder = RequestBuilder::default();
        // Set concurrency before SetDAGRequest intentionally: SetDAGRequest may override it
        // (e.g., to 1) for small-limit DAGs.
        if index_scan_concurrency > 0 {
            builder.set_concurrency(index_scan_concurrency);
        }
        let dctx = &self.ctx.table_ctx.dctx;
        builder
            .set_dag_request(dag_pb)
            .set_start_ts(self.start_ts)
            .set_desc(self.desc)
            .set_keep_order(self.keep_order)
            .set_txn_scope(self.txn_scope.clone())
            .set_read_replica_scope(self.read_replica_scope.clone())
            .set_is_staleness(self.is_staleness)
            .set_from_session_vars(dctx);
        if let Some(is) = self.ctx.table_ctx.get_info_schema() {
            builder.set_from_info_schema(is);
        }

        let est_size = self.idx_net_data_size / (total_ranges.max(1) as f64);
        let adjuster = new_closest_read_adjuster(dctx, &builder.request, est_size);
        builder
            .set_closest_replica_read_adjuster(adjuster)
            .set_mem_tracker(Some(tracker))
            .set_conn_id_and_conn_alias(dctx.connection_id, dctx.session_alias.clone())
            .set_copr_request_rate_limit(shared_copr_request_rate_limit);

        if self.index_look_up_push_down {
            // Paging and Cop-cache is not supported in index lookup push down.
            builder.request.paging.enable = false;
            builder.request.cacheable = false;
        }

        if builder.request.paging.enable
            && batch_size > 0
            && builder.request.paging.min_paging_size < batch_size as u64
        {
            builder.request.paging.min_paging_size = batch_size as u64;
            if builder.request.paging.max_paging_size < batch_size as u64 {
                builder.request.paging.max_paging_size = batch_size as u64;
            }
        }

        kv_range.sort_by(|a, b| a.start_key.cmp(&b.start_key).cmp(&0));
        let kv_req = builder.set_key_ranges(kv_range).build()?;
        self.select_result_hook
            .select_result(
                kv_ctx,
                Arc::clone(dctx),
                &kv_req,
                tps,
                get_physical_plan_ids(&self.idx_plans),
                idx_id,
            )
            .await
    }

    // go: IndexLookUpExecutor.buildTableReader L1 [M-GOCTX]
    async fn build_table_reader(
        &self,
        kv_ctx: &tidb_kv::Context,
        handles: Vec<Handle>,
        partition_table: Option<Arc<dyn PhysicalTable>>,
        cancelled: bool,
    ) -> Result<TableReaderExecutor, Error> {
        let table = if self.partition_table_mode
            && let Some(pt) = partition_table
        {
            Some(pt as TableRef)
        } else {
            self.table.as_ref().map(Arc::clone)
        };
        let handle_len = handles.len();
        let mut table_reader_exec = TableReaderExecutor {
            ctx: self.ctx.table_ctx.clone(),
            base: self.base.build_new_base_executor_v2(
                self.ctx.stmt_runtime_stats_coll.as_ref(),
                Some(self.base.schema.clone()),
                self.get_table_root_plan_id(),
            ),
            index_usage_reporter: None,
            table,
            kv_range_builder: None,
            ranges: Vec::new(),
            grouped_ranges: Vec::new(),
            group_by_col_idxs: Vec::new(),
            kv_ranges: Vec::new(),
            dag_pb: self.table_request.clone(),
            start_ts: self.start_ts,
            txn_scope: self.txn_scope.clone(),
            read_replica_scope: self.read_replica_scope.clone(),
            is_staleness: self.is_staleness,
            net_data_size: self.avg_row_size * (handle_len as f64),
            columns: self.columns.clone(),
            result_handler: None,
            plans: self.tbl_plans.clone(),
            table_plan: None,
            mem_tracker: None,
            select_result_hook: SelectResultHook::default(),
            keep_order: false,
            desc: false,
            by_items: self.by_items.clone(),
            paging: false,
            store_type: tidb_base::kv::TiKV,
            cor_col_in_filter: self.cor_col_in_tbl_side,
            cor_col_in_access: false,
            virtual_column_index: Vec::new(),
            virtual_column_ret_field_types: Vec::new(),
            batch_cop: false,
            dummy: false,
        };
        table_reader_exec.build_virtual_column_info();
        match self
            .data_reader_builder
            .build_table_reader_from_handles(kv_ctx, table_reader_exec, handles, true)
            .await
        {
            Ok(tr) => Ok(tr),
            Err(err) => {
                if !cancelled {
                    tidb_base::log_error!(
                        "build table reader from handles failed",
                        "error" => &err,
                    );
                }
                Err(err)
            }
        }
    }

    // go: IndexLookUpExecutor.getHandle L1 [M-FT]
    fn get_handle(
        &self,
        row: &Row<'_>,
        handle_idx: &[usize],
        is_common_handle: bool,
        tp: GetHandleType,
    ) -> Result<Handle, Error> {
        let mut handle = if is_common_handle {
            let tbl_meta = self
                .table
                .as_ref()
                .expect("IndexLookUpExecutor.table must be set for common handle")
                .meta();
            let mut datums: Vec<Datum> = Vec::with_capacity(handle_idx.len());
            for (i, &idx) in handle_idx.iter().enumerate() {
                let rtp = &self.handle_cols[i].ret_type;
                if new_collation_enabled()
                    && tbl_meta.common_handle_version == 0
                    && rtp.eval_type() == ETString
                    && !has_binary_flag(rtp.get_flag())
                    && tp == GetHandleType::FromIndex
                {
                    let mut rtp_bin = (**rtp).clone();
                    rtp_bin.set_collate(CollationBin);
                    datums.push(row.get_datum(idx, &rtp_bin));
                    continue;
                }
                datums.push(row.get_datum(idx, rtp));
            }
            if let Some(pk_idx) = self.primary_key_index.as_deref() {
                truncate_index_values(tbl_meta, pk_idx, &mut datums);
            }
            let ectx = self.ctx.table_ctx.ectx.get_eval_ctx();
            let mut handle_encoded = Vec::new();
            let loc = ectx.location();
            if let Err(err) = encode_key(&loc, &mut handle_encoded, &datums) {
                ectx.err_ctx().handle_error(err)?;
            }
            Handle::Common(Arc::new(new_common_handle(handle_encoded)?))
        } else if handle_idx.is_empty() {
            Handle::Int(IntHandle(row.get_int64(0)))
        } else {
            Handle::Int(IntHandle(row.get_int64(handle_idx[0])))
        };

        if self.need_partition_handle(tp)? {
            let pid = row.get_int64(row.len() - 1);
            handle = Handle::Partition(new_partition_handle(pid, handle));
        }
        Ok(handle)
    }
}

/// IndexLookUpExecutor implements double read for index scan.
pub struct IndexLookUpExecutor {
    pub ctx: IndexLookUpExecutorContext,
    pub base: BaseExecutorV2,
    pub index_usage_reporter: Option<IndexUsageReporter>,

    pub table: Option<TableRef>,
    pub index: Arc<IndexInfo>,
    pub ranges: Vec<Range>,
    pub grouped_ranges: Vec<Vec<Range>>,
    pub dag_pb: tipb::DagRequest,
    pub start_ts: u64,
    pub txn_scope: String,
    pub read_replica_scope: String,
    pub is_staleness: bool,
    pub handle_idx: Vec<usize>,
    pub handle_cols: Vec<Arc<Column>>,
    pub primary_key_index: Option<Arc<IndexInfo>>,
    pub table_request: tipb::DagRequest,

    pub columns: Vec<Arc<ColumnInfo>>,
    pub partition_id_map: HashSet<i64>,

    pub data_reader_builder: DataReaderBuilder,
    pub idx_net_data_size: f64,
    pub avg_row_size: f64,

    pub partition_table_mode: bool,
    pub pruned_partitions: Vec<Arc<dyn PhysicalTable>>,
    pub partition_range_map: HashMap<i64, Vec<Range>>,

    // Runtime state managed between Open / Next / Close / Drop.
    pub(crate) finished_tx: Option<watch::Sender<bool>>,
    pub(crate) result_rx: Option<mpsc::Receiver<ResultTaskHandle>>,
    pub(crate) result_curr: Option<LookupTableTaskResult>,
    pub(crate) idx_worker_handle: Option<JoinHandle<()>>,
    pub(crate) tbl_worker_handles: Vec<JoinHandle<()>>,

    pub mem_tracker: Option<Arc<Tracker>>,
    pub check_index_value: Option<CheckIndexValue>,
    pub grouped_kv_ranges: Vec<KvRangesWithPhysicalTblID>,
    pub worker_started: bool,

    pub by_items: Vec<ByItems>,
    pub keep_order: bool,
    pub desc: bool,
    pub index_paging: bool,

    pub cor_col_in_idx_side: bool,
    pub cor_col_in_tbl_side: bool,
    pub cor_col_in_access: bool,
    pub idx_plans: Vec<Plan>,
    pub idx_plan_un_nature_orders: HashMap<isize, isize>,
    pub tbl_plans: Vec<Plan>,
    pub idx_cols: Vec<Arc<Column>>,
    pub col_lens: Vec<isize>,
    pub pushed_limit: Option<PushedDownLimit>,

    pub stats: Option<Arc<IndexLookUpRunTimeStats>>,
    pub select_result_hook: SelectResultHook,

    /// If dummy flag is set, this is not a real IndexLookUpReader; it only provides KV ranges
    /// for UnionScan. Used by temporary tables and cached tables.
    pub dummy: bool,
    /// Whether to push down the index lookup to TiKV.
    pub index_look_up_push_down: bool,
}

impl Drop for IndexLookUpExecutor {
    // glue: Drop safety net stops and aborts worker tasks if dropped without Close
    fn drop(&mut self) {
        if let Some(tx) = self.finished_tx.take() {
            let _ = tx.send(true);
        }
        if let Some(handle) = self.idx_worker_handle.take() {
            handle.abort();
        }
        for handle in self.tbl_worker_handles.drain(..) {
            handle.abort();
        }
        self.result_rx = None;
        self.result_curr = None;
    }
}

impl IndexLookUpExecutor {
    /// Table implements the dataSourceExecutor interface.
    // go: IndexLookUpExecutor.Table L1
    pub fn table(&self) -> Option<&TableRef> {
        self.table.as_ref()
    }

    // go: IndexLookUpExecutor.setDummy L1
    pub fn set_dummy(&mut self) {
        self.dummy = true;
    }

    /// Open implements the Executor Open interface.
    // go: IndexLookUpExecutor.Open L1 [M-GOCTX]
    pub async fn open_impl(&mut self, ctx: &ExecCtx) -> Result<(), Error> {
        if self.cor_col_in_access {
            let Some(is) = self
                .idx_plans
                .first()
                .and_then(|p| p.as_any().downcast_ref::<PhysicalIndexScan>())
            else {
                return Err(Error::plain(
                    "IndexLookUpExecutor.Open: idxPlans[0] is not *core.PhysicalIndexScan",
                ));
            };
            self.ranges = rebuild_index_ranges(
                self.ctx.table_ctx.ectx.as_ref(),
                &self.ctx.table_ctx.rctx,
                is,
                &self.idx_cols,
                &self.col_lens,
            )?;
            if !is.group_by_col_idxs.is_empty() {
                self.grouped_ranges = group_ranges_by_cols(&self.ranges, &is.group_by_col_idxs)?;
            }
        }

        let tracker = match &self.mem_tracker {
            Some(t) => {
                t.reset()?;
                Arc::clone(t)
            }
            None => {
                let t = new_tracker(self.base.meta.id(), -1);
                self.mem_tracker = Some(Arc::clone(&t));
                t
            }
        };
        if let Some(parent) = &self.ctx.table_ctx.stmt_mem_tracker {
            tracker.attach_to(parent)?;
        }

        debug_assert!(
            !(!self.partition_range_map.is_empty() && !self.grouped_ranges.is_empty()),
            "partitionRangeMap and groupedRanges should not appear together"
        );

        self.build_table_key_ranges()?;

        if self.dummy {
            return Ok(());
        }

        self.open_internal(ctx)
    }

    // go: IndexLookUpExecutor.buildTableKeyRanges L1
    pub fn build_table_key_ranges(&mut self) -> Result<(), Error> {
        let mut table_ids = Vec::with_capacity(self.pruned_partitions.len().max(1));
        if self.partition_table_mode {
            for p in &self.pruned_partitions {
                table_ids.push(p.get_physical_id());
            }
        } else if let Some(tbl) = &self.table {
            table_ids.push(get_physical_table_id(tbl.as_ref()));
        } else {
            return Err(Error::plain(
                "IndexLookUpExecutor.buildTableKeyRanges: table is None",
            ));
        }

        let grouped_ranges: Vec<Vec<Range>> = if self.grouped_ranges.is_empty() {
            vec![self.ranges.clone()]
        } else {
            self.grouped_ranges.clone()
        };

        let mut kv_ranges: Vec<Vec<KeyRange>> = Vec::with_capacity(grouped_ranges.len());
        let mut physical_tbl_ids = Vec::with_capacity(table_ids.len() * grouped_ranges.len());
        for ranges in &grouped_ranges {
            let kv_range = build_key_ranges(
                &self.ctx.table_ctx.dctx,
                ranges,
                &self.partition_range_map,
                &table_ids,
                self.index.id,
                self.mem_tracker.as_ref(),
            )?;
            kv_ranges.extend(kv_range);
            physical_tbl_ids.extend_from_slice(&table_ids);
        }

        if kv_ranges.len() > 1 {
            debug_assert!(self.partition_table_mode || !self.grouped_ranges.is_empty());
        }
        self.grouped_kv_ranges = kv_ranges
            .into_iter()
            .enumerate()
            .map(|(i, key_ranges)| KvRangesWithPhysicalTblID {
                physical_table_id: physical_tbl_ids[i],
                key_ranges,
            })
            .collect();
        Ok(())
    }

    // go: IndexLookUpExecutor.open L2 (watch channel + oneshot result channel, M-CH, M-GOCTX)
    pub fn open_internal(&mut self, _ctx: &ExecCtx) -> Result<(), Error> {
        self.init_runtime_stats();
        let tracker = match &self.mem_tracker {
            Some(t) => {
                t.reset()?;
                Arc::clone(t)
            }
            None => {
                let t = new_tracker(self.base.meta.id(), -1);
                self.mem_tracker = Some(Arc::clone(&t));
                t
            }
        };
        if let Some(parent) = &self.ctx.table_ctx.stmt_mem_tracker {
            tracker.attach_to(parent)?;
        }

        if self.cor_col_in_idx_side {
            let execs = if self.index_look_up_push_down {
                construct_list_based_dist_exec_for_un_nature_order_plans(
                    &self.ctx.table_ctx.build_pb_ctx,
                    &self.idx_plans,
                    &self.idx_plan_un_nature_orders,
                )?
            } else {
                construct_list_based_dist_exec(&self.ctx.table_ctx.build_pb_ctx, &self.idx_plans)?
            };
            self.dag_pb.set_executors(execs.into());
        }

        if self.cor_col_in_tbl_side {
            let execs =
                construct_list_based_dist_exec(&self.ctx.table_ctx.build_pb_ctx, &self.tbl_plans)?;
            self.table_request.set_executors(execs.into());
        }

        self.worker_started = false;
        self.result_curr = None;
        self.finished_tx = None;
        self.result_rx = None;
        self.idx_worker_handle = None;
        self.tbl_worker_handles.clear();
        Ok(())
    }

    // go: IndexLookUpExecutor.initRuntimeStats L1
    fn init_runtime_stats(&mut self) {
        if self.base.stats.runtime_stats().is_some() {
            self.stats = Some(Arc::new(IndexLookUpRunTimeStats::new(
                self.ctx.index_lookup_concurrency,
            )));
        }
    }

    // go: IndexLookUpExecutor.startWorkers L2 (spawns async tasks on ExecCtx.pool; watch cancellation, M-GO, M-CH)
    fn start_workers(&mut self, ctx: &ExecCtx, init_batch_size: usize) -> Result<(), Error> {
        let (finished_tx, finished_rx) = watch::channel(false);
        let ch_size = LookupTableTaskChannelSize.load(Ordering::Relaxed).max(1) as usize;
        let (result_tx, result_rx) = mpsc::channel::<ResultTaskHandle>(ch_size);
        let (work_tx, work_rx) = mpsc::channel::<LookupTableWorkTask>(1);

        let mut local_dag_pb = self.dag_pb.clone();
        if self.base.stats.runtime_stats().is_some() {
            local_dag_pb.set_collect_execution_summaries(true);
        }
        let kv_ranges: Vec<Vec<KeyRange>> = self
            .grouped_kv_ranges
            .iter()
            .map(|r| r.key_ranges.clone())
            .collect();

        // When len(kvRanges) == 1, no merge sort is needed: strip by_items and their leading
        // output offsets on the local worker copy (never mutating self.dag_pb/self.by_items, spec §10.4 & §14.3).
        let mut local_by_items = self.by_items.clone();
        if kv_ranges.len() == 1 && !local_by_items.is_empty() {
            let strip = local_by_items.len();
            let trimmed: Vec<u32> = local_dag_pb
                .get_output_offsets()
                .iter()
                .skip(strip)
                .copied()
                .collect();
            local_dag_pb.set_output_offsets(trimmed);
            local_by_items.clear();
        }

        let mem_tracker = self
            .mem_tracker
            .as_ref()
            .map(Arc::clone)
            .unwrap_or_else(|| new_tracker(self.base.meta.id(), -1));

        let shared = Arc::new(IndexLookUpShared {
            ctx: self.ctx.clone(),
            base: IndexLookUpBaseSnapshot::from_base(&self.base),
            table: self.table.as_ref().map(Arc::clone),
            index: Arc::clone(&self.index),
            dag_pb: local_dag_pb,
            by_items: local_by_items,
            start_ts: self.start_ts,
            txn_scope: self.txn_scope.clone(),
            read_replica_scope: self.read_replica_scope.clone(),
            is_staleness: self.is_staleness,
            handle_idx: self.handle_idx.clone(),
            handle_cols: self.handle_cols.clone(),
            primary_key_index: self.primary_key_index.as_ref().map(Arc::clone),
            table_request: self.table_request.clone(),
            columns: self.columns.clone(),
            data_reader_builder: self.data_reader_builder.clone(),
            idx_net_data_size: self.idx_net_data_size,
            avg_row_size: self.avg_row_size,
            partition_table_mode: self.partition_table_mode,
            pruned_partitions: self.pruned_partitions.clone(),
            keep_order: self.keep_order,
            desc: self.desc,
            index_paging: self.index_paging,
            cor_col_in_tbl_side: self.cor_col_in_tbl_side,
            idx_plans: self.idx_plans.clone(),
            tbl_plans: self.tbl_plans.clone(),
            pushed_limit: self.pushed_limit,
            index_look_up_push_down: self.index_look_up_push_down,
            check_index_value: self.check_index_value.clone(),
            mem_tracker,
            stats: self.stats.as_ref().map(Arc::clone),
            select_result_hook: self.select_result_hook.clone(),
        });

        self.start_index_worker(
            ctx,
            Arc::clone(&shared),
            kv_ranges,
            work_tx,
            result_tx,
            finished_rx.clone(),
            init_batch_size as isize,
        )?;
        self.start_table_worker(ctx, shared, work_rx, finished_rx);

        self.finished_tx = Some(finished_tx);
        self.result_rx = Some(result_rx);
        self.worker_started = true;
        Ok(())
    }

    // go: IndexLookUpExecutor.startIndexWorker L2 (clones local dag_pb/by_items per worker; async task on ExecPool; no trace region, M-GO, M-CH, M-DEFER)
    #[allow(clippy::too_many_arguments)]
    fn start_index_worker(
        &mut self,
        ctx: &ExecCtx,
        shared: Arc<IndexLookUpShared>,
        kv_ranges: Vec<Vec<KeyRange>>,
        work_tx: mpsc::Sender<LookupTableWorkTask>,
        result_tx: mpsc::Sender<ResultTaskHandle>,
        mut finished_rx: watch::Receiver<bool>,
        init_batch_size: isize,
    ) -> Result<(), Error> {
        let tracker = new_tracker(LabelForIndexWorker, -1);
        tracker.attach_to(&shared.mem_tracker)?;

        let mut tbl_scan_idx_for_rewrite_partition_id: Option<usize> = None;
        let tps: Vec<Arc<FieldType>> = if shared.index_look_up_push_down {
            if shared.partition_table_mode {
                for (idx, executor) in shared.dag_pb.get_executors().iter().enumerate() {
                    if executor.get_tp() == tipb::ExecType::TypeTableScan {
                        tbl_scan_idx_for_rewrite_partition_id = Some(idx);
                        break;
                    }
                }
                if tbl_scan_idx_for_rewrite_partition_id.is_none() {
                    debug_assert!(false);
                    return Err(Error::plain(
                        "cannot find table scan executor in for partition index lookup push down",
                    ));
                }
            }
            shared.base.ret_field_types.to_vec()
        } else {
            shared.get_ret_tps_for_index_reader()
        };

        let idx_id = shared.get_index_plan_root_id();
        let need_merge = shared.keep_order && need_merge_sort(&shared.by_items, kv_ranges.len());

        let handle = spawn_lookup_worker(ctx, "IndexLookUpIndexWorker", async move {
            let panic_result_tx = result_tx.clone();
            let mut panic_finished_rx = finished_rx.clone();
            let worker_fut = async move {
                // Drop guard guarantees work_tx closes before result_tx on every exit path (spec §10.5).
                let max_batch_size = shared.ctx.index_lookup_size;
                let batch_size = shared.calculate_batch_size(init_batch_size, max_batch_size);
                let mut worker = IndexWorker {
                    shared: Arc::clone(&shared),
                    work_tx: Some(work_tx),
                    result_tx: Some(result_tx),
                    finished_rx: finished_rx.clone(),
                    keep_order: shared.keep_order,
                    batch_size,
                    max_batch_size,
                    max_chunk_size: shared.base.alloc.max_chunk_size().max(0) as usize,
                    check_index_value: shared.check_index_value.clone(),
                    pushed_limit: shared.pushed_limit,
                    scanned_keys: 0,
                };
                let index_types = shared.get_ret_tps_for_index_reader();
                let kv_ctx = kv_ctx_from_dctx(&shared.ctx.table_ctx.dctx);

                if !need_merge {
                    let max_in_flight = get_index_scan_max_in_flight(
                        shared.ctx.table_ctx.dctx.dist_sql_concurrency,
                    );
                    let kv_ranges_count = kv_ranges.len();
                    let mut next_range = 0usize;
                    let push_down_intermediate_types = vec![index_types.clone()];

                    let err = worker
                        .fetch_handles_rolling(
                            &kv_ctx,
                            max_in_flight,
                            kv_ranges_count,
                            &index_types,
                            |kv_ctx_inner, finished_inner, cur_batch_size| {
                                let shared_c = Arc::clone(&shared);
                                let tps_c = tps.clone();
                                let tracker_c = Arc::clone(&tracker);
                                let push_types = push_down_intermediate_types.clone();
                                let cur_idx = next_range;
                                let range_opt = kv_ranges.get(cur_idx).cloned();
                                if range_opt.is_some() {
                                    next_range += 1;
                                }
                                let kv_ctx_owned = kv_ctx_inner.clone();
                                let mut fin_rx = finished_inner.clone();
                                async move {
                                    let Some(kv_range) = range_opt else {
                                        return Ok(None);
                                    };
                                    if *fin_rx.borrow() {
                                        return Ok(None);
                                    }
                                    let result_res = tokio::select! {
                                        biased;
                                        _ = fin_rx.changed() => return Ok(None),
                                        res = shared_c.build_index_select_result_for_range(
                                            &kv_ctx_owned,
                                            cur_idx,
                                            kv_range,
                                            tbl_scan_idx_for_rewrite_partition_id,
                                            tps_c,
                                            idx_id,
                                            tracker_c,
                                            kv_ranges_count,
                                            cur_batch_size,
                                            0,
                                            None,
                                        ) => res,
                                    };
                                    let mut result = result_res?;
                                    let in_flight_cost =
                                        get_select_result_in_flight_cost(result.as_ref());
                                    let entry = if shared_c.index_look_up_push_down {
                                        match result.into_iter(push_types) {
                                            Ok(row_iter) => SelectResultWithMeta {
                                                result: None,
                                                row_iter: Some(row_iter),
                                                in_flight_cost,
                                            },
                                            Err(e) => {
                                                let _ = result.close();
                                                return Err(e);
                                            }
                                        }
                                    } else {
                                        SelectResultWithMeta {
                                            result: Some(result),
                                            row_iter: None,
                                            in_flight_cost,
                                        }
                                    };
                                    Ok(Some(entry))
                                }
                            },
                        )
                        .await;
                    if let Err(e) = err {
                        worker.sync_err(e).await;
                    }
                    // Explicit close order: work_tx before result_tx (spec §10.5).
                    worker.work_tx = None;
                    worker.result_tx = None;
                    return;
                }

                let shared_copr_rate_limit = get_merge_sort_shared_copr_request_rate_limit(
                    need_merge,
                    shared.ctx.table_ctx.dctx.dist_sql_concurrency,
                );
                let merge_sort_concurrency = get_merge_sort_index_scan_concurrency(
                    need_merge,
                    kv_ranges.len(),
                    shared.ctx.table_ctx.dctx.dist_sql_concurrency,
                );
                let total_ranges = kv_ranges.len();
                let mut results: Vec<Box<dyn SelectResult>> = Vec::with_capacity(total_ranges);
                for (idx, kv_range) in kv_ranges.into_iter().enumerate() {
                    if *finished_rx.borrow() {
                        break;
                    }
                    let res = tokio::select! {
                        biased;
                        _ = finished_rx.changed() => break,
                        r = shared.build_index_select_result_for_range(
                            &kv_ctx,
                            idx,
                            kv_range,
                            tbl_scan_idx_for_rewrite_partition_id,
                            tps.clone(),
                            idx_id,
                            Arc::clone(&tracker),
                            total_ranges,
                            worker.batch_size,
                            merge_sort_concurrency,
                            shared_copr_rate_limit.clone(),
                        ) => r,
                    };
                    match res {
                        Ok(r) => results.push(r),
                        Err(e) => {
                            for mut prev in results {
                                let _ = prev.close();
                            }
                            worker.sync_err(e).await;
                            worker.work_tx = None;
                            worker.result_tx = None;
                            return;
                        }
                    }
                }
                if results.is_empty() {
                    worker.work_tx = None;
                    worker.result_tx = None;
                    return;
                }

                let eval_ctx = shared.ctx.table_ctx.ectx.get_eval_ctx();
                let ssr = match new_sorted_select_results(
                    eval_ctx.as_ref(),
                    results,
                    None,
                    shared.by_items.clone(),
                    Some(Arc::clone(&shared.mem_tracker)),
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        worker.sync_err(e).await;
                        worker.work_tx = None;
                        worker.result_tx = None;
                        return;
                    }
                };
                let merged_results = vec![ssr];
                let sel_result_list = if shared.index_look_up_push_down {
                    match new_select_result_row_iter_list(merged_results, vec![index_types.clone()])
                    {
                        Ok(list) => list,
                        Err(e) => {
                            worker.sync_err(e).await;
                            worker.work_tx = None;
                            worker.result_tx = None;
                            return;
                        }
                    }
                } else {
                    new_select_result_list(merged_results)
                };

                if let Err(e) = worker
                    .fetch_handles(&kv_ctx, sel_result_list, &index_types)
                    .await
                {
                    worker.sync_err(e).await;
                }
                worker.work_tx = None;
                worker.result_tx = None;
            };
            if let Err((payload, bt)) =
                tidb_base::panic_guard::catch_unwind_future(worker_fut).await
            {
                tidb_base::log_warn!(
                    "indexWorker in IndexLookupExecutor panicked",
                    "recover" => tidb_base::panic_guard::panic_payload_log_value(payload.as_ref()),
                    "stack" => bt.as_deref().unwrap_or(""),
                );
                let err = get_recover_error(payload);
                let (done_tx, done_rx) = oneshot::channel();
                let _ = done_tx.send(Err(err));
                if !*panic_finished_rx.borrow() {
                    tokio::select! {
                        biased;
                        _ = panic_finished_rx.changed() => {}
                        _ = panic_result_tx.send(done_rx) => {}
                    }
                }
            }
        });

        self.idx_worker_handle = Some(handle);
        Ok(())
    }

    // go: IndexLookUpExecutor.startTableWorker L2 (spawns async tasks sharing Arc<tokio::sync::Mutex<mpsc::Receiver>>; no trace region, M-GO, M-CH)
    fn start_table_worker(
        &mut self,
        ctx: &ExecCtx,
        shared: Arc<IndexLookUpShared>,
        work_rx: mpsc::Receiver<LookupTableWorkTask>,
        finished_rx: watch::Receiver<bool>,
    ) {
        let lookup_concurrency_limit = shared.ctx.index_lookup_concurrency.max(1) as usize;
        let shared_work_rx = Arc::new(AsyncMutex::new(work_rx));
        self.tbl_worker_handles = Vec::with_capacity(lookup_concurrency_limit);
        for worker_id in 0..lookup_concurrency_limit {
            let worker_tracker = new_tracker(worker_id as isize, -1);
            let _ = worker_tracker.attach_to(&shared.mem_tracker);
            let mut worker = TableWorker {
                shared: Arc::clone(&shared),
                work_rx: Arc::clone(&shared_work_rx),
                finished_rx: finished_rx.clone(),
                keep_order: shared.keep_order,
                handle_idx: shared.handle_idx.clone(),
                mem_tracker: worker_tracker,
                check_index_value: shared.check_index_value.clone(),
            };
            let exec_ctx = ctx.clone();
            let handle = spawn_lookup_worker(ctx, "IndexLookUpTableWorker", async move {
                worker.pick_and_exec_task(&exec_ctx).await;
            });
            self.tbl_worker_handles.push(handle);
        }
    }

    /// Close implements Exec Close interface.
    // go: IndexLookUpExecutor.Close L2 (watch signal + drain result_rx + await JoinHandles + ordered stats registration; omits D14.1 metrics, M-DEFER, M-WG)
    pub async fn close_impl(&mut self) -> Result<(), Error> {
        let res = self.close_body().await;
        // LIFO defer equivalent: register runtime stats last after all workers have joined (spec §10.5).
        if let (Some(stats), Some(coll)) = (&self.stats, &self.ctx.stmt_runtime_stats_coll) {
            coll.register_stats(self.base.meta.id() as i64, Box::new(stats.snapshot()));
        }
        res
    }

    // go-part: IndexLookUpExecutor.Close
    async fn close_body(&mut self) -> Result<(), Error> {
        if let Some(reporter) = &self.index_usage_reporter
            && let Some(tbl) = &self.table
            && let Some(first_plan) = self.idx_plans.first()
        {
            let phys_id = tbl.as_physical_table().map(|p| p.get_physical_id());
            reporter.report_cop_index_usage_for_table(
                tbl.meta(),
                phys_id,
                self.index.id,
                first_plan.id() as isize,
            );
        }
        if self.dummy {
            return Ok(());
        }
        if !self.worker_started || self.finished_tx.is_none() {
            return Ok(());
        }

        if let Some(tx) = self.finished_tx.take() {
            let _ = tx.send(true);
        }
        if let Some(mut rx) = self.result_rx.take() {
            runtime_channel::clear(&mut rx).await;
        }
        // Await handles without removing them until complete so Drop still aborts if Close is cancelled.
        if let Some(handle) = self.idx_worker_handle.as_mut() {
            let _ = handle.await;
            self.idx_worker_handle = None;
        }
        while let Some(handle) = self.tbl_worker_handles.last_mut() {
            let _ = handle.await;
            self.tbl_worker_handles.pop();
        }
        self.worker_started = false;
        self.result_curr = None;
        Ok(())
    }

    /// Next implements Exec Next interface.
    // go: IndexLookUpExecutor.Next L1 [M-GOCTX, M-ROW]
    pub async fn next_impl(&mut self, ctx: &ExecCtx, req: &mut Chunk) -> Result<(), Error> {
        if self.dummy {
            req.reset();
            return Ok(());
        }
        if !self.worker_started {
            self.start_workers(ctx, req.required_rows())?;
        }
        req.reset();
        loop {
            let Some(result_task) = self.get_result_task().await? else {
                return Ok(());
            };
            if result_task.cursor < result_task.rows.len() {
                let remain_task = result_task.rows.len() - result_task.cursor;
                let remain_req = req.required_rows().saturating_sub(req.num_rows());
                let num_to_append = remain_task.min(remain_req);
                for rh in &result_task.rows[result_task.cursor..result_task.cursor + num_to_append]
                {
                    req.append_row(rh.row());
                }
                result_task.cursor += num_to_append;
                if req.is_full() {
                    return Ok(());
                }
            }
        }
    }

    // go: IndexLookUpExecutor.getResultTask L2 (receives completed result over oneshot; A1 memory freed on drop of previous task, M-CH, M-MEM)
    async fn get_result_task(&mut self) -> Result<Option<&mut LookupTableTaskResult>, Error> {
        if let Some(curr) = &self.result_curr
            && curr.cursor < curr.rows.len()
        {
            return Ok(self.result_curr.as_mut());
        }
        let enable_stats = self.stats.is_some();
        let start = enable_stats.then(Instant::now);
        let Some(rx) = self.result_rx.as_mut() else {
            return Ok(None);
        };
        let Some(done_rx) = rx.recv().await else {
            return Ok(None);
        };
        let mut index_fetched_instant = enable_stats.then(Instant::now);
        let task_res = match done_rx.await {
            Ok(res) => res?,
            Err(_) => {
                if self.finished_tx.as_ref().is_some_and(|tx| *tx.borrow()) {
                    return Ok(None);
                }
                return Err(Error::plain(
                    "IndexLookUpExecutor: table worker dropped task channel unexpectedly",
                ));
            }
        };
        if let (Some(stats), Some(start_t), Some(fetched_t)) =
            (&self.stats, start, &mut index_fetched_instant)
        {
            stats.next_wait_index_scan.fetch_add(
                fetched_t.saturating_duration_since(start_t).as_nanos() as i64,
                Ordering::Relaxed,
            );
            if let Some(build_done) = task_res.build_done_time
                && build_done > *fetched_t
            {
                stats.next_wait_table_lookup_build.fetch_add(
                    build_done.saturating_duration_since(*fetched_t).as_nanos() as i64,
                    Ordering::Relaxed,
                );
                *fetched_t = build_done;
            }
            stats
                .next_wait_table_lookup_resp
                .fetch_add(fetched_t.elapsed().as_nanos() as i64, Ordering::Relaxed);
        }
        // Replacing self.result_curr drops the previous task and its TrackedAlloc (A1).
        self.result_curr = Some(task_res);
        Ok(self.result_curr.as_mut())
    }
}

#[async_trait]
impl Executor for IndexLookUpExecutor {
    // glue: M-SELF embedded BaseExecutorV2
    fn base(&self) -> &BaseExecutorV2 {
        &self.base
    }

    // glue: M-SELF embedded BaseExecutorV2
    fn base_mut(&mut self) -> &mut BaseExecutorV2 {
        &mut self.base
    }

    // glue: Executor::open delegates to open_impl
    async fn open(&mut self, ctx: &ExecCtx) -> Result<(), Error> {
        self.open_impl(ctx).await
    }

    // glue: Executor::next delegates to next_impl
    async fn next(&mut self, ctx: &ExecCtx, req: &mut Chunk) -> Result<(), Error> {
        self.next_impl(ctx, req).await
    }

    // glue: Executor::close delegates to close_impl
    async fn close(&mut self) -> Result<(), Error> {
        self.close_impl().await
    }

    // glue: Executor::detach delegates to detach::index_look_up_executor_detach
    fn detach(&self) -> Option<Box<dyn Executor>> {
        crate::detach::index_look_up_executor_detach(self)
    }
}

// ============================================================================
// Batch-size & concurrency helpers
// ============================================================================

// go: getIndexScanMaxInFlight L1
pub fn get_index_scan_max_in_flight(dist_sql_concurrency: isize) -> isize {
    if dist_sql_concurrency < 1 {
        return 1;
    }
    2 * dist_sql_concurrency
}

// go: getSelectResultInFlightCost L1
pub fn get_select_result_in_flight_cost(result: &dyn SelectResult) -> isize {
    let mut in_flight_cost = 1isize;
    if let Some((conc, extra_conc)) = get_select_result_concurrency(result) {
        in_flight_cost = conc + extra_conc;
    }
    if in_flight_cost < 1 {
        in_flight_cost = 1;
    }
    in_flight_cost
}

// go: getMergeSortSharedCoprRequestRateLimit L1
pub fn get_merge_sort_shared_copr_request_rate_limit(
    need_merge: bool,
    dist_sql_concurrency: isize,
) -> Option<Arc<RateLimit>> {
    if !need_merge {
        return None;
    }
    let capacity = dist_sql_concurrency.max(1) as usize;
    Some(Arc::new(RateLimit::new(2 * capacity)))
}

// go: getMergeSortIndexScanConcurrency L1
pub fn get_merge_sort_index_scan_concurrency(
    need_merge: bool,
    kv_ranges_count: usize,
    dist_sql_concurrency: isize,
) -> isize {
    if !need_merge || kv_ranges_count == 0 {
        return 0;
    }
    let base = dist_sql_concurrency.max(1);
    let shared_budget = 4 * base;
    let per_range_concurrency = shared_budget / (kv_ranges_count as isize);
    // CRITICAL: use `.max(2).min(base)`, NEVER `.clamp(2, base)` (panics when base == 1).
    per_range_concurrency.max(2).min(base)
}

/// CalculateBatchSize calculates a suitable initial batch size.
// go: CalculateBatchSize L1 [M-INT]
pub fn calculate_batch_size(
    est_rows: isize,
    init_batch_size: isize,
    max_batch_size: isize,
) -> isize {
    let mut batch_size = init_batch_size.min(max_batch_size);
    if est_rows >= max_batch_size {
        return max_batch_size;
    }
    if batch_size <= 0 {
        return batch_size;
    }
    while batch_size < est_rows {
        if batch_size >= max_batch_size - batch_size {
            return max_batch_size;
        }
        batch_size *= 2;
        if batch_size >= max_batch_size {
            return max_batch_size;
        }
    }
    batch_size
}

// ============================================================================
// indexWorker & selectResult collections
// ============================================================================

struct SelectResultEntry {
    result: Option<Box<dyn SelectResult>>,
    row_iter: Option<Box<dyn SelectResultIter>>,
}

struct SelectResultList(Vec<SelectResultEntry>);

// go: newSelectResultList L1
fn new_select_result_list(results: Vec<Box<dyn SelectResult>>) -> SelectResultList {
    SelectResultList(
        results
            .into_iter()
            .map(|r| SelectResultEntry {
                result: Some(r),
                row_iter: None,
            })
            .collect(),
    )
}

// go: newSelectResultRowIterList L1
fn new_select_result_row_iter_list(
    results: Vec<Box<dyn SelectResult>>,
    intermediate_result_types: Vec<Vec<Arc<FieldType>>>,
) -> Result<SelectResultList, Error> {
    let mut ret = new_select_result_list(results);
    for entry in &mut ret.0 {
        if let Some(res) = entry.result.as_mut() {
            match res.into_iter(intermediate_result_types.clone()) {
                Ok(row_iter) => {
                    entry.row_iter = Some(row_iter);
                    entry.result = None;
                }
                Err(err) => {
                    ret.close();
                    return Err(err);
                }
            }
        }
    }
    Ok(ret)
}

impl SelectResultList {
    // go: selectResultList.Close L1
    fn close(&mut self) {
        for r in &mut self.0 {
            let err = if let Some(iter) = r.row_iter.as_mut() {
                iter.close()
            } else if let Some(res) = r.result.as_mut() {
                res.close()
            } else {
                Ok(())
            };
            if let Err(e) = err {
                tidb_base::log_error!("close Select result failed", "error" => &e);
            }
        }
    }
}

impl Drop for SelectResultList {
    // glue: Drop safety net closes remaining SelectResults/SelectResultIters
    fn drop(&mut self) {
        self.close();
    }
}

struct SelectResultWithMeta {
    result: Option<Box<dyn SelectResult>>,
    row_iter: Option<Box<dyn SelectResultIter>>,
    in_flight_cost: isize,
}

impl SelectResultWithMeta {
    // go: selectResultWithMeta.Close L1
    fn close(&mut self) {
        let err = if let Some(iter) = self.row_iter.as_mut() {
            iter.close()
        } else if let Some(res) = self.result.as_mut() {
            res.close()
        } else {
            Ok(())
        };
        if let Err(e) = err {
            tidb_base::log_error!("close Select result failed", "error" => &e);
        }
    }
}

impl Drop for SelectResultWithMeta {
    // glue: Drop safety net closes SelectResult/SelectResultIter
    fn drop(&mut self) {
        self.close();
    }
}

struct ExtractedLookupTaskData {
    start_time: Instant,
    finish_fetch: Instant,
    completed_rows: Vec<RowHandle>,
    handles: Vec<Handle>,
    ret_chunk: Option<Chunk>,
    exhausted: bool,
}

struct IndexWorker {
    shared: Arc<IndexLookUpShared>,
    work_tx: Option<mpsc::Sender<LookupTableWorkTask>>,
    result_tx: Option<mpsc::Sender<ResultTaskHandle>>,
    finished_rx: watch::Receiver<bool>,
    keep_order: bool,
    batch_size: isize,
    max_batch_size: isize,
    max_chunk_size: usize,
    check_index_value: Option<CheckIndexValue>,
    pushed_limit: Option<PushedDownLimit>,
    scanned_keys: u64,
}

impl IndexWorker {
    // go: indexWorker.syncErr L2 (sends error via pre-resolved oneshot on result_tx, M-CH)
    async fn sync_err(&mut self, err: Error) {
        let (done_tx, done_rx) = oneshot::channel();
        let _ = done_tx.send(Err(err));
        if let Some(tx) = &self.result_tx {
            let mut fin = self.finished_rx.clone();
            tokio::select! {
                biased;
                _ = fin.changed() => {}
                _ = tx.send(done_rx) => {}
            }
        }
    }

    // go: indexWorker.fetchHandles L2 (drop guard closes results; catch_unwind converts panic to Err, M-DEFER, M-PANIC)
    async fn fetch_handles(
        &mut self,
        kv_ctx: &tidb_kv::Context,
        mut results: SelectResultList,
        index_types: &[Arc<FieldType>],
    ) -> Result<(), Error> {
        let res = tidb_base::panic_guard::catch_unwind_future(self.fetch_handles_inner(
            kv_ctx,
            &mut results,
            index_types,
        ))
        .await;
        // Close results deterministically before returning (Drop also serves as safety net).
        results.close();
        match res {
            Ok(inner) => inner,
            Err((payload, bt)) => {
                tidb_base::log_warn!(
                    "indexWorker in IndexLookupExecutor panicked",
                    "recover" => tidb_base::panic_guard::panic_payload_log_value(payload.as_ref()),
                    "stack" => bt.as_deref().unwrap_or(""),
                );
                Err(get_recover_error(payload))
            }
        }
    }

    // go-part: indexWorker.fetchHandles
    async fn fetch_handles_inner(
        &mut self,
        kv_ctx: &tidb_kv::Context,
        results: &mut SelectResultList,
        index_types: &[Arc<FieldType>],
    ) -> Result<(), Error> {
        let (mut chk, handle_offsets) = self.prepare_handle_fetch(index_types)?;
        let mut task_id = 0usize;
        let mut i = 0usize;
        while i < results.0.len() {
            let cur_result_idx = i;
            if let Some(limit) = self.pushed_limit
                && self.scanned_keys >= limit.count.saturating_add(limit.offset)
            {
                break;
            }
            let entry = &mut results.0[cur_result_idx];
            let data = self
                .extract_lookup_task_data(
                    kv_ctx,
                    entry.result.as_deref_mut(),
                    entry.row_iter.as_deref_mut(),
                    chk.as_mut(),
                    &handle_offsets,
                )
                .await?;
            if data.exhausted {
                i += 1;
            }
            if self
                .build_and_dispatch_lookup_tasks(cur_result_idx, &mut task_id, data)
                .await
            {
                return Ok(());
            }
        }
        Ok(())
    }

    // go: indexWorker.fetchHandlesRolling L2 (drop guard closes results; catch_unwind converts panic to Err; select! on finished_rx, M-DEFER, M-PANIC)
    async fn fetch_handles_rolling<F, Fut>(
        &mut self,
        kv_ctx: &tidb_kv::Context,
        max_in_flight: isize,
        kv_ranges_count: usize,
        index_types: &[Arc<FieldType>],
        build_next: F,
    ) -> Result<(), Error>
    where
        F: FnMut(&tidb_kv::Context, &watch::Receiver<bool>, isize) -> Fut,
        Fut: Future<Output = Result<Option<SelectResultWithMeta>, Error>>,
    {
        let res = tidb_base::panic_guard::catch_unwind_future(self.fetch_handles_rolling_inner(
            kv_ctx,
            max_in_flight,
            kv_ranges_count,
            index_types,
            build_next,
        ))
        .await;
        match res {
            Ok(inner) => inner,
            Err((payload, bt)) => {
                tidb_base::log_warn!(
                    "indexWorker in IndexLookupExecutor panicked",
                    "recover" => tidb_base::panic_guard::panic_payload_log_value(payload.as_ref()),
                    "stack" => bt.as_deref().unwrap_or(""),
                );
                Err(get_recover_error(payload))
            }
        }
    }

    // go-part: indexWorker.fetchHandlesRolling
    async fn fetch_handles_rolling_inner<F, Fut>(
        &mut self,
        kv_ctx: &tidb_kv::Context,
        max_in_flight: isize,
        kv_ranges_count: usize,
        index_types: &[Arc<FieldType>],
        mut build_next: F,
    ) -> Result<(), Error>
    where
        F: FnMut(&tidb_kv::Context, &watch::Receiver<bool>, isize) -> Fut,
        Fut: Future<Output = Result<Option<SelectResultWithMeta>, Error>>,
    {
        if kv_ranges_count == 0 {
            return Ok(());
        }
        let max_in_flight = max_in_flight.max(1);
        let mut results: Vec<SelectResultWithMeta> = Vec::with_capacity(kv_ranges_count);
        let mut in_flight: isize = 0;

        let (mut chk, handle_offsets) = self.prepare_handle_fetch(index_types)?;

        while in_flight < max_in_flight {
            let Some(entry) = build_next(kv_ctx, &self.finished_rx, self.batch_size).await? else {
                break;
            };
            in_flight += entry.in_flight_cost;
            results.push(entry);
        }
        debug_assert!(results.len() <= kv_ranges_count);

        let mut task_id = 0usize;
        let mut i = 0usize;
        while i < results.len() {
            let cur_result_idx = i;
            if let Some(limit) = self.pushed_limit
                && self.scanned_keys >= limit.count.saturating_add(limit.offset)
            {
                break;
            }
            let entry = &mut results[cur_result_idx];
            let data = self
                .extract_lookup_task_data(
                    kv_ctx,
                    entry.result.as_deref_mut(),
                    entry.row_iter.as_deref_mut(),
                    chk.as_mut(),
                    &handle_offsets,
                )
                .await?;
            if data.exhausted {
                in_flight -= results[cur_result_idx].in_flight_cost;
                while in_flight < max_in_flight {
                    let Some(entry) =
                        build_next(kv_ctx, &self.finished_rx, self.batch_size).await?
                    else {
                        break;
                    };
                    in_flight += entry.in_flight_cost;
                    results.push(entry);
                }
                debug_assert!(results.len() <= kv_ranges_count);
                i += 1;
            }
            if self
                .build_and_dispatch_lookup_tasks(cur_result_idx, &mut task_id, data)
                .await
            {
                break;
            }
        }
        // Explicitly close all entries on exit (Drop on SelectResultWithMeta also guarantees cleanup).
        for entry in &mut results {
            entry.close();
        }
        Ok(())
    }

    // go: indexWorker.prepareHandleFetch L2 (direct chunk alloc per M-POOL; updates shared index_scan_basic_stats)
    fn prepare_handle_fetch(
        &self,
        index_types: &[Arc<FieldType>],
    ) -> Result<(Option<Chunk>, Vec<usize>), Error> {
        let chk = if !self.shared.index_look_up_push_down {
            Some(chunk::new_chunk_with_capacity(
                index_types,
                self.max_chunk_size,
            ))
        } else {
            None
        };
        let handle_offsets = self.get_handle_offsets(index_types.len())?;
        let idx_id = self.shared.get_index_plan_root_id();
        if let (Some(coll), Some(stats)) =
            (&self.shared.ctx.stmt_runtime_stats_coll, &self.shared.stats)
            && idx_id != self.shared.base.id
        {
            stats.set_index_scan_basic_stats(coll.get_basic_runtime_stats(idx_id as i64));
        }
        Ok((chk, handle_offsets))
    }

    // go: indexWorker.extractLookupTaskData L1 [M-GOCTX]
    async fn extract_lookup_task_data<'a>(
        &mut self,
        kv_ctx: &tidb_kv::Context,
        result: Option<&'a mut (dyn SelectResult + 'static)>,
        row_iter: Option<&'a mut (dyn SelectResultIter + 'static)>,
        chk: Option<&mut Chunk>,
        handle_offsets: &[usize],
    ) -> Result<ExtractedLookupTaskData, Error> {
        let start_time = Instant::now();
        let (completed_rows, handles, ret_chunk, exhausted) = if self.shared.index_look_up_push_down
        {
            let iter = row_iter.expect("row_iter must be set for indexLookUpPushDown");
            let (rows, handles, exhausted) = self
                .extract_look_up_push_down_rows_or_handles(kv_ctx, iter, handle_offsets)
                .await?;
            (rows, handles, None, exhausted)
        } else {
            let res = result.expect("result must be set for normal IndexLookUp");
            let chk_mut = chk.expect("chk must be set for normal IndexLookUp");
            let (handles, ret_chk) = self
                .extract_task_handles(kv_ctx, chk_mut, res, handle_offsets)
                .await?;
            let exhausted = handles.is_empty();
            (Vec::new(), handles, ret_chk, exhausted)
        };
        let finish_fetch = Instant::now();
        Ok(ExtractedLookupTaskData {
            start_time,
            finish_fetch,
            completed_rows,
            handles,
            ret_chunk,
            exhausted,
        })
    }

    // go: indexWorker.buildAndDispatchLookupTasks L2 (select! on finished_rx; omits D14.1 counters; pairs work_tx/result_tx via oneshot, M-CH)
    async fn build_and_dispatch_lookup_tasks(
        &mut self,
        cur_result_idx: usize,
        task_id: &mut usize,
        data: ExtractedLookupTaskData,
    ) -> bool {
        if data.handles.is_empty() && data.completed_rows.is_empty() {
            return false;
        }

        let completed_task = if !data.completed_rows.is_empty() {
            *task_id += 1;
            Some(Self::build_completed_task(data.completed_rows))
        } else {
            None
        };

        let table_pair = if !data.handles.is_empty() {
            let (mut work_task, done_rx) = self.build_table_task(data.handles, data.ret_chunk);
            if self.shared.partition_table_mode {
                work_task.partition_table =
                    Some(Arc::clone(&self.shared.pruned_partitions[cur_result_idx]));
            }
            *task_id += 1;
            Some((work_task, done_rx))
        } else {
            None
        };

        let finish_build = Instant::now();
        if *self.finished_rx.borrow() {
            return true;
        }

        if let Some(completed_rx) = completed_task
            && let Some(result_tx) = &self.result_tx
        {
            let mut fin = self.finished_rx.clone();
            tokio::select! {
                biased;
                _ = fin.changed() => return true,
                res = result_tx.send(completed_rx) => {
                    if res.is_err() {
                        return true;
                    }
                }
            }
        }

        if let Some((work_task, done_rx)) = table_pair
            && let (Some(work_tx), Some(result_tx)) = (&self.work_tx, &self.result_tx)
        {
            let mut fin = self.finished_rx.clone();
            tokio::select! {
                biased;
                _ = fin.changed() => return true,
                res = work_tx.send(work_task) => {
                    if res.is_err() {
                        return true;
                    }
                }
            }
            let mut fin2 = self.finished_rx.clone();
            tokio::select! {
                biased;
                _ = fin2.changed() => return true,
                res = result_tx.send(done_rx) => {
                    if res.is_err() {
                        return true;
                    }
                }
            }
        }

        if let Some(stats) = &self.shared.stats {
            stats.fetch_handle.fetch_add(
                data.finish_fetch
                    .saturating_duration_since(data.start_time)
                    .as_nanos() as i64,
                Ordering::Relaxed,
            );
            stats
                .task_wait
                .fetch_add(finish_build.elapsed().as_nanos() as i64, Ordering::Relaxed);
            stats.fetch_handle_total.fetch_add(
                data.start_time.elapsed().as_nanos() as i64,
                Ordering::Relaxed,
            );
        }
        false
    }

    // go: indexWorker.getHandleOffsets L1
    fn get_handle_offsets(&self, index_tps_len: usize) -> Result<Vec<usize>, Error> {
        let mut num_cols_without_pid = index_tps_len;
        if self
            .shared
            .need_partition_handle(GetHandleType::FromIndex)?
        {
            num_cols_without_pid = num_cols_without_pid.saturating_sub(1);
        }
        let mut handle_offset = Vec::with_capacity(self.shared.handle_cols.len().max(1));
        let hc_len = self.shared.handle_cols.len();
        for i in 0..hc_len {
            handle_offset.push(num_cols_without_pid - hc_len + i);
        }
        if handle_offset.is_empty() {
            handle_offset.push(num_cols_without_pid.saturating_sub(1));
        }
        Ok(handle_offset)
    }

    // go: indexWorker.extractLookUpPushDownRowsOrHandles L2 (drop guard records indexScanBasicStats; Option<SelectResultRow> replaces IsEmpty; M-DEFER, M-ROW)
    async fn extract_look_up_push_down_rows_or_handles(
        &mut self,
        kv_ctx: &tidb_kv::Context,
        iter: &mut dyn SelectResultIter,
        handle_offset: &[usize],
    ) -> Result<(Vec<RowHandle>, Vec<Handle>, bool), Error> {
        debug_assert!(
            self.check_index_value.is_none(),
            "CheckIndex or CheckTable should not use index lookup push down"
        );
        const CHANNEL_IDX_INDEX: isize = 0;
        const CHANNEL_IDX_ROW: isize = 1;

        let start_time = Instant::now();
        let start_scan_keys = self.scanned_keys;

        let res = self
            .extract_look_up_push_down_rows_or_handles_loop(
                kv_ctx,
                iter,
                handle_offset,
                CHANNEL_IDX_INDEX,
                CHANNEL_IDX_ROW,
            )
            .await;

        // Defer equivalent: always record scanned keys into indexScanBasicStats (spec §10.5).
        if let Some(stats) = &self.shared.stats {
            let cnt = self.scanned_keys.saturating_sub(start_scan_keys) as i64;
            stats
                .get_index_scan_basic_stats()
                .record(start_time.elapsed().as_nanos() as i64, cnt);
        }
        res
    }

    // go-part: indexWorker.extractLookUpPushDownRowsOrHandles
    async fn extract_look_up_push_down_rows_or_handles_loop(
        &mut self,
        kv_ctx: &tidb_kv::Context,
        iter: &mut dyn SelectResultIter,
        handle_offset: &[usize],
        channel_idx_index: isize,
        channel_idx_row: isize,
    ) -> Result<(Vec<RowHandle>, Vec<Handle>, bool), Error> {
        let mut rows: Vec<RowHandle> = Vec::new();
        let mut handles: Vec<Handle> = Vec::new();
        let batch_limit = self.batch_size.max(0) as usize;
        let check_limit = self.pushed_limit;
        let is_common = self.shared.is_common_handle();
        let mut stride = CheckpointStride::new(32);

        while handles.len() + rows.len() < batch_limit {
            if *self.finished_rx.borrow() {
                return Ok((rows, handles, true));
            }
            let mut fin = self.finished_rx.clone();
            let row_opt = tokio::select! {
                biased;
                _ = fin.changed() => return Ok((rows, handles, true)),
                r = iter.next(kv_ctx) => r?,
            };
            let Some(sel_row) = row_opt else {
                return Ok((rows, handles, true));
            };
            self.scanned_keys += 1;
            if let Some(limit) = check_limit {
                if self.scanned_keys <= limit.offset {
                    continue;
                }
                if self.scanned_keys > limit.offset.saturating_add(limit.count) {
                    return Ok((rows, handles, false));
                }
            }
            if sel_row.channel_index == channel_idx_row {
                rows.push(sel_row.row);
            } else if sel_row.channel_index == channel_idx_index {
                let h = self.shared.get_handle(
                    &sel_row.row.row(),
                    handle_offset,
                    is_common,
                    GetHandleType::FromIndex,
                )?;
                handles.push(h);
            } else {
                return Err(Error::plain(format!(
                    "unexpected channel index {}",
                    sel_row.channel_index
                )));
            }
            let _ = stride.checkpoint(&()).await;
            self.shared.base.killer.handle_sql_killer_signal()?;
        }

        self.batch_size = self.batch_size.saturating_mul(2).min(self.max_batch_size);
        Ok((rows, handles, false))
    }

    // go: indexWorker.extractTaskHandles L2 (CheckpointStride(32) in handle decode loop; select! on finished_rx, M-KILL, M-GOCTX)
    async fn extract_task_handles(
        &mut self,
        kv_ctx: &tidb_kv::Context,
        chk: &mut Chunk,
        idx_result: &mut dyn SelectResult,
        handle_offset: &[usize],
    ) -> Result<(Vec<Handle>, Option<Chunk>), Error> {
        let check_limit = if self.check_index_value.is_none() {
            self.pushed_limit
        } else {
            None
        };
        let batch_limit = self.batch_size.max(0) as usize;
        let is_common = self.shared.is_common_handle();
        let mut handles: Vec<Handle> = Vec::new();
        let mut ret_chk: Option<Chunk> = None;
        let mut stride = CheckpointStride::new(32);

        while handles.len() < batch_limit {
            let mut required_rows = batch_limit - handles.len();
            if let Some(limit) = check_limit {
                let end = limit.offset.saturating_add(limit.count);
                if end <= self.scanned_keys {
                    return Ok((handles, None));
                }
                let left_cnt = end - self.scanned_keys;
                if (required_rows as u64) > left_cnt {
                    required_rows = left_cnt as usize;
                }
            }
            chk.set_required_rows(required_rows as isize, self.max_chunk_size);
            let start_time = Instant::now();
            if *self.finished_rx.borrow() {
                return Ok((handles, ret_chk));
            }
            let mut fin = self.finished_rx.clone();
            tokio::select! {
                biased;
                _ = fin.changed() => return Ok((handles, ret_chk)),
                res = idx_result.next(kv_ctx, chk) => res?,
            }
            if let Some(stats) = &self.shared.stats {
                stats.get_index_scan_basic_stats().record(
                    start_time.elapsed().as_nanos() as i64,
                    chk.num_rows() as i64,
                );
            }
            if chk.num_rows() == 0 {
                return Ok((handles, ret_chk));
            }
            if handles.is_empty() {
                handles.reserve(chk.num_rows());
            }
            for i in 0..chk.num_rows() {
                self.scanned_keys += 1;
                if let Some(limit) = check_limit {
                    if self.scanned_keys <= limit.offset {
                        continue;
                    }
                    if self.scanned_keys > limit.offset.saturating_add(limit.count) {
                        return Ok((handles, None));
                    }
                }
                let h = self.shared.get_handle(
                    &chk.get_row(i),
                    handle_offset,
                    is_common,
                    GetHandleType::FromIndex,
                )?;
                handles.push(h);
                let _ = stride.checkpoint(&()).await;
                self.shared.base.killer.handle_sql_killer_signal()?;
            }
            if let Some(check) = &self.check_index_value {
                let target = ret_chk.get_or_insert_with(|| {
                    chunk::new_chunk_with_capacity(&check.idx_col_tps, batch_limit)
                });
                target.append(chk, 0, chk.num_rows());
            }
        }
        self.batch_size = self.batch_size.saturating_mul(2).min(self.max_batch_size);
        Ok((handles, ret_chk))
    }

    // go: indexWorker.buildCompletedTask L2 (returns pre-resolved oneshot with RowHandle vec, M-CH, M-ROW)
    fn build_completed_task(rows: Vec<RowHandle>) -> ResultTaskHandle {
        let (done_tx, done_rx) = oneshot::channel();
        let _ = done_tx.send(Ok(LookupTableTaskResult {
            rows,
            row_idx: Vec::new(),
            cursor: 0,
            build_done_time: None,
            _alloc_guard: None,
        }));
        done_rx
    }

    // go: indexWorker.buildTableTask L2 (returns paired LookupTableWorkTask + oneshot::Receiver, M-CH)
    fn build_table_task(
        &self,
        handles: Vec<Handle>,
        ret_chk: Option<Chunk>,
    ) -> (LookupTableWorkTask, ResultTaskHandle) {
        let mut index_order: Option<HandleMap<usize>> = None;
        let mut duplicated_index_order: Option<HandleMap<usize>> = None;

        if self.keep_order {
            let mut map = new_handle_map();
            for (i, h) in handles.iter().enumerate() {
                map.set(h, i);
            }
            index_order = Some(map);
        }

        if self.check_index_value.is_some() {
            let mut map = new_handle_map();
            let mut dup = new_handle_map();
            for (i, h) in handles.iter().enumerate() {
                if map.get(h).is_some() {
                    dup.set(h, i);
                } else {
                    map.set(h, i);
                }
            }
            index_order = Some(map);
            duplicated_index_order = Some(dup);
        }

        let (done_tx, done_rx) = oneshot::channel();
        let work_task = LookupTableWorkTask {
            handles,
            index_order,
            duplicated_index_order,
            idx_rows: ret_chk,
            partition_table: None,
            done_tx,
        };
        (work_task, done_rx)
    }
}

// ============================================================================
// tableWorker & consistency check
// ============================================================================

struct TableWorker {
    shared: Arc<IndexLookUpShared>,
    work_rx: Arc<AsyncMutex<mpsc::Receiver<LookupTableWorkTask>>>,
    finished_rx: watch::Receiver<bool>,
    keep_order: bool,
    handle_idx: Vec<usize>,
    mem_tracker: Arc<Tracker>,
    check_index_value: Option<CheckIndexValue>,
}

impl TableWorker {
    // go: tableWorker.pickAndExecTask L2 (select! work_rx / finished_rx; catch_unwind sends Err on oneshot without nil deref, M-CH, M-PANIC)
    async fn pick_and_exec_task(&mut self, exec_ctx: &ExecCtx) {
        loop {
            if *self.finished_rx.borrow() {
                return;
            }
            let mut fin = self.finished_rx.clone();
            let task = tokio::select! {
                biased;
                _ = fin.changed() => return,
                maybe_task = async {
                    let mut rx = self.work_rx.lock().await;
                    rx.recv().await
                } => {
                    match maybe_task {
                        Some(t) => t,
                        None => return,
                    }
                }
            };

            let LookupTableWorkTask {
                handles,
                index_order,
                duplicated_index_order,
                idx_rows,
                partition_table,
                done_tx,
            } = task;

            let start_time = Instant::now();
            let exec_res = tidb_base::panic_guard::catch_unwind_future(self.execute_task(
                exec_ctx,
                handles,
                index_order,
                duplicated_index_order,
                idx_rows,
                partition_table,
            ))
            .await;

            match exec_res {
                Ok(res) => {
                    if let Some(stats) = &self.shared.stats {
                        stats
                            .table_row_scan
                            .fetch_add(start_time.elapsed().as_nanos() as i64, Ordering::Relaxed);
                        stats.table_task_num.fetch_add(1, Ordering::Relaxed);
                    }
                    let _ = done_tx.send(res);
                }
                Err((payload, bt)) => {
                    tidb_base::log_error!(
                        "tableWorker in IndexLookUpExecutor panicked",
                        "recover" => tidb_base::panic_guard::panic_payload_log_value(payload.as_ref()),
                        "stack" => bt.as_deref().unwrap_or(""),
                    );
                    let err = get_recover_error(payload);
                    let _ = done_tx.send(Err(err));
                    return;
                }
            }
        }
    }

    // go: tableWorker.compareData L2 (boundary stub for ADMIN CHECK, blocked-by:D06.4)
    async fn compare_data(
        &self,
        _exec_ctx: &ExecCtx,
        _table_reader: &mut TableReaderExecutor,
    ) -> Result<LookupTableTaskResult, Error> {
        // glue: blocked-by:D06.4 (ADMIN CHECK tables.CompareIndexAndVal in IndexLookUp compareData)
        Err(blocked(
            "D06.4",
            "tables.CompareIndexAndVal in IndexLookUp compareData",
        ))
    }

    // go: tableWorker.executeTask L2 (A1 TrackedAlloc + checkpoint; M-ROW RowHandle; CheckpointStride(32) in rowIdx/consistency loops; select! on finished_rx; no trace region, M-MEM, M-ROW, M-KILL, M-DEFER)
    async fn execute_task(
        &self,
        exec_ctx: &ExecCtx,
        handles: Vec<Handle>,
        index_order: Option<HandleMap<usize>>,
        _duplicated_index_order: Option<HandleMap<usize>>,
        _idx_rows: Option<Chunk>,
        partition_table: Option<Arc<dyn PhysicalTable>>,
    ) -> Result<LookupTableTaskResult, Error> {
        let kv_ctx = kv_ctx_from_dctx(&self.shared.ctx.table_ctx.dctx);
        let handle_cnt = handles.len();
        let handles_for_consistency =
            if !self.shared.ctx.weak_consistency && self.shared.tbl_plans.len() == 1 {
                handles.clone()
            } else {
                Vec::new()
            };

        let mut fin = self.finished_rx.clone();
        let build_res = tokio::select! {
            biased;
            _ = fin.changed() => {
                return Ok(LookupTableTaskResult {
                    rows: Vec::new(),
                    row_idx: Vec::new(),
                    cursor: 0,
                    build_done_time: Some(Instant::now()),
                    _alloc_guard: None,
                });
            }
            r = self.shared.build_table_reader(&kv_ctx, handles, partition_table, *self.finished_rx.borrow()) => r,
        };
        let build_done_time = Some(Instant::now());
        let mut table_reader = match build_res {
            Ok(tr) => tr,
            Err(err) => {
                if !*self.finished_rx.borrow() {
                    tidb_base::log_error!("build table reader failed", "error" => &err);
                }
                return Err(err);
            }
        };

        // Always close table_reader on every exit path without masking the main result (spec §10.5).
        let res = self
            .execute_task_with_reader(
                exec_ctx,
                &mut table_reader,
                handle_cnt,
                index_order,
                handles_for_consistency,
                build_done_time,
            )
            .await;
        if let Err(close_err) = exec::close(&mut table_reader).await {
            tidb_base::log_error!(
                "close table reader in IndexLookUpExecutor failed",
                "error" => &close_err,
            );
        }
        res
    }

    // go-part: tableWorker.executeTask
    async fn execute_task_with_reader(
        &self,
        exec_ctx: &ExecCtx,
        table_reader: &mut TableReaderExecutor,
        handle_cnt: usize,
        index_order: Option<HandleMap<usize>>,
        handles_for_consistency: Vec<Handle>,
        build_done_time: Option<Instant>,
    ) -> Result<LookupTableTaskResult, Error> {
        if self.check_index_value.is_some() {
            return self.compare_data(exec_ctx, table_reader).await;
        }

        // A1 memory tracking: TrackedAlloc tracks table chunks and flushes automatically on drop.
        let alloc = TrackedAlloc::new(Arc::clone(&self.mem_tracker));
        alloc.flush();
        self.mem_tracker.checkpoint()?;

        let ret_types = exec::ret_types(table_reader).to_vec();
        let init_cap = table_reader.base.alloc.init_cap().max(0) as usize;
        let max_chunk_size = table_reader.base.alloc.max_chunk_size().max(0) as usize;

        let mut rows: Vec<RowHandle> = Vec::with_capacity(handle_cnt);
        let mut cancelled = false;
        loop {
            if *self.finished_rx.borrow() {
                cancelled = true;
                break;
            }
            let mut chk = chunk::new_in(
                &ret_types,
                init_cap,
                max_chunk_size,
                ColAlloc::Tracked(alloc.clone()),
            );
            let mut fin = self.finished_rx.clone();
            let next_res = tokio::select! {
                biased;
                _ = fin.changed() => {
                    cancelled = true;
                    break;
                }
                r = exec::next(exec_ctx, table_reader, &mut chk) => r,
            };
            if let Err(err) = next_res {
                if !*self.finished_rx.borrow() {
                    tidb_base::log_warn!("table reader fetch next chunk failed", "error" => &err);
                }
                return Err(err);
            }
            let n = chk.num_rows();
            if n == 0 {
                break;
            }
            alloc.flush();
            self.mem_tracker.checkpoint()?;
            let arc_chk = Arc::new(chk);
            for idx in 0..n {
                rows.push(RowHandle::new(Arc::clone(&arc_chk), idx));
            }
        }

        alloc.flush();
        self.mem_tracker.checkpoint()?;

        let is_common = self.shared.is_common_handle();
        let mut stride = CheckpointStride::new(32);

        let mut task_res = LookupTableTaskResult {
            rows,
            row_idx: Vec::new(),
            cursor: 0,
            build_done_time,
            _alloc_guard: Some(alloc),
        };

        if self.keep_order {
            let Some(order_map) = index_order.as_ref() else {
                return Err(Error::plain(
                    "IndexLookUpExecutor: keepOrder is true but indexOrder is None",
                ));
            };
            task_res.row_idx = Vec::with_capacity(task_res.rows.len());
            for rh in &task_res.rows {
                let handle = self.shared.get_handle(
                    &rh.row(),
                    &self.handle_idx,
                    is_common,
                    GetHandleType::FromTable,
                )?;
                let Some(&pos) = order_map.get(&handle) else {
                    return Err(Error::plain(format!(
                        "IndexLookUpExecutor: handle {handle:?} missing from indexOrder"
                    )));
                };
                task_res.row_idx.push(pos);
                let _ = stride.checkpoint(&()).await;
                self.shared.base.killer.handle_sql_killer_signal()?;
            }
            self.mem_tracker.checkpoint()?;
            task_res.sort_by_index_order();
        }

        if handle_cnt != task_res.rows.len()
            && !cancelled
            && !*self.finished_rx.borrow()
            && !self.shared.ctx.weak_consistency
            && self.shared.tbl_plans.len() == 1
        {
            let mut obtained_handles_map: HandleMap<bool> = new_handle_map();
            for rh in &task_res.rows {
                let handle = self.shared.get_handle(
                    &rh.row(),
                    &self.handle_idx,
                    is_common,
                    GetHandleType::FromTable,
                )?;
                obtained_handles_map.set(&handle, true);
                let _ = stride.checkpoint(&()).await;
                self.shared.base.killer.handle_sql_killer_signal()?;
            }
            let miss_hds = get_lack_handles(&handles_for_consistency, &mut obtained_handles_map);
            let tbl_meta = self
                .shared
                .table
                .as_ref()
                .expect("IndexLookUpExecutor.table must be set")
                .meta();
            return Err(Reporter {
                tbl: tbl_meta,
                idx: &self.shared.index,
                enable_redact_log: self.shared.ctx.enable_redact_log.clone(),
            }
            .report_lookup_inconsistent(
                handle_cnt as i64,
                task_res.rows.len() as i64,
                &miss_hds,
                &handles_for_consistency,
                0,
            ));
        }

        Ok(task_res)
    }
}

/// getDatumRow extracts a row's datums according to `fields`.
// go: getDatumRow L1
pub fn get_datum_row(r: &Row<'_>, fields: &[Arc<FieldType>]) -> Vec<Datum> {
    let num_cols = r.len();
    let mut datum_row = Vec::with_capacity(num_cols.min(fields.len()));
    for (col_idx, ft) in fields.iter().enumerate().take(num_cols) {
        datum_row.push(r.get_datum(col_idx, ft));
    }
    datum_row
}

/// GetLackHandles gets the handles in `expected_handles` that are not in `obtained_handles_map`.
// go: GetLackHandles L1
pub fn get_lack_handles<V>(
    expected_handles: &[Handle],
    obtained_handles_map: &mut HandleMap<V>,
) -> Vec<Handle> {
    let diff_cnt = expected_handles
        .len()
        .saturating_sub(obtained_handles_map.len().max(0) as usize);
    let mut diff_handles = Vec::with_capacity(diff_cnt);
    let mut cnt = 0usize;
    for handle in expected_handles {
        let mut is_exist = false;
        if obtained_handles_map.get(handle).is_some() {
            obtained_handles_map.delete(handle);
            is_exist = true;
        }
        if !is_exist {
            diff_handles.push(handle.clone());
            cnt += 1;
            if cnt == diff_cnt {
                break;
            }
        }
    }
    diff_handles
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

    // ========================================================================
    // IndexLookUpExecutor (D12.1 Part 2C) unit tests
    // ========================================================================

    #[test]
    fn test_get_lack_handles_go_port() {
        // Port of Go `TestGetLackHandles` (`pkg/executor/distsql_test.go:136`).
        let expected: Vec<Handle> = (1..=10).map(|i| Handle::Int(IntHandle(i))).collect();
        let mut hm = new_handle_map();
        for h in &expected {
            hm.set(h, true);
        }
        let diff = get_lack_handles(&expected, &mut hm);
        assert!(diff.is_empty());
        assert_eq!(hm.len(), 0);

        let mut hm2 = new_handle_map();
        for i in [1, 5, 10] {
            hm2.set(&Handle::Int(IntHandle(i)), true);
        }
        let diff2: Vec<i64> = get_lack_handles(&expected, &mut hm2)
            .iter()
            .map(|h| h.int_value())
            .collect();
        assert_eq!(diff2, vec![2, 3, 4, 6, 7, 8, 9]);
    }

    #[test]
    fn test_index_look_up_stats_go_port_and_edge_cases() {
        // Port of Go `TestIndexLookUpStats` (`pkg/executor/distsql_test.go:250`).
        let sec: i64 = 1_000_000_000;
        let mut stats = IndexLookUpRunTimeStats::new(1);
        stats.fetch_handle_total.store(5 * sec, Ordering::Relaxed);
        stats.fetch_handle.store(2 * sec, Ordering::Relaxed);
        stats.task_wait.store(2 * sec, Ordering::Relaxed);
        stats.table_row_scan.store(2 * sec, Ordering::Relaxed);
        stats.table_task_num.store(2, Ordering::Relaxed);
        stats.next_wait_index_scan.store(sec, Ordering::Relaxed);
        stats
            .next_wait_table_lookup_build
            .store(2 * sec, Ordering::Relaxed);
        stats
            .next_wait_table_lookup_resp
            .store(3 * sec, Ordering::Relaxed);
        assert_eq!(
            stats.string(),
            "index_task: {total_time: 5s, fetch_handle: 2s, build: 1s, wait: 2s}, table_task: {total_time: 2s, num: 2, concurrency: 1}, next: {wait_index: 1s, wait_table_lookup_build: 2s, wait_table_lookup_resp: 3s}"
        );
        let cloned = stats.clone_stats();
        assert_eq!(cloned.string(), stats.string());
        assert_eq!(stats.tp(), TpIndexLookUpRunTimeStats);
        stats.merge(cloned.as_ref());
        assert_eq!(
            stats.string(),
            "index_task: {total_time: 10s, fetch_handle: 4s, build: 2s, wait: 4s}, table_task: {total_time: 4s, num: 4, concurrency: 1}, next: {wait_index: 2s, wait_table_lookup_build: 4s, wait_table_lookup_resp: 6s}"
        );

        // Spec §14 item 5 Go bug parity: when both index_scan and table_scan are 0, `next:` is omitted.
        let empty_front = IndexLookUpRunTimeStats::new(5);
        empty_front
            .next_wait_index_scan
            .store(1_000_000, Ordering::Relaxed);
        assert_eq!(empty_front.string(), "");
    }

    #[test]
    fn test_concurrency_and_batch_size_boundaries() {
        assert_eq!(get_index_scan_max_in_flight(0), 1);
        assert_eq!(get_index_scan_max_in_flight(-2), 1);
        assert_eq!(get_index_scan_max_in_flight(5), 10);

        // Spec §12.2: c = 1 must NOT panic!
        assert_eq!(get_merge_sort_index_scan_concurrency(false, 4, 5), 0);
        assert_eq!(get_merge_sort_index_scan_concurrency(true, 0, 5), 0);
        assert_eq!(get_merge_sort_index_scan_concurrency(true, 10, 1), 1);
        assert_eq!(get_merge_sort_index_scan_concurrency(true, 2, 1), 1);
        assert_eq!(get_merge_sort_index_scan_concurrency(true, 10, 5), 2);
        assert_eq!(get_merge_sort_index_scan_concurrency(true, 2, 5), 5);
        assert!(get_merge_sort_shared_copr_request_rate_limit(false, 5).is_none());
        assert!(get_merge_sort_shared_copr_request_rate_limit(true, 1).is_some());

        assert_eq!(calculate_batch_size(50, 1, 100), 64);
        assert_eq!(calculate_batch_size(100, 1, 100), 100);
        assert_eq!(calculate_batch_size(200, 1, 100), 100);
        assert_eq!(calculate_batch_size(10, 32, 100), 32);
        assert_eq!(
            calculate_batch_size(isize::MAX - 1, 1, isize::MAX),
            isize::MAX
        );
    }

    struct PanicSelectResult {
        closed: Arc<AtomicBool>,
    }

    #[async_trait]
    impl SelectResult for PanicSelectResult {
        async fn next_raw(&mut self, _ctx: &tidb_kv::Context) -> Result<Option<Vec<u8>>, Error> {
            Ok(None)
        }
        async fn next(&mut self, _ctx: &tidb_kv::Context, _chk: &mut Chunk) -> Result<(), Error> {
            panic!("injected worker panic");
        }
        fn into_iter(
            &mut self,
            _: Vec<Vec<Arc<FieldType>>>,
        ) -> Result<Box<dyn SelectResultIter>, Error> {
            Err(Error::plain("not implemented"))
        }
        fn close(&mut self) -> Result<(), Error> {
            self.closed.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    struct MockPushDownIter {
        items: Vec<(isize, Vec<i64>)>,
        pos: usize,
        row_fts: Vec<Arc<FieldType>>,
        idx_fts: Vec<Arc<FieldType>>,
        closed: Arc<AtomicBool>,
    }

    #[async_trait]
    impl SelectResultIter for MockPushDownIter {
        async fn next(
            &mut self,
            _ctx: &tidb_kv::Context,
        ) -> Result<Option<tidb_store::distsql::SelectResultRow>, Error> {
            if self.pos >= self.items.len() {
                return Ok(None);
            }
            let (ch_idx, vals) = &self.items[self.pos];
            self.pos += 1;
            let fts = if *ch_idx == 1 {
                &self.row_fts
            } else {
                &self.idx_fts
            };
            let mut chk = chunk::new_chunk_with_capacity(fts, 1);
            for (c, &v) in vals.iter().enumerate() {
                chk.append_int64(c, v);
            }
            Ok(Some(tidb_store::distsql::SelectResultRow {
                channel_index: *ch_idx,
                row: RowHandle::new(Arc::new(chk), 0),
            }))
        }
        fn close(&mut self) -> Result<(), Error> {
            self.closed.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    struct MockPushDownSelectResult {
        items: Vec<(isize, Vec<i64>)>,
        row_fts: Vec<Arc<FieldType>>,
        idx_fts: Vec<Arc<FieldType>>,
        closed: Arc<AtomicBool>,
    }

    #[async_trait]
    impl SelectResult for MockPushDownSelectResult {
        async fn next_raw(&mut self, _ctx: &tidb_kv::Context) -> Result<Option<Vec<u8>>, Error> {
            Ok(None)
        }
        async fn next(&mut self, _ctx: &tidb_kv::Context, _chk: &mut Chunk) -> Result<(), Error> {
            Err(Error::plain("expected into_iter"))
        }
        fn into_iter(
            &mut self,
            _col_types: Vec<Vec<Arc<FieldType>>>,
        ) -> Result<Box<dyn SelectResultIter>, Error> {
            Ok(Box::new(MockPushDownIter {
                items: std::mem::take(&mut self.items),
                pos: 0,
                row_fts: self.row_fts.clone(),
                idx_fts: self.idx_fts.clone(),
                closed: Arc::clone(&self.closed),
            }))
        }
        fn close(&mut self) -> Result<(), Error> {
            self.closed.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    fn make_lookup_table_and_plan(
        keep_order: bool,
        push_down: bool,
        extra_tbl_plan: bool,
    ) -> (
        Arc<TableInfo>,
        tidb_planner::core::physical_plans::PhysicalIndexLookUpReader,
    ) {
        use tidb_planner::core::physical_plans::{
            PhysicalIndexLookUpReader, PhysicalLocalIndexLookUp, PhysicalTableScan,
        };

        let mut pk_ft = int_ft();
        pk_ft.add_flag(PriKeyFlag | NotNullFlag);
        let c0 = Arc::new(ColumnInfo {
            id: 1,
            name: CIStr::new("id"),
            offset: 0,
            field_type: pk_ft.clone(),
            state: StatePublic,
            ..Default::default()
        });
        let c1 = Arc::new(ColumnInfo {
            id: 2,
            name: CIStr::new("k"),
            offset: 1,
            field_type: int_ft(),
            state: StatePublic,
            ..Default::default()
        });
        let c2 = Arc::new(ColumnInfo {
            id: 3,
            name: CIStr::new("v"),
            offset: 2,
            field_type: int_ft(),
            state: StatePublic,
            ..Default::default()
        });
        let idx = Arc::new(IndexInfo {
            id: 1,
            name: CIStr::new("idx_k"),
            table: CIStr::new("t"),
            state: StatePublic,
            columns: Some(vec![IndexColumn {
                name: CIStr::new("k"),
                offset: 1,
                length: UnspecifiedLength,
                use_changing_type: false,
            }]),
            ..Default::default()
        });
        let tbl = Arc::new(TableInfo {
            id: 700,
            name: CIStr::new("t"),
            columns: Some(vec![Arc::clone(&c0), Arc::clone(&c1), Arc::clone(&c2)]),
            indices: Some(vec![Arc::clone(&idx)]),
            pk_is_handle: true,
            state: StatePublic,
            ..Default::default()
        });

        let idx_col_k = Arc::new(Column {
            id: 2,
            unique_id: 2,
            index: 0,
            ret_type: Arc::new(int_ft()),
            ..Default::default()
        });
        let idx_col_id = Arc::new(Column {
            id: 1,
            unique_id: 1,
            index: 1,
            ret_type: Arc::new(pk_ft.clone()),
            ..Default::default()
        });
        let tbl_col_id = Arc::new(Column {
            id: 1,
            unique_id: 1,
            index: 0,
            ret_type: Arc::new(pk_ft),
            ..Default::default()
        });
        let tbl_col_v = Arc::new(Column {
            id: 3,
            unique_id: 3,
            index: 1,
            ret_type: Arc::new(int_ft()),
            ..Default::default()
        });

        let idx_schema = new_schema(vec![Arc::clone(&idx_col_k), Arc::clone(&idx_col_id)]);
        let tbl_schema = new_schema(vec![Arc::clone(&tbl_col_id), Arc::clone(&tbl_col_v)]);
        let stats = Arc::new(StatsInfo {
            row_count: 20.0,
            ..Default::default()
        });

        let mut idx_scan = PhysicalIndexScan {
            table: Arc::clone(&tbl),
            index: Arc::clone(&idx),
            columns: vec![Arc::clone(&c1), Arc::clone(&c0)],
            ranges: vec![int_range(1, 100)],
            keep_order,
            ..Default::default()
        };
        idx_scan.set_id(2);
        idx_scan.sp.set_schema(Some(idx_schema));
        idx_scan.set_stats(Some(Arc::clone(&stats)));
        let idx_scan_ref: PhysicalPlanRef = Arc::new(idx_scan);

        let mut tbl_scan = PhysicalTableScan {
            table: Arc::clone(&tbl),
            columns: vec![Arc::clone(&c0), Arc::clone(&c2)],
            store_type: tidb_base::kv::TiKV,
            ..Default::default()
        };
        tbl_scan.set_id(3);
        tbl_scan.sp.set_schema(Some(tbl_schema.clone()));
        tbl_scan.set_stats(Some(Arc::clone(&stats)));
        let tbl_scan_ref: PhysicalPlanRef = Arc::new(tbl_scan);

        let mut table_plans: Vec<PhysicalPlanRef> = vec![Arc::clone(&tbl_scan_ref)];
        if extra_tbl_plan {
            let mut lim = PhysicalLimit {
                offset: 0,
                count: 1000,
                ..Default::default()
            };
            lim.set_id(4);
            lim.sp.set_schema(Some(tbl_schema.clone()));
            lim.set_stats(Some(Arc::clone(&stats)));
            lim.sp.base.children = vec![Arc::clone(&tbl_scan_ref)];
            table_plans.push(Arc::new(lim));
        }

        let (index_plans, un_nature) = if push_down {
            let mut local = PhysicalLocalIndexLookUp {
                index_handle_offsets: vec![1],
                ..Default::default()
            };
            local.set_id(5);
            local.sp.set_schema(Some(tbl_schema.clone()));
            local.sp.base.children = vec![Arc::clone(&idx_scan_ref), Arc::clone(&tbl_scan_ref)];
            let mut map = HashMap::new();
            map.insert(0, 2);
            map.insert(1, 2);
            (
                vec![
                    Arc::clone(&idx_scan_ref),
                    Arc::clone(&tbl_scan_ref),
                    Arc::new(local) as PhysicalPlanRef,
                ],
                map,
            )
        } else {
            (vec![Arc::clone(&idx_scan_ref)], HashMap::new())
        };

        let mut reader = PhysicalIndexLookUpReader {
            index_plan: Some(Arc::clone(index_plans.last().unwrap())),
            table_plan: Some(Arc::clone(table_plans.last().unwrap())),
            index_plans,
            index_plans_un_nature_orders: un_nature,
            table_plans,
            extra_handle_col: Some(Arc::clone(&tbl_col_id)),
            keep_order,
            index_look_up_push_down: push_down,
            ..Default::default()
        };
        reader.set_id(1);
        reader.sp.set_schema(Some(tbl_schema));
        reader.set_stats(Some(stats));
        (tbl, reader)
    }

    #[tokio::test]
    async fn index_lookup_inconsistent_8133_and_exemptions() {
        // Port of Go `TestInconsistentIndex` core logic (8133 error + weakConsistency + multi-tblPlan exemptions).
        for (weak, extra_plan, expect_err) in [
            (false, false, true),
            (true, false, false),
            (false, true, false),
        ] {
            let (tbl, plan) = make_lookup_table_and_plan(true, false, extra_plan);
            let is = make_info_schema(tbl).await;
            let mut s = MockSession::new(100);
            s.vars.stmt_ctx_mut().weak_consistency = weak;
            s.vars.index_lookup_size = 10;
            let mut b = new_executor_builder(&s, Some(is), Arc::new(TelemetryInfo::default()));
            let mut ilu = b.build_index_look_up_reader(&mut s, &plan).await.unwrap();

            let idx_closed = Arc::new(AtomicBool::new(false));
            let tbl_closed = Arc::new(AtomicBool::new(false));
            let idx_c = Arc::clone(&idx_closed);
            ilu.select_result_hook.select_result_func = Some(Arc::new(move |_, _, _, _, _| {
                let c = Arc::clone(&idx_c);
                Box::pin(async move {
                    Ok(Box::new(TrackedMockSelectResult {
                        rows: vec![vec![1], vec![2], vec![3]],
                        pos: 0,
                        closed: c,
                        next_err: None,
                        close_err: None,
                    }) as Box<dyn SelectResult>)
                })
            }));
            let tbl_c = Arc::clone(&tbl_closed);
            ilu.data_reader_builder
                .select_result_hook
                .select_result_func = Some(Arc::new(move |_, _, _, _, _| {
                let c = Arc::clone(&tbl_c);
                Box::pin(async move {
                    // Only return 2 of the 3 handles (missing handle 2).
                    Ok(Box::new(TrackedMockSelectResult {
                        rows: vec![vec![1, 10], vec![3, 30]],
                        pos: 0,
                        closed: c,
                        next_err: None,
                        close_err: None,
                    }) as Box<dyn SelectResult>)
                })
            }));

            let ctx = ExecCtx::new(s.vars.stmt_ctx_arc());
            exec::open(&ctx, &mut ilu).await.unwrap();
            let mut chk = exec::new_first_chunk(&ilu);
            let res = exec::next(&ctx, &mut ilu, &mut chk).await;
            if expect_err {
                let err = res.expect_err("expected 8133 inconsistency error");
                assert_eq!(
                    err.to_string(),
                    "[executor:8133]data inconsistency in table: t, index: idx_k, index-count:3 != record-count:2"
                );
            } else {
                res.unwrap();
                assert_eq!(chk.num_rows(), 2);
            }
            exec::close(&mut ilu).await.unwrap();
            assert!(idx_closed.load(Ordering::SeqCst));
            assert!(tbl_closed.load(Ordering::SeqCst));
            assert_eq!(ilu.mem_tracker.as_ref().unwrap().bytes_consumed(), 0);
        }
    }

    #[tokio::test]
    async fn index_lookup_pushdown_dual_channel_and_request_flags() {
        let (tbl, plan) = make_lookup_table_and_plan(false, true, false);
        let is = make_info_schema(tbl).await;
        let mut s = MockSession::new(100);
        s.vars.enable_paging = true;
        s.vars.index_lookup_size = 10;
        let mut b = new_executor_builder(&s, Some(is), Arc::new(TelemetryInfo::default()));
        let mut ilu = b.build_index_look_up_reader(&mut s, &plan).await.unwrap();

        let saw_paging = Arc::new(AtomicBool::new(true));
        let saw_cacheable = Arc::new(AtomicBool::new(true));
        let idx_closed = Arc::new(AtomicBool::new(false));
        let tbl_closed = Arc::new(AtomicBool::new(false));

        let sp = Arc::clone(&saw_paging);
        let sc = Arc::clone(&saw_cacheable);
        let ic = Arc::clone(&idx_closed);
        ilu.select_result_hook.select_result_func = Some(Arc::new(move |_, _, req, _, _| {
            sp.store(req.paging.enable, Ordering::SeqCst);
            sc.store(req.cacheable, Ordering::SeqCst);
            let c = Arc::clone(&ic);
            Box::pin(async move {
                Ok(Box::new(MockPushDownSelectResult {
                    items: vec![
                        (1, vec![1, 100]), // channelIdxRow (hit)
                        (0, vec![2]),      // channelIdxIndex (miss -> tableWorker)
                        (1, vec![3, 300]), // channelIdxRow (hit)
                    ],
                    row_fts: vec![Arc::new(int_ft()), Arc::new(int_ft())],
                    idx_fts: vec![Arc::new(int_ft())],
                    closed: c,
                }) as Box<dyn SelectResult>)
            })
        }));
        let tc = Arc::clone(&tbl_closed);
        ilu.data_reader_builder
            .select_result_hook
            .select_result_func = Some(Arc::new(move |_, _, _, _, _| {
            let c = Arc::clone(&tc);
            Box::pin(async move {
                Ok(Box::new(TrackedMockSelectResult {
                    rows: vec![vec![2, 200]],
                    pos: 0,
                    closed: c,
                    next_err: None,
                    close_err: None,
                }) as Box<dyn SelectResult>)
            })
        }));

        let ctx = ExecCtx::new(s.vars.stmt_ctx_arc());
        exec::open(&ctx, &mut ilu).await.unwrap();
        let mut chk = exec::new_first_chunk(&ilu);
        let mut got = Vec::new();
        loop {
            exec::next(&ctx, &mut ilu, &mut chk).await.unwrap();
            if chk.num_rows() == 0 {
                break;
            }
            for i in 0..chk.num_rows() {
                got.push((chk.get_row(i).get_int64(0), chk.get_row(i).get_int64(1)));
            }
        }
        exec::close(&mut ilu).await.unwrap();
        got.sort();
        assert_eq!(got, vec![(1, 100), (2, 200), (3, 300)]);
        assert!(!saw_paging.load(Ordering::SeqCst));
        assert!(!saw_cacheable.load(Ordering::SeqCst));
        assert!(idx_closed.load(Ordering::SeqCst));
        assert!(tbl_closed.load(Ordering::SeqCst));
        assert_eq!(ilu.mem_tracker.as_ref().unwrap().bytes_consumed(), 0);
    }

    #[tokio::test]
    async fn index_lookup_lifecycle_panic_oom_cancel_and_detach() {
        let sink = tidb_log::sink::MemorySink::new();
        let cfg = tidb_base::config::LogConfig {
            level: "info".to_owned(),
            format: "text".to_owned(),
            disable_timestamp: false,
            disable_error_verbose: true,
            ..tidb_base::config::LogConfig::default()
        };
        let log_sink = Arc::clone(&sink);
        tidb_log::init_logger_with_sinks(&cfg, "", log_sink, None).unwrap();
        tidb_log::set_panic_hook();

        // Normalizes the dynamic timestamp, caller line number, and captured backtrace in a
        // panic recovery log line while asserting single-line structure and non-empty Go-style
        // `\n\t` stack frames, so the entire line can be compared verbatim via `assert_eq!`.
        let normalize_panic_line = |raw: &str| -> String {
            assert!(
                !raw.contains('\n') && !raw.contains('\r'),
                "log line must be single-line: {raw:?}"
            );
            let after_ts = raw
                .strip_prefix('[')
                .and_then(|s| s.split_once("] "))
                .map(|(ts, rest)| {
                    assert_eq!(ts.len(), 30, "expected pingcap/log timestamp, got: {ts:?}");
                    rest
                })
                .expect("missing leading [timestamp] ");
            let (before_stack, stack_quoted) = after_ts
                .split_once(" [stack=\"")
                .expect("missing [stack=\"...\"] field");
            let stack_body = stack_quoted
                .strip_suffix("\"]")
                .expect("stack field must be the last field");
            assert!(
                !stack_body.is_empty() && stack_body.contains("\\n\\t"),
                "stack must contain Go-style \\n\\t frame locations: {stack_body:?}"
            );
            // Replace `[distsql.rs:<digits>]` with `[distsql.rs:<LINE>]`.
            let normalized_prefix = if let Some((head, tail)) =
                before_stack.split_once("[distsql.rs:")
                && let Some((line_no, rest)) = tail.split_once(']')
                && !line_no.is_empty()
                && line_no.bytes().all(|b| b.is_ascii_digit())
            {
                format!("{head}[distsql.rs:<LINE>]{rest}")
            } else {
                before_stack.to_owned()
            };
            format!("[<TS>] {normalized_prefix} [stack=\"<STACK>\"]")
        };

        // 1. Index worker panic -> converted to Err, tracker zeroed, mock closed, whole-line golden log.
        {
            let _ = sink.take();
            let (tbl, plan) = make_lookup_table_and_plan(true, false, false);
            let is = make_info_schema(tbl).await;
            let mut s = MockSession::new(100);
            let mut b = new_executor_builder(&s, Some(is), Arc::new(TelemetryInfo::default()));
            let mut ilu = b.build_index_look_up_reader(&mut s, &plan).await.unwrap();
            let closed = Arc::new(AtomicBool::new(false));
            let c = Arc::clone(&closed);
            ilu.select_result_hook.select_result_func = Some(Arc::new(move |_, _, _, _, _| {
                let c2 = Arc::clone(&c);
                Box::pin(async move {
                    Ok(Box::new(PanicSelectResult { closed: c2 }) as Box<dyn SelectResult>)
                })
            }));
            let ctx = ExecCtx::new(s.vars.stmt_ctx_arc());
            exec::open(&ctx, &mut ilu).await.unwrap();
            let mut chk = exec::new_first_chunk(&ilu);
            let err = exec::next(&ctx, &mut ilu, &mut chk).await.unwrap_err();
            assert!(err.to_string().contains("injected worker panic"), "{err}");
            exec::close(&mut ilu).await.unwrap();
            assert!(closed.load(Ordering::SeqCst));
            assert_eq!(ilu.mem_tracker.as_ref().unwrap().bytes_consumed(), 0);

            let logged = String::from_utf8(sink.take()).unwrap();
            let panic_lines: Vec<&str> =
                logged.lines().filter(|l| l.contains("panicked")).collect();
            assert_eq!(
                panic_lines.len(),
                1,
                "expected 1 indexWorker panic log:\n{logged}"
            );
            assert_eq!(
                normalize_panic_line(panic_lines[0]),
                "[<TS>] [WARN] [distsql.rs:<LINE>] [\"indexWorker in IndexLookupExecutor panicked\"] [recover=\"injected worker panic\"] [stack=\"<STACK>\"]"
            );
        }

        // 2. Table worker panic -> converted to Err, tracker zeroed, both mocks closed, whole-line golden log.
        {
            let _ = sink.take();
            let (tbl, plan) = make_lookup_table_and_plan(true, false, false);
            let is = make_info_schema(tbl).await;
            let mut s = MockSession::new(100);
            let mut b = new_executor_builder(&s, Some(is), Arc::new(TelemetryInfo::default()));
            let mut ilu = b.build_index_look_up_reader(&mut s, &plan).await.unwrap();
            let idx_closed = Arc::new(AtomicBool::new(false));
            let tbl_closed = Arc::new(AtomicBool::new(false));
            let ic = Arc::clone(&idx_closed);
            ilu.select_result_hook.select_result_func = Some(Arc::new(move |_, _, _, _, _| {
                let c = Arc::clone(&ic);
                Box::pin(async move {
                    Ok(Box::new(TrackedMockSelectResult {
                        rows: vec![vec![1]],
                        pos: 0,
                        closed: c,
                        next_err: None,
                        close_err: None,
                    }) as Box<dyn SelectResult>)
                })
            }));
            let tc = Arc::clone(&tbl_closed);
            ilu.data_reader_builder
                .select_result_hook
                .select_result_func = Some(Arc::new(move |_, _, _, _, _| {
                let c = Arc::clone(&tc);
                Box::pin(async move {
                    Ok(Box::new(PanicSelectResult { closed: c }) as Box<dyn SelectResult>)
                })
            }));
            let ctx = ExecCtx::new(s.vars.stmt_ctx_arc());
            exec::open(&ctx, &mut ilu).await.unwrap();
            let mut chk = exec::new_first_chunk(&ilu);
            let err = exec::next(&ctx, &mut ilu, &mut chk).await.unwrap_err();
            assert!(err.to_string().contains("injected worker panic"), "{err}");
            exec::close(&mut ilu).await.unwrap();
            assert!(idx_closed.load(Ordering::SeqCst));
            assert!(tbl_closed.load(Ordering::SeqCst));
            assert_eq!(ilu.mem_tracker.as_ref().unwrap().bytes_consumed(), 0);

            let logged = String::from_utf8(sink.take()).unwrap();
            let panic_lines: Vec<&str> =
                logged.lines().filter(|l| l.contains("panicked")).collect();
            assert_eq!(
                panic_lines.len(),
                1,
                "expected 1 tableWorker panic log:\n{logged}"
            );
            assert_eq!(
                normalize_panic_line(panic_lines[0]),
                "[<TS>] [ERROR] [distsql.rs:<LINE>] [\"tableWorker in IndexLookUpExecutor panicked\"] [recover=\"injected worker panic\"] [stack=\"<STACK>\"]"
            );
        }

        // 3. Memory quota exceeded in tableWorker -> A1 checkpoint returns Err, tracker zeroed after Close.
        {
            let (tbl, plan) = make_lookup_table_and_plan(true, false, false);
            let is = make_info_schema(tbl).await;
            let mut s = MockSession::new(100);
            let stmt_tracker = new_tracker(-1, 512); // enough for Open's key range, exceeded by tableWorker chunk allocation
            stmt_tracker.set_action_on_exceed(Some(tidb_memory::action::PanicOnExceed::new(
                Some(Arc::clone(&s.killer)),
                0,
            ) as tidb_memory::ActionRef));
            s.vars.stmt_ctx_mut().mem_tracker = Some(Arc::clone(&stmt_tracker));
            let mut b = new_executor_builder(&s, Some(is), Arc::new(TelemetryInfo::default()));
            let mut ilu = b.build_index_look_up_reader(&mut s, &plan).await.unwrap();
            ilu.select_result_hook.select_result_func = Some(Arc::new(|_, _, _, _, _| {
                Box::pin(async {
                    Ok(Box::new(TrackedMockSelectResult {
                        rows: vec![vec![1], vec![2]],
                        pos: 0,
                        closed: Arc::new(AtomicBool::new(false)),
                        next_err: None,
                        close_err: None,
                    }) as Box<dyn SelectResult>)
                })
            }));
            ilu.data_reader_builder
                .select_result_hook
                .select_result_func = Some(Arc::new(|_, _, _, _, _| {
                Box::pin(async {
                    Ok(Box::new(TrackedMockSelectResult {
                        rows: vec![vec![1, 10], vec![2, 20]],
                        pos: 0,
                        closed: Arc::new(AtomicBool::new(false)),
                        next_err: None,
                        close_err: None,
                    }) as Box<dyn SelectResult>)
                })
            }));
            let ctx = ExecCtx::new(s.vars.stmt_ctx_arc());
            exec::open(&ctx, &mut ilu).await.unwrap();
            let mut chk = exec::new_first_chunk(&ilu);
            let err = exec::next(&ctx, &mut ilu, &mut chk).await.unwrap_err();
            assert!(
                err.to_string().contains("Out Of Memory Quota") || err.to_string().contains("8175"),
                "unexpected OOM error: {err}"
            );
            exec::close(&mut ilu).await.unwrap();
            assert_eq!(ilu.mem_tracker.as_ref().unwrap().bytes_consumed(), 0);
        }

        // 4. Open -> Close without Next, early Close before exhaustion, Drop without Close, and Detach.
        {
            let (tbl, plan) = make_lookup_table_and_plan(true, false, false);
            let is = make_info_schema(tbl).await;
            let mut s = MockSession::new(100);
            let mut b = new_executor_builder(&s, Some(is), Arc::new(TelemetryInfo::default()));
            let mut ilu = b.build_index_look_up_reader(&mut s, &plan).await.unwrap();
            assert!(ilu.detach().is_some());

            let ctx = ExecCtx::new(s.vars.stmt_ctx_arc());
            // Open -> Close without Next.
            exec::open(&ctx, &mut ilu).await.unwrap();
            exec::close(&mut ilu).await.unwrap();

            // Early Close after 1 row, then Drop without Close on second Open.
            ilu.select_result_hook.select_result_func = Some(Arc::new(|_, _, _, _, _| {
                Box::pin(async {
                    let rows = (1..=32).map(|i| vec![i]).collect();
                    Ok(Box::new(TrackedMockSelectResult {
                        rows,
                        pos: 0,
                        closed: Arc::new(AtomicBool::new(false)),
                        next_err: None,
                        close_err: None,
                    }) as Box<dyn SelectResult>)
                })
            }));
            ilu.data_reader_builder
                .select_result_hook
                .select_result_func = Some(Arc::new(|_, _, _, _, _| {
                Box::pin(async {
                    let rows = (1..=32).map(|i| vec![i, i * 10]).collect();
                    Ok(Box::new(TrackedMockSelectResult {
                        rows,
                        pos: 0,
                        closed: Arc::new(AtomicBool::new(false)),
                        next_err: None,
                        close_err: None,
                    }) as Box<dyn SelectResult>)
                })
            }));
            exec::open(&ctx, &mut ilu).await.unwrap();
            let mut chk = exec::new_first_chunk(&ilu);
            chk.set_required_rows(1, 32);
            exec::next(&ctx, &mut ilu, &mut chk).await.unwrap();
            assert_eq!(chk.num_rows(), 1);
            exec::close(&mut ilu).await.unwrap();
            assert_eq!(ilu.mem_tracker.as_ref().unwrap().bytes_consumed(), 0);

            // Drop without Close.
            exec::open(&ctx, &mut ilu).await.unwrap();
            exec::next(&ctx, &mut ilu, &mut chk).await.unwrap();
            drop(ilu);
        }
    }

    async fn run_concurrent_lookup_smoke() {
        let (tbl, plan) = make_lookup_table_and_plan(true, false, false);
        let is = make_info_schema(tbl).await;
        let mut s = MockSession::new(100);
        s.vars.index_lookup_size = 4;
        s.vars.set_index_lookup_concurrency(4);
        let mut b = new_executor_builder(&s, Some(is), Arc::new(TelemetryInfo::default()));
        let mut ilu = b.build_index_look_up_reader(&mut s, &plan).await.unwrap();
        ilu.select_result_hook.select_result_func = Some(Arc::new(|_, _, _, _, _| {
            Box::pin(async {
                // Reverse handle order so keepOrder sorts every batch.
                let rows = (1..=30).rev().map(|i| vec![i]).collect();
                Ok(Box::new(TrackedMockSelectResult {
                    rows,
                    pos: 0,
                    closed: Arc::new(AtomicBool::new(false)),
                    next_err: None,
                    close_err: None,
                }) as Box<dyn SelectResult>)
            })
        }));
        ilu.data_reader_builder
            .select_result_hook
            .select_result_func = Some(Arc::new(|_, _, req, _, _| {
            let krs = req
                .key_ranges
                .as_ref()
                .map(|k| k.append_self_to(Vec::new()))
                .unwrap_or_default();
            Box::pin(async move {
                let mut rows = Vec::new();
                for kr in &krs {
                    if let Ok(h_start) = tidb_tablecodec::decode_row_key(&kr.start_key)
                        && let Ok(h_end) = tidb_tablecodec::decode_row_key(&kr.end_key)
                    {
                        for id in h_start.int_value()..h_end.int_value() {
                            rows.push(vec![id, id * 10]);
                        }
                    }
                }
                Ok(Box::new(TrackedMockSelectResult {
                    rows,
                    pos: 0,
                    closed: Arc::new(AtomicBool::new(false)),
                    next_err: None,
                    close_err: None,
                }) as Box<dyn SelectResult>)
            })
        }));
        let ctx = ExecCtx::new(s.vars.stmt_ctx_arc());
        exec::open(&ctx, &mut ilu).await.unwrap();
        let mut chk = exec::new_first_chunk(&ilu);
        let mut got = Vec::new();
        loop {
            exec::next(&ctx, &mut ilu, &mut chk).await.unwrap();
            if chk.num_rows() == 0 {
                break;
            }
            for i in 0..chk.num_rows() {
                got.push(chk.get_row(i).get_int64(0));
            }
        }
        exec::close(&mut ilu).await.unwrap();
        let want: Vec<i64> = (1..=30).rev().collect();
        assert_eq!(got, want);
        assert_eq!(ilu.mem_tracker.as_ref().unwrap().bytes_consumed(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn index_lookup_no_deadlock_single_thread_runtime() {
        run_concurrent_lookup_smoke().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn index_lookup_no_deadlock_multi_thread_runtime() {
        run_concurrent_lookup_smoke().await;
    }

    #[tokio::test]
    async fn build_index_look_up_pre_open_fallback_guards_keep_stmt_ctx_clean() {
        // 1. Static partition -> blocked-by:D06.4
        {
            let (tbl, mut plan) = make_lookup_table_and_plan(true, false, false);
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
            let ti = Arc::new(TelemetryInfo::default());
            let mut b = new_executor_builder(&s, Some(is), Arc::clone(&ti));
            assert!(b.build_index_look_up_reader(&mut s, &plan).await.is_none());
            assert_eq!(
                tidb_base::blocked::blocked_by(&b.err.unwrap()),
                Some("D06.4")
            );
            assert!(!ti.use_table_look_up.load(Ordering::SeqCst));
            assert!(!s.vars.stmt_ctx().is_tikv.load(Ordering::SeqCst));
            assert!(s.vars.stmt_ctx().index_names.is_empty());
            assert!(s.vars.stmt_ctx().table_ids.is_empty());
        }

        // 2. Dynamic partition pruning -> blocked-by:D09.12
        {
            let (tbl_orig, mut plan) = make_lookup_table_and_plan(true, false, false);
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
            let ti = Arc::new(TelemetryInfo::default());
            let mut b = new_executor_builder(&s, None, Arc::clone(&ti));
            assert!(b.build_index_look_up_reader(&mut s, &plan).await.is_none());
            assert_eq!(
                tidb_base::blocked::blocked_by(&b.err.unwrap()),
                Some("D09.12")
            );
            assert!(!ti.use_table_look_up.load(Ordering::SeqCst));
            assert!(s.vars.stmt_ctx().index_names.is_empty());
            assert!(s.vars.stmt_ctx().table_ids.is_empty());
        }

        // 3. Cached table -> blocked-by:D06.4
        {
            let (tbl_orig, mut plan) = make_lookup_table_and_plan(true, false, false);
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
            let mut s = MockSession::new(100);
            let ti = Arc::new(TelemetryInfo::default());
            let mut b = new_executor_builder(&s, None, Arc::clone(&ti));
            assert!(b.build_index_look_up_reader(&mut s, &plan).await.is_none());
            assert_eq!(
                tidb_base::blocked::blocked_by(&b.err.unwrap()),
                Some("D06.4")
            );
            assert!(!ti.use_table_look_up.load(Ordering::SeqCst));
            assert!(s.vars.stmt_ctx().index_names.is_empty());
            assert!(s.vars.stmt_ctx().table_ids.is_empty());
        }

        // 4. Correlated column in access condition -> blocked-by:D10.1
        {
            let (tbl, mut plan) = make_lookup_table_and_plan(true, false, false);
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
            let ti = Arc::new(TelemetryInfo::default());
            let mut b = new_executor_builder(&s, Some(is), Arc::clone(&ti));
            assert!(b.build_index_look_up_reader(&mut s, &plan).await.is_none());
            assert_eq!(
                tidb_base::blocked::blocked_by(&b.err.unwrap()),
                Some("D10.1")
            );
            assert!(!ti.use_table_look_up.load(Ordering::SeqCst));
            assert!(s.vars.stmt_ctx().index_names.is_empty());
            assert!(s.vars.stmt_ctx().table_ids.is_empty());
        }
    }

    #[tokio::test]
    async fn index_lookup_table_request_sorts_handles_and_pins_kv_ranges() {
        let (tbl, plan) = make_lookup_table_and_plan(true, false, false);
        let tbl_id = tbl.id;
        let is = make_info_schema(tbl).await;
        let mut s = MockSession::new(100);
        s.vars.index_lookup_size = 10;
        let mut b = new_executor_builder(&s, Some(is), Arc::new(TelemetryInfo::default()));
        let mut ilu = b.build_index_look_up_reader(&mut s, &plan).await.unwrap();

        let captured_ranges: Arc<parking_lot::Mutex<Vec<tidb_kv::KeyRange>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let captured_flags: Arc<parking_lot::Mutex<Option<(bool, bool)>>> =
            Arc::new(parking_lot::Mutex::new(None));

        let idx_closed = Arc::new(AtomicBool::new(false));
        let ic = Arc::clone(&idx_closed);
        ilu.select_result_hook.select_result_func = Some(Arc::new(move |_, _, _, _, _| {
            let c = Arc::clone(&ic);
            Box::pin(async move {
                // Deliberately unsorted handles from index scan: [30, 10, 20].
                Ok(Box::new(TrackedMockSelectResult {
                    rows: vec![vec![30], vec![10], vec![20]],
                    pos: 0,
                    closed: c,
                    next_err: None,
                    close_err: None,
                }) as Box<dyn SelectResult>)
            })
        }));

        let cr = Arc::clone(&captured_ranges);
        let cf = Arc::clone(&captured_flags);
        let tbl_closed = Arc::new(AtomicBool::new(false));
        let tc = Arc::clone(&tbl_closed);
        ilu.data_reader_builder
            .select_result_hook
            .select_result_func = Some(Arc::new(move |_, _, req, _, _| {
            *cr.lock() = req
                .key_ranges
                .as_ref()
                .map(|kr| kr.ranges().iter().flatten().cloned().collect())
                .unwrap_or_default();
            *cf.lock() = Some((req.keep_order, req.desc));
            let c = Arc::clone(&tc);
            Box::pin(async move {
                Ok(Box::new(TrackedMockSelectResult {
                    rows: vec![vec![10, 100], vec![20, 200], vec![30, 300]],
                    pos: 0,
                    closed: c,
                    next_err: None,
                    close_err: None,
                }) as Box<dyn SelectResult>)
            })
        }));

        let ctx = ExecCtx::new(s.vars.stmt_ctx_arc());
        exec::open(&ctx, &mut ilu).await.unwrap();
        let mut chk = exec::new_first_chunk(&ilu);
        let mut got = Vec::new();
        loop {
            exec::next(&ctx, &mut ilu, &mut chk).await.unwrap();
            if chk.num_rows() == 0 {
                break;
            }
            for i in 0..chk.num_rows() {
                got.push((chk.get_row(i).get_int64(0), chk.get_row(i).get_int64(1)));
            }
        }
        exec::close(&mut ilu).await.unwrap();

        // Output preserves index order [30, 10, 20], while the table-side KV request MUST sort
        // handles ascending [10, 20, 30] (Go builder.go:4944).
        assert_eq!(got, vec![(30, 300), (10, 100), (20, 200)]);
        let rec_prefix = tidb_tablecodec::gen_table_record_prefix(tbl_id);
        let expected_ranges: Vec<tidb_kv::KeyRange> = [10i64, 20, 30]
            .into_iter()
            .map(|h| {
                let start =
                    tidb_tablecodec::encode_record_key(&rec_prefix, &Handle::Int(IntHandle(h)));
                let end = start.prefix_next();
                tidb_kv::KeyRange {
                    start_key: start,
                    end_key: end,
                }
            })
            .collect();
        assert_eq!(*captured_ranges.lock(), expected_ranges);
        assert_eq!(*captured_flags.lock(), Some((false, false)));
    }

    #[tokio::test]
    async fn index_lookup_dropped_done_tx_returns_error_unless_finished() {
        let (tbl, plan) = make_lookup_table_and_plan(false, false, false);
        let is = make_info_schema(tbl).await;
        let mut s = MockSession::new(100);
        let mut b = new_executor_builder(&s, Some(is), Arc::new(TelemetryInfo::default()));
        let mut ilu = b.build_index_look_up_reader(&mut s, &plan).await.unwrap();

        // Unfinished executor + dropped oneshot sender -> internal error (never silent EOF).
        let (fin_tx, _fin_rx) = watch::channel(false);
        let (res_tx, res_rx) = mpsc::channel(1);
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        drop(done_tx);
        res_tx.send(done_rx).await.unwrap();
        drop(res_tx);
        ilu.finished_tx = Some(fin_tx.clone());
        ilu.result_rx = Some(res_rx);

        let err = ilu
            .get_result_task()
            .await
            .err()
            .expect("dropped done_tx without finished must return an error");
        assert!(err.to_string().contains("dropped task channel"), "{err}");

        // Once finished is signaled, a dropped oneshot sender cleanly yields Ok(None).
        let _ = fin_tx.send(true);
        let (res_tx2, res_rx2) = mpsc::channel(1);
        let (done_tx2, done_rx2) = tokio::sync::oneshot::channel();
        drop(done_tx2);
        res_tx2.send(done_rx2).await.unwrap();
        drop(res_tx2);
        ilu.result_rx = Some(res_rx2);
        assert!(ilu.get_result_task().await.unwrap().is_none());
    }

    #[test]
    fn index_lookup_get_datum_row_extracts_column_datums() {
        let fts = vec![Arc::new(int_ft()), Arc::new(int_ft())];
        let mut chk = chunk::new_chunk_with_capacity(&fts, 1);
        chk.append_int64(0, 7);
        chk.append_int64(1, 42);
        let ds = get_datum_row(&chk.get_row(0), &fts);
        assert_eq!(ds.len(), 2);
        assert_eq!(ds[0].get_int64(), 7);
        assert_eq!(ds[1].get_int64(), 42);
    }
}
