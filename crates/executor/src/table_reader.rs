//! go-file: pkg/executor/table_reader.go
//!
//! Port of Go TiDB v8.5.8 `pkg/executor/table_reader.go` (block D12.1 Part 2A: `TableReaderExecutor`,
//! `TableReaderExecutorContext`, `TableResultHandler`, `SelectResultHook`, `KvRangeBuilder`,
//! `buildVirtualColumnIndex`, `buildVirtualColumnInfo`).
//!
//! `TableReaderExecutor.buildKVReqForPartitionTableScan` is TiFlash BatchCop-only (`DEF.MPP`,
//! spec §9.5) and is not ported.

use std::cmp::Ordering as CmpOrdering;
use std::mem::size_of;
use std::sync::Arc;

use async_trait::async_trait;
use futures::future::BoxFuture;
use protobuf::Message;
use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_base::kv::{StoreType, TiFlash};
use tidb_chunk::chunk::Chunk;
use tidb_expression::Schema;
use tidb_infoschema::context::MetaOnlyInfoSchema;
use tidb_infoschema::table::TableRef;
use tidb_infoschema::table::column::fill_virtual_column_value;
use tidb_kv::context::RequestSource;
use tidb_kv::key::{KeyRange, key_range_slice_mem_usage};
use tidb_kv::kv::Request;
use tidb_memory::tracker::{Tracker, new_tracker};
use tidb_model::{ColumnInfo, find_column_info_by_id};
use tidb_parser::types::FieldType;
use tidb_planner::util::byitem::ByItems;
use tidb_sessionctx::distsql_ctx::DistSQLContext;
use tidb_sessionctx::exprctx::ExprContextRef;
use tidb_sessionctx::planctx::BuildPBContext;
use tidb_sessionctx::ranger::Range;
use tidb_sessionctx::ranger::context::RangerContext;
use tidb_store::distsql::{
    RequestBuilder, SelectResult, select_with_runtime_stats, split_ranges_across_int64_boundary,
};

use crate::analyze_utils::normalize_ctx_err_with_cause;
use crate::builder::{get_physical_table_id, new_closest_read_adjuster};
use crate::distsql::{Closeable, close_all, get_physical_plan_ids};
use crate::exec::ExecCtx;
use crate::glue::{Plan, SessionContext};
use crate::internal::builder::builder_utils::{
    construct_list_based_dist_exec, construct_tree_based_dist_exec,
};
use crate::internal::exec::executor::{BaseExecutorV2, Executor};
use crate::internal::exec::indexusage::IndexUsageReporter;
use crate::internal::util::partition_table::update_executor_table_id;

/// Function signature for the test hook in [`SelectResultHook`].
pub type SelectResultFunc = Arc<
    dyn Fn(
            &tidb_kv::Context,
            Arc<DistSQLContext>,
            &Request,
            Vec<Arc<FieldType>>,
            Vec<isize>,
        ) -> BoxFuture<'static, Result<Box<dyn SelectResult>, Error>>
        + Send
        + Sync,
>;

/// selectResultHook is used to hack `distsql::select_with_runtime_stats` safely for testing.
#[derive(Clone, Default)]
pub struct SelectResultHook {
    pub select_result_func: Option<SelectResultFunc>,
}

impl SelectResultHook {
    // go: selectResultHook.SelectResult L1 [M-GOCTX]
    pub async fn select_result(
        &self,
        ctx: &tidb_kv::Context,
        dctx: Arc<DistSQLContext>,
        kv_req: &Request,
        field_types: Vec<Arc<FieldType>>,
        cop_plan_ids: Vec<isize>,
        root_plan_id: isize,
    ) -> Result<Box<dyn SelectResult>, Error> {
        let Some(f) = &self.select_result_func else {
            return select_with_runtime_stats(
                ctx,
                dctx,
                kv_req,
                field_types,
                cop_plan_ids,
                root_plan_id,
            )
            .await;
        };
        f(ctx, dctx, kv_req, field_types, cop_plan_ids).await
    }
}

/// Go `kvRangeBuilder`: builds partitioned KV ranges from logical ranges.
pub trait KvRangeBuilder: Send + Sync {
    // glue: Go interface method kvRangeBuilder.buildKeyRange
    fn build_key_range(
        &self,
        dctx: &DistSQLContext,
        ranges: &[Range],
    ) -> Result<Vec<Vec<KeyRange>>, Error>;
    // glue: Go interface method kvRangeBuilder.buildKeyRangeSeparately
    fn build_key_range_separately(
        &self,
        dctx: &DistSQLContext,
        ranges: &[Range],
    ) -> Result<(Vec<i64>, Vec<Vec<KeyRange>>), Error>;
}

/// Optional DDL owner lookup returning the server ID (Go `infosync.ServerInfo.ServerIDGetter()`).
pub type GetDDLOwnerFn = Arc<dyn Fn(&tidb_kv::Context) -> Result<u64, Error> + Send + Sync>;

/// tableReaderExecutorContext is the execution context for [`TableReaderExecutor`].
#[derive(Clone)]
pub struct TableReaderExecutorContext {
    pub dctx: Arc<DistSQLContext>,
    pub rctx: Arc<RangerContext>,
    pub build_pb_ctx: Arc<BuildPBContext>,
    pub ectx: ExprContextRef,

    pub stmt_mem_tracker: Option<Arc<Tracker>>,

    pub info_schema: Option<Arc<dyn MetaOnlyInfoSchema>>,
    pub get_ddl_owner: Option<GetDDLOwnerFn>,
}

impl TableReaderExecutorContext {
    // go: tableReaderExecutorContext.GetInfoSchema L1
    pub fn get_info_schema(&self) -> Option<Arc<dyn MetaOnlyInfoSchema>> {
        self.info_schema.as_ref().map(Arc::clone)
    }

    // go: tableReaderExecutorContext.GetDDLOwner L2 (ServerInfo reduced to server ID u64; domain.DDL is D06.5)
    pub fn get_ddl_owner(&self, ctx: &tidb_kv::Context) -> Result<u64, Error> {
        if let Some(f) = &self.get_ddl_owner {
            return f(ctx);
        }
        Err(Error::plain("GetDDLOwner in a context without DDL"))
    }
}

/// newTableReaderExecutorContext builds the frozen execution context for [`TableReaderExecutor`].
///
/// `domain.DDL()` is D06.5 (not ported), so `get_ddl_owner` is `None`.
// go: newTableReaderExecutorContext L2 (domain.DDL is D06.5 so get_ddl_owner is None; N2 freezes session contexts)
pub fn new_table_reader_executor_context(
    sctx: &dyn SessionContext,
) -> Result<TableReaderExecutorContext, Error> {
    let build_pb_ctx = sctx.get_build_pb_ctx()?;
    let dctx = sctx.get_dist_sql_ctx();
    let rctx = sctx.get_ranger_ctx();
    let ectx = sctx.get_expr_ctx();
    let stmt_mem_tracker = sctx.get_session_vars().stmt_ctx().mem_tracker.clone();
    let info_schema = sctx
        .get_info_schema()
        .map(|is| is as Arc<dyn MetaOnlyInfoSchema>);
    Ok(TableReaderExecutorContext {
        dctx,
        rctx,
        build_pb_ctx,
        ectx,
        stmt_mem_tracker,
        info_schema,
        get_ddl_owner: None,
    })
}

/// Construct a lightweight `tidb_kv::Context` from `DistSQLContext` (spec §8.3, `M-GOCTX`).
// glue: M-GOCTX request source propagation from DistSQLContext
pub(crate) fn kv_ctx_from_dctx(dctx: &DistSQLContext) -> tidb_kv::Context {
    let base = tidb_kv::Context::background();
    if dctx.in_restricted_sql
        || !dctx.request_source_type.is_empty()
        || !dctx.explicit_request_source_type.is_empty()
    {
        base.with_request_source(RequestSource {
            request_source_internal: dctx.in_restricted_sql,
            request_source_type: dctx.request_source_type.clone(),
            explicit_request_source_type: dctx.explicit_request_source_type.clone(),
        })
    } else {
        base
    }
}

/// TableReaderExecutor sends DAG request and reads table data from kv layer.
pub struct TableReaderExecutor {
    pub ctx: TableReaderExecutorContext,
    pub base: BaseExecutorV2,
    pub index_usage_reporter: Option<IndexUsageReporter>,

    pub table: Option<TableRef>,

    pub kv_range_builder: Option<Arc<dyn KvRangeBuilder>>,
    pub ranges: Vec<Range>,

    pub grouped_ranges: Vec<Vec<Range>>,
    pub group_by_col_idxs: Vec<isize>,

    /// kv_ranges are only used for union scan.
    pub kv_ranges: Vec<KeyRange>,
    pub dag_pb: tipb::DagRequest,
    pub start_ts: u64,
    pub txn_scope: String,
    pub read_replica_scope: String,
    pub is_staleness: bool,
    pub net_data_size: f64,
    /// columns are only required by union scan and virtual column.
    pub columns: Vec<Arc<ColumnInfo>>,

    /// result_handler handles the order of the result. Since (MaxInt64, MaxUint64] stores
    /// before [0, MaxInt64] physically for unsigned int.
    pub result_handler: Option<TableResultHandler>,
    pub plans: Vec<Plan>,
    pub table_plan: Option<Plan>,

    pub mem_tracker: Option<Arc<Tracker>>,
    pub select_result_hook: SelectResultHook,

    pub keep_order: bool,
    pub desc: bool,
    /// by_items only for partition table with orderBy + pushedLimit
    pub by_items: Vec<ByItems>,
    pub paging: bool,
    pub store_type: StoreType,
    pub cor_col_in_filter: bool,
    pub cor_col_in_access: bool,
    /// virtual_column_index records all the indices of virtual columns and sorts them in
    /// definition order to make sure we can compute virtual columns in right order.
    pub virtual_column_index: Vec<usize>,
    /// virtual_column_ret_field_types records the RetFieldTypes of virtual columns.
    pub virtual_column_ret_field_types: Vec<Arc<FieldType>>,
    /// batch_cop indicates whether to use super batch coprocessor request (TiFlash only).
    pub batch_cop: bool,

    /// If dummy flag is set, this is not a real TableReader, it just provides KV ranges for
    /// UnionScan. Used by temporary tables and cached tables.
    pub dummy: bool,
}

impl TableReaderExecutor {
    /// Table implements the dataSourceExecutor interface.
    // go: TableReaderExecutor.Table L1
    pub fn table(&self) -> Option<&TableRef> {
        self.table.as_ref()
    }

    // go: TableReaderExecutor.setDummy L1
    pub fn set_dummy(&mut self) {
        self.dummy = true;
    }

    // go: TableReaderExecutor.memUsage L2 (A1 memory estimate using Rust struct size and key_range_slice_mem_usage)
    pub fn mem_usage(&self) -> i64 {
        let mut res = size_of::<Self>() as i64;
        res += (size_of::<Range>() * self.ranges.capacity()) as i64;
        for v in &self.ranges {
            res += v.mem_usage();
        }
        res += key_range_slice_mem_usage(&self.kv_ranges);
        res += i64::from(self.dag_pb.compute_size());
        res
    }

    /// Open initializes necessary variables for using this executor.
    // go: TableReaderExecutor.Open L2 (corColInAccess stops at blocked-by:D10.1; Drop closes partial SelectResults on error)
    pub async fn open_impl(&mut self, _ctx: &ExecCtx) -> Result<(), Error> {
        if let Some(t) = &self.mem_tracker {
            t.reset()?;
        } else {
            self.mem_tracker = Some(new_tracker(self.base.meta.id(), -1));
        }
        if let (Some(t), Some(stmt_t)) = (&self.mem_tracker, &self.ctx.stmt_mem_tracker) {
            t.attach_to(stmt_t)?;
        }

        if self.cor_col_in_filter {
            // If there's correlated column in filter, need to rewrite dagPB
            if self.store_type == TiFlash {
                let table_plan = self
                    .table_plan
                    .as_ref()
                    .expect("TableReaderExecutor.table_plan");
                let mut execs = construct_tree_based_dist_exec(&self.ctx.build_pb_ctx, table_plan)?;
                self.dag_pb.set_root_executor(execs.remove(0));
            } else {
                let execs = construct_list_based_dist_exec(&self.ctx.build_pb_ctx, &self.plans)?;
                self.dag_pb.set_executors(execs.into());
            }
        }
        if self.ctx.dctx.runtime_stats_coll.is_some() {
            self.dag_pb.set_collect_execution_summaries(true);
        }
        if self.cor_col_in_access {
            // glue: stub, blocked-by:D10.1 (PhysicalTableScan.ResolveCorrelatedColumns / GroupRangesByCols)
            return Err(blocked(
                "D10.1",
                "PhysicalTableScan.ResolveCorrelatedColumns (correlated column in TableReader access condition)",
            ));
        }

        self.result_handler = Some(TableResultHandler::default());

        // Use a unified `grouped_ranges` to represent the ranges to be read, whether they
        // come from `self.ranges` or `self.grouped_ranges`.
        let grouped_ranges: Vec<Vec<Range>> = if !self.grouped_ranges.is_empty() {
            self.grouped_ranges.clone()
        } else if !self.ranges.is_empty() {
            vec![self.ranges.clone()]
        } else {
            Vec::new()
        };

        let is_common_handle = self
            .table
            .as_ref()
            .is_some_and(|t| t.meta().is_common_handle);
        let mut first_part_grouped_ranges: Vec<Vec<Range>> = Vec::new();
        let mut second_part_grouped_ranges: Vec<Vec<Range>> = Vec::new();
        for ranges in &grouped_ranges {
            let (signed_ranges, unsigned_ranges) = split_ranges_across_int64_boundary(
                ranges,
                self.keep_order,
                self.desc,
                is_common_handle,
            );
            if !signed_ranges.is_empty() {
                first_part_grouped_ranges.push(signed_ranges);
            }
            if !unsigned_ranges.is_empty() {
                second_part_grouped_ranges.push(unsigned_ranges);
            }
        }

        // kv_ctx carries DistSQL request-source metadata rather than ExecCtx; KILL is checked
        // after each response (Go select_result.go:454), and dropping the future closes workers via Drop.
        let kv_ctx = kv_ctx_from_dctx(&self.ctx.dctx);

        // Treat temporary table as dummy table, avoid sending distsql request to TiKV.
        // Calculate the kv ranges here, UnionScan relies on these kv ranges.
        if self.dummy {
            if self.desc && !second_part_grouped_ranges.is_empty() {
                std::mem::swap(
                    &mut first_part_grouped_ranges,
                    &mut second_part_grouped_ranges,
                );
            }
            for ranges in &first_part_grouped_ranges {
                let kv_req = self.build_kv_req(&kv_ctx, ranges)?;
                if let Some(krs) = &kv_req.key_ranges {
                    let existing = std::mem::take(&mut self.kv_ranges);
                    self.kv_ranges = krs.append_self_to(existing);
                }
            }
            for ranges in &second_part_grouped_ranges {
                let kv_req = self.build_kv_req(&kv_ctx, ranges)?;
                if let Some(krs) = &kv_req.key_ranges {
                    let existing = std::mem::take(&mut self.kv_ranges);
                    self.kv_ranges = krs.append_self_to(existing);
                }
            }
            return Ok(());
        }

        let first_result = self
            .build_resp_for_grouped_ranges(&kv_ctx, &first_part_grouped_ranges)
            .await?;
        if second_part_grouped_ranges.is_empty() {
            self.result_handler
                .as_mut()
                .expect("result_handler initialized")
                .open(None, Some(first_result));
            return Ok(());
        }
        let second_result = match self
            .build_resp_for_grouped_ranges(&kv_ctx, &second_part_grouped_ranges)
            .await
        {
            Ok(sr) => sr,
            Err(err) => {
                // Close first_result explicitly before returning so mock and real
                // SelectResults release resources immediately (spec §10.3 / §13 item 4).
                let mut first = first_result;
                let _ = first.close();
                return Err(err);
            }
        };
        self.result_handler
            .as_mut()
            .expect("result_handler initialized")
            .open(Some(first_result), Some(second_result));
        Ok(())
    }

    /// Next fills data into the chunk passed by its caller.
    /// The task is delegated to [`TableResultHandler`], followed by virtual column evaluation.
    // go: TableReaderExecutor.Next L1 [M-GOCTX]
    pub async fn next_impl(&mut self, _ctx: &ExecCtx, req: &mut Chunk) -> Result<(), Error> {
        if self.dummy {
            // Treat temporary table as dummy table, avoid sending distsql request to TiKV.
            req.reset();
            return Ok(());
        }

        // kv_ctx carries DistSQL request-source metadata rather than ExecCtx; KILL is checked
        // after each response (Go select_result.go:454), and dropping the future closes workers via Drop.
        let kv_ctx = kv_ctx_from_dctx(&self.ctx.dctx);
        self.result_handler
            .as_mut()
            .expect("TableReaderExecutor.Next called before Open")
            .next_chunk(&kv_ctx, req)
            .await?;

        let schema_cols = &self.base.meta.schema().columns;
        fill_virtual_column_value(
            &self.virtual_column_ret_field_types,
            &self.virtual_column_index,
            schema_cols,
            &self.columns,
            &*self.ctx.ectx,
            req,
        )?;

        Ok(())
    }

    /// Close implements the Executor Close interface.
    // go: TableReaderExecutor.Close L1
    pub async fn close_impl(&mut self) -> Result<(), Error> {
        if let Some(reporter) = &self.index_usage_reporter
            && let Some(tbl) = &self.table
            && let Some(first_plan) = self.plans.first()
        {
            let phys_id = tbl.as_physical_table().map(|p| p.get_physical_id());
            reporter.report_cop_index_usage_for_handle(
                tbl.meta(),
                phys_id,
                first_plan.id() as isize,
            );
        }

        let mut err: Result<(), Error> = Ok(());
        if let Some(rh) = &mut self.result_handler {
            err = rh.close();
        }
        self.kv_ranges.clear();
        if self.dummy {
            return Ok(());
        }
        err
    }

    /// buildRespForGroupedRanges builds the input `Range` groups into a [`SelectResult`].
    ///
    /// TiFlash partitioned paths (`NewSerialSelectResults` / `buildKVReqForPartitionTableScan`)
    /// stop at `blocked("DEF.MPP", ...)`; multi-result merge (`NewSortedSelectResults`) stops
    /// at `blocked("D07.3", ...)` until D07.3 Part 2 lands (spec §8.3, §11.1).
    // go: TableReaderExecutor.buildRespForGroupedRanges L2 (TiFlash BatchCop/Serial is DEF.MPP; multi-result NewSortedSelectResults is blocked-by:D07.3 until Part 2; closes partial SelectResults on error)
    pub(crate) async fn build_resp_for_grouped_ranges(
        &mut self,
        ctx: &tidb_kv::Context,
        grouped_ranges: &[Vec<Range>],
    ) -> Result<Box<dyn SelectResult>, Error> {
        if self.store_type == TiFlash && self.kv_range_builder.is_some() {
            debug_assert!(grouped_ranges.len() == 1 && self.grouped_ranges.is_empty());
            // glue: stub, blocked-by:DEF.MPP (TiFlash partitioned TableReader: NewSerialSelectResults / BatchCop)
            return Err(blocked(
                "DEF.MPP",
                "TiFlash partitioned TableReader (NewSerialSelectResults / BatchCop)",
            ));
        }

        let mut kv_reqs = self.build_kv_req_separately_for_grouped_ranges(ctx, grouped_ranges)?;
        // Even if it's an empty request, we still need to build a kvReq to make the following
        // execution logic work. Otherwise, there will be a panic.
        if kv_reqs.is_empty() {
            let kv_req = self.build_kv_req(ctx, &[])?;
            kv_reqs.push(kv_req);
        }
        self.kv_ranges = Self::sort_and_get_kv_ranges_from_reqs(&mut kv_reqs);
        let ret_types: Vec<Arc<FieldType>> = self.base.meta.ret_field_types().to_vec();
        let cop_plan_ids = get_physical_plan_ids(&self.plans);
        let root_plan_id = self.base.meta.id();

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
        if results.len() == 1 {
            return Ok(results.remove(0));
        }

        debug_assert!(
            !self.by_items.is_empty(),
            "In current logic, if there are more than one result, len(e.byItems) must be > 0"
        );
        for sr in &mut results {
            let _ = sr.close();
        }
        // glue: stub, blocked-by:D07.3 (distsql.NewSortedSelectResults for multi-group TableReader)
        Err(blocked(
            "D07.3",
            "distsql.NewSortedSelectResults for multi-group TableReader",
        ))
    }

    // go: TableReaderExecutor.buildKVReqSeparatelyForGroupedRanges L1
    pub(crate) fn build_kv_req_separately_for_grouped_ranges(
        &mut self,
        ctx: &tidb_kv::Context,
        grouped_ranges: &[Vec<Range>],
    ) -> Result<Vec<Request>, Error> {
        let mut kv_reqs: Vec<Request> = Vec::new();
        for ranges in grouped_ranges {
            if self.kv_range_builder.is_some() && !self.by_items.is_empty() {
                let reqs = self.build_kv_req_separately(ctx, ranges)?;
                kv_reqs.extend(reqs);
            } else {
                let kv_req = self.build_kv_req(ctx, ranges)?;
                kv_reqs.push(kv_req);
            }
        }
        Ok(kv_reqs)
    }

    /// sortAndGetKVRangesFromReqs sorts each request's `KeyRanges` by `StartKey` and returns the
    /// merged sorted slice of all `KeyRange`s.
    // go: sortAndGetKVRangesFromReqs L1
    pub fn sort_and_get_kv_ranges_from_reqs(kv_reqs: &mut [Request]) -> Vec<KeyRange> {
        let mut kv_ranges: Vec<KeyRange> = Vec::with_capacity(kv_reqs.len());
        for kv_req in kv_reqs.iter_mut() {
            if let Some(krs) = &mut kv_req.key_ranges {
                krs.sort_by_func(|i, j| match i.start_key.0.cmp(&j.start_key.0) {
                    CmpOrdering::Less => -1,
                    CmpOrdering::Equal => 0,
                    CmpOrdering::Greater => 1,
                });
                kv_ranges = krs.append_self_to(kv_ranges);
            }
        }
        kv_ranges.sort_by(|i, j| i.start_key.0.cmp(&j.start_key.0));
        kv_ranges
    }

    // go: TableReaderExecutor.buildKVReqSeparately L1
    pub(crate) fn build_kv_req_separately(
        &mut self,
        ctx: &tidb_kv::Context,
        ranges: &[Range],
    ) -> Result<Vec<Request>, Error> {
        let (pids, kv_ranges) = self
            .kv_range_builder
            .as_ref()
            .expect("buildKVReqSeparately requires kv_range_builder")
            .build_key_range_separately(&self.ctx.dctx, ranges)?;
        let mut kv_reqs = Vec::with_capacity(kv_ranges.len());
        for (i, kv_range) in kv_ranges.into_iter().enumerate() {
            let root_exec = if self.dag_pb.has_root_executor() {
                Some(self.dag_pb.mut_root_executor())
            } else {
                None
            };
            update_executor_table_id(ctx, root_exec, true, &[pids[i]])?;
            let mut builder = RequestBuilder::default();
            builder
                .set_key_ranges(kv_range)
                .set_dag_request(self.dag_pb.clone())
                .set_start_ts(self.start_ts)
                .set_desc(self.desc)
                .set_keep_order(self.keep_order)
                .set_txn_scope(self.txn_scope.clone())
                .set_read_replica_scope(self.read_replica_scope.clone())
                .set_from_session_vars(&self.ctx.dctx);
            if let Some(is) = self.ctx.get_info_schema() {
                builder.set_from_info_schema(is);
            }
            builder
                .set_mem_tracker(self.mem_tracker.clone())
                .set_store_type(self.store_type)
                .set_paging(self.paging)
                .set_allow_batch_cop(self.batch_cop);
            let adjuster =
                new_closest_read_adjuster(&self.ctx.dctx, &builder.request, self.net_data_size);
            builder
                .set_closest_replica_read_adjuster(adjuster)
                .set_conn_id_and_conn_alias(
                    self.ctx.dctx.connection_id,
                    self.ctx.dctx.session_alias.clone(),
                );
            let kv_req = builder.build()?;
            kv_reqs.push(kv_req);
        }
        Ok(kv_reqs)
    }

    // go-exception: DEF.MPP TableReaderExecutor.buildKVReqForPartitionTableScan (TiFlash BatchCop only, spec §9.5)

    // go: TableReaderExecutor.buildKVReq L2 (cluster table DDLOwner routing stops at blocked-by:D06.5 when no get_ddl_owner is configured)
    pub(crate) fn build_kv_req(
        &self,
        ctx: &tidb_kv::Context,
        ranges: &[Range],
    ) -> Result<Request, Error> {
        let mut builder = RequestBuilder::default();
        if let Some(krb) = &self.kv_range_builder {
            let kv_range = krb.build_key_range(&self.ctx.dctx, ranges)?;
            builder.set_partition_key_ranges(kv_range);
        } else {
            let Some(tbl) = &self.table else {
                return Err(Error::plain(
                    "TableReaderExecutor.buildKVReq: table is None",
                ));
            };
            let tid = get_physical_table_id(tbl.as_ref());
            let is_common_handle = tbl.meta().is_common_handle;
            builder.set_handle_ranges(Some(&self.ctx.dctx), tid, is_common_handle, ranges);
        }
        // TODO(D10.11): port GetClusterTableCopDestination (Go only requests DDLOwner when
        // destination == DDLOwner, not for all-TiDB cluster tables) and move this boundary
        // into build_table_reader's pre-Open guards.
        if let Some(tbl) = &self.table
            && tbl.type_().is_cluster_table()
        {
            if self.ctx.get_ddl_owner.is_some() {
                let server_id = self.ctx.get_ddl_owner(ctx)?;
                builder.set_tidb_server_id(server_id);
            } else {
                // glue: stub, blocked-by:D06.5 (cluster table DDLOwner lookup in TableReaderExecutor.buildKVReq)
                return Err(blocked(
                    "D06.5",
                    "cluster table DDLOwner lookup in TableReaderExecutor.buildKVReq",
                ));
            }
        }
        builder
            .set_dag_request(self.dag_pb.clone())
            .set_start_ts(self.start_ts)
            .set_desc(self.desc)
            .set_keep_order(self.keep_order)
            .set_txn_scope(self.txn_scope.clone())
            .set_read_replica_scope(self.read_replica_scope.clone())
            .set_is_staleness(self.is_staleness)
            .set_from_session_vars(&self.ctx.dctx);
        if let Some(is) = self.ctx.get_info_schema() {
            builder.set_from_info_schema(is);
        }
        builder
            .set_mem_tracker(self.mem_tracker.clone())
            .set_store_type(self.store_type)
            .set_allow_batch_cop(self.batch_cop);
        let adjuster =
            new_closest_read_adjuster(&self.ctx.dctx, &builder.request, self.net_data_size);
        builder
            .set_closest_replica_read_adjuster(adjuster)
            .set_paging(self.paging)
            .set_conn_id_and_conn_alias(
                self.ctx.dctx.connection_id,
                self.ctx.dctx.session_alias.clone(),
            );
        builder.build()
    }
}

/// The schema positions of the virtual generated columns, in column definition order (the
/// order they are computed in).
///
/// Go calls `plannercore.FindColumnInfoByID` (D10.9, VS1-31); the model's function of the
/// same name has the same body (the first column with the ID).
// go: buildVirtualColumnIndex L1
pub fn build_virtual_column_index(schema: &Schema, columns: &[Arc<ColumnInfo>]) -> Vec<usize> {
    let mut virtual_column_index: Vec<usize> = Vec::with_capacity(columns.len());
    for (i, col) in schema.columns.iter().enumerate() {
        if col.virtual_expr.is_some() {
            virtual_column_index.push(i);
        }
    }
    let offset = |i: usize| {
        find_column_info_by_id(columns, schema.columns[i].id)
            .expect("a virtual column of the plan's columns")
            .offset
    };
    // slices.SortFunc: not stable; the offsets of distinct columns differ.
    virtual_column_index.sort_by_key(|&i| offset(i));
    virtual_column_index
}

impl TableReaderExecutor {
    /// buildVirtualColumnInfo saves virtual column indices and sorts them in definition order.
    // go: TableReaderExecutor.buildVirtualColumnInfo L1
    pub fn build_virtual_column_info(&mut self) {
        let schema = self.base.meta.schema();
        let (idx, types) = build_virtual_column_info(&schema, &self.columns);
        self.virtual_column_index = idx;
        self.virtual_column_ret_field_types = types;
    }
}

/// buildVirtualColumnInfo saves virtual column indices and sorts them in definition order.
// go: buildVirtualColumnInfo L1 [M-SHARE]
pub fn build_virtual_column_info(
    schema: &Schema,
    columns: &[Arc<ColumnInfo>],
) -> (Vec<usize>, Vec<Arc<FieldType>>) {
    let col_idxs = build_virtual_column_index(schema, columns);
    let mut ret_types = Vec::new();
    if !col_idxs.is_empty() {
        ret_types = Vec::with_capacity(col_idxs.len());
        for &idx in &col_idxs {
            ret_types.push(Arc::clone(&schema.columns[idx].ret_type));
        }
    }
    (col_idxs, ret_types)
}

#[async_trait]
impl Executor for TableReaderExecutor {
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
        crate::detach::table_reader_executor_detach(self)
    }
}

/// tableResultHandler manages the two ordered result streams when an unsigned integer handle
/// range crosses `MaxInt64`.
///
/// If the PK is unsigned and `KeepOrder=true`:
/// - ascending: `optional_result` handles `[0, MaxInt64]` and `result` handles `(MaxInt64, MaxUint64]`;
/// - descending: `optional_result` handles `(MaxInt64, MaxUint64]` and `result` handles `[0, MaxInt64]`;
/// - otherwise: `optional_finished = true` and `result` handles the whole range.
#[derive(Default)]
pub struct TableResultHandler {
    optional_result: Option<Box<dyn SelectResult>>,
    result: Option<Box<dyn SelectResult>>,
    optional_finished: bool,
}

impl Drop for TableResultHandler {
    /// Safety net when `TableReaderExecutor` is dropped without `close()`: closes both
    /// underlying `SelectResult` streams so no coprocessor worker leaks (spec §10.3).
    // glue: Drop safety net (N3 §3.5, spec §10.3)
    fn drop(&mut self) {
        let _ = self.close();
    }
}

impl TableResultHandler {
    // go: tableResultHandler.open L1
    pub fn open(
        &mut self,
        optional_result: Option<Box<dyn SelectResult>>,
        result: Option<Box<dyn SelectResult>>,
    ) {
        if optional_result.is_none() {
            self.optional_finished = true;
            self.result = result;
            return;
        }
        self.optional_result = optional_result;
        self.result = result;
        self.optional_finished = false;
    }

    // go: tableResultHandler.nextChunk L1 [M-GOCTX]
    pub async fn next_chunk(
        &mut self,
        ctx: &tidb_kv::Context,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        if !self.optional_finished {
            self.optional_result
                .as_mut()
                .expect("optional_result set when !optional_finished")
                .next(ctx, chk)
                .await?;
            if chk.num_rows() > 0 {
                return Ok(());
            }
            self.optional_finished = true;
        }
        self.result
            .as_mut()
            .expect("result set in tableResultHandler")
            .next(ctx, chk)
            .await
    }

    // go: tableResultHandler.nextRaw L1 [M-GOCTX]
    pub async fn next_raw(&mut self, ctx: &tidb_kv::Context) -> Result<Option<Vec<u8>>, Error> {
        if !self.optional_finished {
            let data = self
                .optional_result
                .as_mut()
                .expect("optional_result set when !optional_finished")
                .next_raw(ctx)
                .await
                .map_err(|err| normalize_ctx_err_with_cause(ctx, err))?;
            if let Some(data) = data {
                return Ok(Some(data));
            }
            self.optional_finished = true;
        }
        self.result
            .as_mut()
            .expect("result set in tableResultHandler")
            .next_raw(ctx)
            .await
            .map_err(|err| normalize_ctx_err_with_cause(ctx, err))
    }

    // go: tableResultHandler.Close L1
    pub fn close(&mut self) -> Result<(), Error> {
        let mut opt = self.optional_result.take();
        let mut res = self.result.take();
        let opt_ref: Option<&mut dyn Closeable> = opt.as_mut().map(|r| r as &mut dyn Closeable);
        let res_ref: Option<&mut dyn Closeable> = res.as_mut().map(|r| r as &mut dyn Closeable);
        close_all(&mut [opt_ref, res_ref])
    }
}

/// The schema the table reader executor is built with: the reader's, or its table plan's when
/// the reader has none. Go writes the table plan's schema into the plan
/// (`v.SetSchema(v.GetTablePlan().Schema())`, upstream `#50358`); the plan may be the one in
/// the instance plan cache, shared by every session, so the executor reads the schema
/// instead (the plan-cache exception A3 spec §2.3 row 9b). `buildTableReader` (D12.1, VS1-34)
/// builds the executor and its result columns from this schema.
// go-part: executorBuilder.buildTableReader (the empty-schema fix of upstream #50358, read instead of written: A3 spec §2.3 row 9b)
pub fn table_reader_schema(v: &tidb_planner::core::physical_plans::PhysicalTableReader) -> &Schema {
    use tidb_planner::PhysicalPlan;
    if v.schema().columns.is_empty()
        && let Some(tp) = v.get_table_plan()
        && !tp.schema().columns.is_empty()
    {
        return tp.schema();
    }
    v.schema()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tidb_expression::{Column, new_schema};
    use tidb_planner::core::physical_plans::{PhysicalTableReader, PhysicalTableScan};
    use tidb_planner::core::plan_cache_heap_size::{plan_structure_hash, seal_plan};
    use tidb_planner::{PhysicalPlan, PhysicalPlanRef};

    use super::table_reader_schema;

    /// A3 spec §2.3 row 9b: a cached table reader whose schema is empty (Go's `#50358` case)
    /// gives the executor its table plan's schema, and the cached plan is unchanged
    /// (structural hash) and still has no schema of its own.
    #[test]
    fn empty_reader_schema_is_read_from_the_table_plan_not_written() {
        let col = Arc::new(Column {
            unique_id: 1,
            ..Default::default()
        });
        let mut scan = PhysicalTableScan::default();
        scan.set_schema(new_schema(vec![col]));
        let scan: PhysicalPlanRef = Arc::new(scan);
        let mut reader = PhysicalTableReader::default();
        reader.set_schema(new_schema(Vec::new()));
        reader.set_children(vec![Arc::clone(&scan)]);
        let reader: PhysicalPlanRef = Arc::new(reader);
        seal_plan(reader.as_ref());
        let before = plan_structure_hash(reader.as_ref());

        let r = reader
            .as_any()
            .downcast_ref::<PhysicalTableReader>()
            .unwrap();
        assert_eq!(table_reader_schema(r).columns.len(), 1);
        assert!(r.schema().columns.is_empty());
        assert_eq!(plan_structure_hash(reader.as_ref()), before);
    }

    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use async_trait::async_trait;
    use tidb_base::Error;
    use tidb_base::blocked::blocked_by;
    use tidb_base::kv::{TiFlash, TiKV};
    use tidb_base::mysql::{TypeLonglong, UnsignedFlag};
    use tidb_chunk::chunk::Chunk;
    use tidb_expression::{CorrelatedColumn, Expr};
    use tidb_infoschema::table::{PhysicalTable, TableRef, mock_table_from_meta};
    use tidb_kv::kv::Request;
    use tidb_kv::option::{ReplicaReadClosestAdaptive, ReplicaReadLeader};
    use tidb_model::{
        ColumnInfo, DBInfo, PartitionInfo, StatePublic, TableCacheStatusEnable, TableInfo,
        TempTableLocal,
    };
    use tidb_parser::model::CIStr;
    use tidb_parser::types::FieldType;
    use tidb_planner::core::physical_plans::{
        BatchCop, Cop, PhysicalLimit, PhysicalProjection, PhysicalSelection, PhysicalSort,
        PhysicalTopN,
    };
    use tidb_planner::util::byitem::ByItems;
    use tidb_sessionctx::distsql_ctx::DistSQLContext;
    use tidb_sessionctx::ranger::Range;
    use tidb_statistics::HistColl;
    use tidb_store::distsql::{SelectResult, SelectResultIter};
    use tidb_types::datum::{new_int_datum, new_uint_datum};
    use tidb_types::field_type::new_field_type;

    use super::*;
    use crate::adapter::TelemetryInfo;
    use crate::adapter::tests::MockSession;
    use crate::builder::{
        KvRangeBuilderFromRangeAndPartition, new_closest_read_adjuster, new_executor_builder,
    };
    use crate::exec::ExecCtx;
    use crate::internal::exec::executor::{self as exec, Executor, new_base_executor_v2};
    use crate::internal::util::partition_table::update_executor_table_id;

    struct TrackedMockSelectResult {
        rows: Vec<Vec<i64>>,
        raw_batches: Vec<Vec<u8>>,
        pos: usize,
        raw_pos: usize,
        closed: Arc<AtomicBool>,
        close_calls: Arc<AtomicUsize>,
        close_err: Option<String>,
        next_err: Option<String>,
    }

    impl TrackedMockSelectResult {
        fn new(rows: Vec<Vec<i64>>) -> (Self, Arc<AtomicBool>, Arc<AtomicUsize>) {
            let closed = Arc::new(AtomicBool::new(false));
            let close_calls = Arc::new(AtomicUsize::new(0));
            (
                Self {
                    rows,
                    raw_batches: Vec::new(),
                    pos: 0,
                    raw_pos: 0,
                    closed: Arc::clone(&closed),
                    close_calls: Arc::clone(&close_calls),
                    close_err: None,
                    next_err: None,
                },
                closed,
                close_calls,
            )
        }
    }

    #[async_trait]
    impl SelectResult for TrackedMockSelectResult {
        async fn next_raw(&mut self, _ctx: &tidb_kv::Context) -> Result<Option<Vec<u8>>, Error> {
            if let Some(msg) = &self.next_err {
                return Err(Error::plain(msg.clone()));
            }
            if self.raw_pos >= self.raw_batches.len() {
                return Ok(None);
            }
            let out = self.raw_batches[self.raw_pos].clone();
            self.raw_pos += 1;
            Ok(Some(out))
        }

        async fn next(&mut self, _ctx: &tidb_kv::Context, chk: &mut Chunk) -> Result<(), Error> {
            chk.reset();
            if let Some(msg) = &self.next_err {
                return Err(Error::plain(msg.clone()));
            }
            let take = chk
                .required_rows()
                .min(self.rows.len().saturating_sub(self.pos));
            for r in &self.rows[self.pos..self.pos + take] {
                for (col_idx, &val) in r.iter().enumerate() {
                    chk.append_int64(col_idx, val);
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
            self.close_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(msg) = &self.close_err {
                return Err(Error::plain(msg.clone()));
            }
            Ok(())
        }
    }

    fn int_col(id: i64, index: isize, unsigned: bool) -> Arc<Column> {
        let mut ft = new_field_type(TypeLonglong);
        if unsigned {
            ft.set_flag(UnsignedFlag);
        }
        Arc::new(Column {
            id,
            unique_id: id,
            index,
            ret_type: Arc::new(ft),
            ..Column::default()
        })
    }

    fn test_table_info(id: i64, unsigned_pk: bool) -> Arc<TableInfo> {
        let mut pk_ft = new_field_type(TypeLonglong);
        if unsigned_pk {
            pk_ft.set_flag(UnsignedFlag);
        }
        let c1 = Arc::new(ColumnInfo {
            id: 1,
            name: CIStr::new("id"),
            offset: 0,
            field_type: pk_ft,
            state: StatePublic,
            ..Default::default()
        });
        let c2 = Arc::new(ColumnInfo {
            id: 2,
            name: CIStr::new("v"),
            offset: 1,
            field_type: new_field_type(TypeLonglong),
            state: StatePublic,
            ..Default::default()
        });
        Arc::new(TableInfo {
            id,
            name: CIStr::new("t"),
            pk_is_handle: true,
            columns: Some(vec![c1, c2]),
            state: StatePublic,
            ..Default::default()
        })
    }

    async fn info_schema_with_table(tbl: Arc<TableInfo>) -> crate::glue::InfoSchemaRef {
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

    fn make_table_reader_plan(
        tbl: Arc<TableInfo>,
        ranges: Vec<Range>,
        keep_order: bool,
        desc: bool,
    ) -> PhysicalTableReader {
        let unsigned = tbl
            .get_pk_col_info()
            .is_some_and(|c| tidb_base::mysql::has_unsigned_flag(c.get_flag()));
        let schema = new_schema(vec![int_col(1, 0, unsigned), int_col(2, 1, false)]);
        let mut scan = PhysicalTableScan {
            table: Arc::clone(&tbl),
            columns: tbl.columns.clone().unwrap_or_default(),
            ranges,
            keep_order,
            desc,
            store_type: TiKV,
            tbl_col_hists: Some(Arc::new(HistColl::default())),
            ..Default::default()
        };
        scan.set_id(2);
        scan.set_schema(schema.clone());
        scan.set_stats(Some(Arc::new(tidb_planner::property::StatsInfo {
            row_count: 100.0,
            ..Default::default()
        })));
        let scan_ref: PhysicalPlanRef = Arc::new(scan);
        let mut reader = PhysicalTableReader {
            table_plan: Some(Arc::clone(&scan_ref)),
            table_plans: vec![Arc::clone(&scan_ref)],
            store_type: TiKV,
            read_req_type: Cop,
            ..Default::default()
        };
        reader.set_id(1);
        reader.set_schema(schema);
        reader
    }

    fn bare_reader(
        sctx: &MockSession,
        tbl: TableRef,
        ranges: Vec<Range>,
        keep_order: bool,
        desc: bool,
        hook: SelectResultHook,
    ) -> TableReaderExecutor {
        let unsigned = tbl
            .meta()
            .get_pk_col_info()
            .is_some_and(|c| tidb_base::mysql::has_unsigned_flag(c.get_flag()));
        let schema = new_schema(vec![int_col(1, 0, unsigned), int_col(2, 1, false)]);
        TableReaderExecutor {
            ctx: new_table_reader_executor_context(sctx).unwrap(),
            base: new_base_executor_v2(
                sctx.get_session_vars(),
                &sctx.sql_killer(),
                Some(schema),
                1,
                Vec::new(),
            ),
            index_usage_reporter: None,
            table: Some(tbl),
            kv_range_builder: None,
            ranges,
            grouped_ranges: Vec::new(),
            group_by_col_idxs: Vec::new(),
            kv_ranges: Vec::new(),
            dag_pb: tipb::DagRequest::default(),
            start_ts: 42,
            txn_scope: "global".to_owned(),
            read_replica_scope: "global".to_owned(),
            is_staleness: false,
            net_data_size: 0.0,
            columns: Vec::new(),
            result_handler: None,
            plans: Vec::new(),
            table_plan: None,
            mem_tracker: None,
            select_result_hook: hook,
            keep_order,
            desc,
            by_items: Vec::new(),
            paging: false,
            store_type: TiKV,
            cor_col_in_filter: false,
            cor_col_in_access: false,
            virtual_column_index: Vec::new(),
            virtual_column_ret_field_types: Vec::new(),
            batch_cop: false,
            dummy: false,
        }
    }

    #[tokio::test]
    async fn drop_without_close_closes_both_split_select_results() {
        let sctx = MockSession::new(10);
        let ctx = ExecCtx::new(sctx.vars.stmt_ctx_arc());
        let tbl = mock_table_from_meta(test_table_info(10, true)).unwrap();
        let range = Range {
            low_val: vec![new_uint_datum(0)],
            high_val: vec![new_uint_datum(u64::MAX)],
            ..Default::default()
        };
        let (sr1, closed1, calls1) = TrackedMockSelectResult::new(vec![vec![1, 10]]);
        let (sr2, closed2, calls2) = TrackedMockSelectResult::new(vec![vec![2, 20]]);
        let results = Arc::new(parking_lot::Mutex::new(vec![sr1, sr2]));
        let hook = SelectResultHook {
            select_result_func: Some(Arc::new(move |_, _, _, _, _| {
                let sr = results.lock().remove(0);
                Box::pin(async move { Ok(Box::new(sr) as Box<dyn SelectResult>) })
            })),
        };
        let mut reader = bare_reader(&sctx, tbl, vec![range], true, false, hook);
        reader.open(&ctx).await.unwrap();
        assert!(!closed1.load(Ordering::SeqCst));
        assert!(!closed2.load(Ordering::SeqCst));
        // Drop without calling close(): TableResultHandler::drop must close both results.
        drop(reader);
        assert!(closed1.load(Ordering::SeqCst));
        assert!(closed2.load(Ordering::SeqCst));
        assert_eq!(calls1.load(Ordering::SeqCst), 1);
        assert_eq!(calls2.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn open_error_on_second_split_closes_first_result() {
        let sctx = MockSession::new(10);
        let ctx = ExecCtx::new(sctx.vars.stmt_ctx_arc());
        let tbl = mock_table_from_meta(test_table_info(10, true)).unwrap();
        let range = Range {
            low_val: vec![new_uint_datum(0)],
            high_val: vec![new_uint_datum(u64::MAX)],
            ..Default::default()
        };
        let (sr1, closed1, _) = TrackedMockSelectResult::new(vec![vec![1, 10]]);
        let sr1_slot = Arc::new(parking_lot::Mutex::new(Some(sr1)));
        let hook = SelectResultHook {
            select_result_func: Some(Arc::new(move |_, _, _, _, _| {
                let maybe = sr1_slot.lock().take();
                Box::pin(async move {
                    match maybe {
                        Some(sr) => Ok(Box::new(sr) as Box<dyn SelectResult>),
                        None => Err(Error::plain("second split failed")),
                    }
                })
            })),
        };
        let mut reader = bare_reader(&sctx, tbl, vec![range], true, false, hook);
        let err = reader.open(&ctx).await.unwrap_err();
        assert!(err.to_string().contains("second split failed"));
        assert!(
            closed1.load(Ordering::SeqCst),
            "first SelectResult must be closed when second split fails"
        );
    }

    #[tokio::test]
    async fn open_error_on_kth_grouped_request_closes_previous_results() {
        let sctx = MockSession::new(10);
        let ctx = ExecCtx::new(sctx.vars.stmt_ctx_arc());
        let tbl = mock_table_from_meta(test_table_info(10, false)).unwrap();
        let (sr1, closed1, _) = TrackedMockSelectResult::new(vec![vec![1, 10]]);
        let sr1_slot = Arc::new(parking_lot::Mutex::new(Some(sr1)));
        let hook = SelectResultHook {
            select_result_func: Some(Arc::new(move |_, _, _, _, _| {
                let maybe = sr1_slot.lock().take();
                Box::pin(async move {
                    match maybe {
                        Some(sr) => Ok(Box::new(sr) as Box<dyn SelectResult>),
                        None => Err(Error::plain("kth request failed")),
                    }
                })
            })),
        };
        let mut reader = bare_reader(&sctx, tbl, Vec::new(), true, false, hook);
        reader.grouped_ranges = vec![
            vec![Range {
                low_val: vec![new_int_datum(1)],
                high_val: vec![new_int_datum(5)],
                ..Default::default()
            }],
            vec![Range {
                low_val: vec![new_int_datum(10)],
                high_val: vec![new_int_datum(15)],
                ..Default::default()
            }],
        ];
        reader.by_items = vec![ByItems {
            expr: Expr::Column((*int_col(1, 0, false)).clone()).into_ref(),
            desc: false,
        }];
        let err = reader.open(&ctx).await.unwrap_err();
        assert!(err.to_string().contains("kth request failed"));
        assert!(
            closed1.load(Ordering::SeqCst),
            "previously opened SelectResult must be closed on kth failure"
        );
    }

    #[tokio::test]
    async fn close_all_returns_first_error_and_closes_remaining() {
        let (mut sr1, closed1, _) = TrackedMockSelectResult::new(vec![]);
        let (mut sr2, closed2, _) = TrackedMockSelectResult::new(vec![]);
        sr1.close_err = Some("first close err".to_owned());
        sr2.close_err = Some("second close err".to_owned());
        let mut rh = TableResultHandler::default();
        rh.open(
            Some(Box::new(sr1) as Box<dyn SelectResult>),
            Some(Box::new(sr2) as Box<dyn SelectResult>),
        );
        let err = rh.close().unwrap_err();
        assert_eq!(err.to_string(), "first close err");
        assert!(closed1.load(Ordering::SeqCst));
        assert!(closed2.load(Ordering::SeqCst));
        // Second close is idempotent.
        assert!(rh.close().is_ok());
    }

    #[tokio::test]
    async fn next_raw_concatenates_optional_and_main_streams() {
        let (mut sr1, _, _) = TrackedMockSelectResult::new(vec![]);
        let (mut sr2, _, _) = TrackedMockSelectResult::new(vec![]);
        sr1.raw_batches = vec![b"a".to_vec(), b"b".to_vec()];
        sr2.raw_batches = vec![b"c".to_vec()];
        let mut rh = TableResultHandler::default();
        rh.open(
            Some(Box::new(sr1) as Box<dyn SelectResult>),
            Some(Box::new(sr2) as Box<dyn SelectResult>),
        );
        let kv_ctx = tidb_kv::Context::background();
        assert_eq!(rh.next_raw(&kv_ctx).await.unwrap(), Some(b"a".to_vec()));
        assert_eq!(rh.next_raw(&kv_ctx).await.unwrap(), Some(b"b".to_vec()));
        assert_eq!(rh.next_raw(&kv_ctx).await.unwrap(), Some(b"c".to_vec()));
        assert_eq!(rh.next_raw(&kv_ctx).await.unwrap(), None);
        rh.close().unwrap();
    }

    #[test]
    fn closest_read_adjuster_threshold_and_label_behavior() {
        let dctx = Arc::new(DistSQLContext {
            replica_closest_read_threshold: 100,
            ..Default::default()
        });

        // Non-adaptive replica read returns None.
        let leader_req = Request {
            replica_read: ReplicaReadLeader,
            ..Default::default()
        };
        assert!(new_closest_read_adjuster(&dctx, &leader_req, 500.0).is_none());

        // Adaptive: at or above threshold -> true + appends zone label.
        let mut req = Request {
            replica_read: ReplicaReadClosestAdaptive,
            ..Default::default()
        };
        let adj = new_closest_read_adjuster(&dctx, &req, 300.0).unwrap();
        assert!(adj(&mut req, 3));
        assert_eq!(req.replica_read, ReplicaReadClosestAdaptive);
        assert_eq!(req.match_store_labels.len(), 1);
        assert_eq!(req.match_store_labels[0].key, "zone");

        // Adaptive: below threshold (299.9 / 3 = 99.966 -> int64 truncates to 99 < 100) -> false + Leader.
        let mut req2 = Request {
            replica_read: ReplicaReadClosestAdaptive,
            ..Default::default()
        };
        let adj2 = new_closest_read_adjuster(&dctx, &req2, 299.9).unwrap();
        assert!(!adj2(&mut req2, 3));
        assert_eq!(req2.replica_read, ReplicaReadLeader);
        assert!(req2.match_store_labels.is_empty());
    }

    #[tokio::test]
    async fn build_table_reader_pre_open_fallback_guards_keep_stmt_ctx_clean() {
        let tbl = test_table_info(42, false);
        let is = info_schema_with_table(Arc::clone(&tbl)).await;

        // 1. TiFlash / MPP -> DEF.MPP
        {
            let mut s = MockSession::new(10);
            s.info_schema = Some(Arc::clone(&is));
            let mut plan = make_table_reader_plan(Arc::clone(&tbl), vec![], false, false);
            plan.store_type = TiFlash;
            let plan_ref: Plan = Arc::new(plan);
            let mut b = new_executor_builder(
                &s,
                Some(Arc::clone(&is)),
                Arc::new(TelemetryInfo::default()),
            );
            assert!(b.build(&mut s, Some(&plan_ref)).await.is_none());
            assert_eq!(blocked_by(b.err.as_ref().unwrap()), Some("DEF.MPP"));
            assert!(s.vars.stmt_ctx().table_ids.is_empty());
            assert!(!s.vars.stmt_ctx().is_tikv.load(Ordering::SeqCst));
        }

        // 2. BatchCop -> DEF.MPP
        {
            let mut s = MockSession::new(10);
            let mut plan = make_table_reader_plan(Arc::clone(&tbl), vec![], false, false);
            plan.read_req_type = BatchCop;
            let plan_ref: Plan = Arc::new(plan);
            let mut b = new_executor_builder(
                &s,
                Some(Arc::clone(&is)),
                Arc::new(TelemetryInfo::default()),
            );
            assert!(b.build(&mut s, Some(&plan_ref)).await.is_none());
            assert_eq!(blocked_by(b.err.as_ref().unwrap()), Some("DEF.MPP"));
        }

        // 3. Static partition -> D06.4
        {
            let mut s = MockSession::new(10);
            let mut plan = make_table_reader_plan(Arc::clone(&tbl), vec![], false, false);
            let mut scan = plan.get_table_scan().unwrap().clone();
            scan.is_partition = true;
            scan.physical_table_id = 1001;
            let scan_ref: PhysicalPlanRef = Arc::new(scan);
            plan.table_plan = Some(Arc::clone(&scan_ref));
            plan.table_plans = vec![scan_ref];
            let plan_ref: Plan = Arc::new(plan);
            let mut b = new_executor_builder(
                &s,
                Some(Arc::clone(&is)),
                Arc::new(TelemetryInfo::default()),
            );
            assert!(b.build(&mut s, Some(&plan_ref)).await.is_none());
            assert_eq!(blocked_by(b.err.as_ref().unwrap()), Some("D06.4"));
            assert!(s.vars.stmt_ctx().table_ids.is_empty());
        }

        // 4. Dynamic partition -> D09.12
        {
            let mut s = MockSession::new(10);
            s.vars.stmt_ctx_mut().use_dynamic_prune_mode = true;
            let mut ptbl = (*tbl).clone();
            ptbl.partition = Some(Box::new(PartitionInfo {
                enable: true,
                ..Default::default()
            }));
            let ptbl = Arc::new(ptbl);
            let plan = make_table_reader_plan(ptbl, vec![], false, false);
            let plan_ref: Plan = Arc::new(plan);
            let mut b = new_executor_builder(
                &s,
                Some(Arc::clone(&is)),
                Arc::new(TelemetryInfo::default()),
            );
            assert!(b.build(&mut s, Some(&plan_ref)).await.is_none());
            assert_eq!(blocked_by(b.err.as_ref().unwrap()), Some("D09.12"));
            assert!(s.vars.stmt_ctx().table_ids.is_empty());
        }

        // 5. Correlated column in access condition -> D10.1
        {
            let mut s = MockSession::new(10);
            let mut plan = make_table_reader_plan(Arc::clone(&tbl), vec![], false, false);
            let mut scan = plan.get_table_scan().unwrap().clone();
            scan.access_condition = vec![
                Expr::CorrelatedColumn(CorrelatedColumn {
                    column: (*int_col(1, 0, false)).clone(),
                    slot: tidb_expression::CorrSlot(0),
                })
                .into_ref(),
            ];
            let scan_ref: PhysicalPlanRef = Arc::new(scan);
            plan.table_plan = Some(Arc::clone(&scan_ref));
            plan.table_plans = vec![scan_ref];
            let plan_ref: Plan = Arc::new(plan);
            let mut b = new_executor_builder(
                &s,
                Some(Arc::clone(&is)),
                Arc::new(TelemetryInfo::default()),
            );
            assert!(b.build(&mut s, Some(&plan_ref)).await.is_none());
            assert_eq!(blocked_by(b.err.as_ref().unwrap()), Some("D10.1"));
        }

        // 6. Multi-group grouped_ranges -> D07.3
        {
            let mut s = MockSession::new(10);
            let mut plan = make_table_reader_plan(Arc::clone(&tbl), vec![], true, false);
            let mut scan = plan.get_table_scan().unwrap().clone();
            scan.grouped_ranges = vec![vec![], vec![]];
            let scan_ref: PhysicalPlanRef = Arc::new(scan);
            plan.table_plan = Some(Arc::clone(&scan_ref));
            plan.table_plans = vec![scan_ref];
            let plan_ref: Plan = Arc::new(plan);
            let mut b = new_executor_builder(
                &s,
                Some(Arc::clone(&is)),
                Arc::new(TelemetryInfo::default()),
            );
            assert!(b.build(&mut s, Some(&plan_ref)).await.is_none());
            assert_eq!(blocked_by(b.err.as_ref().unwrap()), Some("D07.3"));
        }

        // 7. Cached table -> D06.4
        {
            let mut s = MockSession::new(10);
            let mut ctbl = (*tbl).clone();
            ctbl.table_cache_status_type = TableCacheStatusEnable;
            let ctbl = Arc::new(ctbl);
            let cis = info_schema_with_table(Arc::clone(&ctbl)).await;
            let plan = make_table_reader_plan(ctbl, vec![], false, false);
            let plan_ref: Plan = Arc::new(plan);
            let mut b = new_executor_builder(&s, Some(cis), Arc::new(TelemetryInfo::default()));
            assert!(b.build(&mut s, Some(&plan_ref)).await.is_none());
            assert_eq!(blocked_by(b.err.as_ref().unwrap()), Some("D06.4"));
            assert!(s.vars.stmt_ctx().table_ids.is_empty());
        }
    }

    #[tokio::test]
    async fn helpers_dummy_mem_usage_detach_ddl_owner_partition_id_and_net_data_size() {
        let mut s = MockSession::new(10);
        let mut tbl_info = (*test_table_info(77, false)).clone();
        tbl_info.temp_table_type = TempTableLocal;
        let tbl_info = Arc::new(tbl_info);
        let is = info_schema_with_table(Arc::clone(&tbl_info)).await;
        s.info_schema = Some(Arc::clone(&is));

        let plan = make_table_reader_plan(
            Arc::clone(&tbl_info),
            vec![Range {
                low_val: vec![new_int_datum(1)],
                high_val: vec![new_int_datum(10)],
                ..Default::default()
            }],
            false,
            false,
        );
        // Verify GetNetDataSize and GetAvgRowSize on PhysicalTableReader (D10.1 / D10.5).
        let expr_ctx = s.get_expr_ctx();
        let avg_row_size = {
            let pctx = tidb_planner::planctx::PlanCtx::new(&mut s.vars, expr_ctx);
            plan.get_avg_row_size(&pctx)
        };
        assert!(avg_row_size > 0.0);
        assert!((plan.get_net_data_size(&s.vars) - 100.0 * avg_row_size).abs() < 1e-9);

        let mut b = new_executor_builder(
            &s,
            Some(Arc::clone(&is)),
            Arc::new(TelemetryInfo::default()),
        );
        let mut reader = crate::builder::build_no_range_table_reader(&mut b, &mut s, &plan)
            .await
            .unwrap();
        assert!(reader.dummy);
        assert!(reader.table().is_some());
        reader.ranges = plan.get_table_scan().unwrap().ranges.clone();
        assert!(reader.mem_usage() > 0);

        // Detach produces an independent usable executor.
        let mut detached = reader.detach().expect("TableReaderExecutor detaches");
        let ctx = ExecCtx::new(s.vars.stmt_ctx_arc());
        reader.open(&ctx).await.unwrap();
        assert!(!reader.kv_ranges.is_empty());
        let mut chk = exec::new_first_chunk(&reader);
        reader.next(&ctx, &mut chk).await.unwrap();
        assert_eq!(chk.num_rows(), 0);
        reader.close().await.unwrap();
        assert!(reader.kv_ranges.is_empty());

        detached.open(&ctx).await.unwrap();
        detached.close().await.unwrap();

        // GetDDLOwner default error and custom callback.
        let kv_ctx = tidb_kv::Context::background();
        assert_eq!(
            reader.ctx.get_ddl_owner(&kv_ctx).unwrap_err().to_string(),
            "GetDDLOwner in a context without DDL"
        );
        reader.ctx.get_ddl_owner = Some(Arc::new(|_| Ok(999)));
        assert_eq!(reader.ctx.get_ddl_owner(&kv_ctx).unwrap(), 999);

        // set_dummy and get_physical_plan_ids.
        reader.dummy = false;
        reader.set_dummy();
        assert!(reader.dummy);
        assert_eq!(get_physical_plan_ids(&reader.plans), vec![2]);

        // UpdateExecutorTableID recursive traversal and out-of-range TypeJoin check.
        let mut scan_pb = tipb::Executor::default();
        scan_pb.set_tp(tipb::ExecType::TypeTableScan);
        scan_pb.mut_tbl_scan().set_table_id(1);
        let mut sel_pb = tipb::Executor::default();
        sel_pb.set_tp(tipb::ExecType::TypeSelection);
        sel_pb.mut_selection().set_child(scan_pb);
        update_executor_table_id(&kv_ctx, Some(&mut sel_pb), true, &[555]).unwrap();
        assert_eq!(
            sel_pb
                .get_selection()
                .get_child()
                .get_tbl_scan()
                .get_table_id(),
            555
        );
        let mut join_pb = tipb::Executor::default();
        join_pb.set_tp(tipb::ExecType::TypeJoin);
        join_pb.mut_join().set_inner_idx(0);
        // probe_idx = 1, but join has 0 children -> error (Go panics on slice out of bounds).
        assert!(update_executor_table_id(&kv_ctx, Some(&mut join_pb), true, &[555]).is_err());

        // KvRangeBuilderFromRangeAndPartition.
        struct DummyPhysTable(TableRef, i64);
        impl tidb_infoschema::table::Table for DummyPhysTable {
            fn cols(&self) -> &[tidb_infoschema::table::ColumnRef] {
                self.0.cols()
            }
            fn visible_cols(&self) -> &[tidb_infoschema::table::ColumnRef] {
                self.0.visible_cols()
            }
            fn hidden_cols(&self) -> &[tidb_infoschema::table::ColumnRef] {
                self.0.hidden_cols()
            }
            fn writable_cols(&self) -> &[tidb_infoschema::table::ColumnRef] {
                self.0.writable_cols()
            }
            fn deletable_cols(&self) -> &[tidb_infoschema::table::ColumnRef] {
                self.0.deletable_cols()
            }
            fn full_hidden_cols_and_visible_cols(&self) -> &[tidb_infoschema::table::ColumnRef] {
                self.0.full_hidden_cols_and_visible_cols()
            }
            fn indices(&self) -> &[tidb_infoschema::table::IndexRef] {
                self.0.indices()
            }
            fn deletable_indices(&self) -> &[tidb_infoschema::table::IndexRef] {
                self.0.deletable_indices()
            }
            fn record_prefix(&self) -> &tidb_kv::key::Key {
                self.0.record_prefix()
            }
            fn index_prefix(&self) -> &tidb_kv::key::Key {
                self.0.index_prefix()
            }
            fn allocators(&self) -> tidb_infoschema::autoid::Allocators {
                self.0.allocators()
            }
            fn meta(&self) -> &Arc<TableInfo> {
                self.0.meta()
            }
            fn type_(&self) -> tidb_infoschema::table::Type {
                self.0.type_()
            }
            fn get_partitioned_table(
                &self,
            ) -> Option<&dyn tidb_infoschema::table::PartitionedTable> {
                None
            }
            fn as_physical_table(&self) -> Option<&dyn PhysicalTable> {
                Some(self)
            }
            fn as_cached_table(&self) -> Option<&dyn tidb_infoschema::table::CachedTable> {
                None
            }
            fn mem_size(&self) -> u64 {
                self.0.mem_size()
            }
            fn as_any(&self) -> &dyn std::any::Any {
                self
            }
        }
        impl PhysicalTable for DummyPhysTable {
            fn get_physical_id(&self) -> i64 {
                self.1
            }
        }
        let base_tbl = mock_table_from_meta(test_table_info(100, false)).unwrap();
        let krb = KvRangeBuilderFromRangeAndPartition {
            partitions: vec![
                Arc::new(DummyPhysTable(Arc::clone(&base_tbl), 101)),
                Arc::new(DummyPhysTable(Arc::clone(&base_tbl), 102)),
            ],
        };
        let dctx = DistSQLContext::default();
        let ranges = vec![Range {
            low_val: vec![new_int_datum(1)],
            high_val: vec![new_int_datum(5)],
            ..Default::default()
        }];
        let (pids, sep) = krb.build_key_range_separately(&dctx, &ranges).unwrap();
        assert_eq!(pids, vec![101, 102]);
        assert_eq!(sep.len(), 2);
        let merged = krb.build_key_range(&dctx, &ranges).unwrap();
        assert_eq!(merged.len(), 2);
        assert_eq!(merged, sep);

        // corColInDistPlan covers Selection, Projection, TopN, TableScan.
        let cor_expr = Expr::CorrelatedColumn(CorrelatedColumn {
            column: (*int_col(1, 0, false)).clone(),
            slot: tidb_expression::CorrSlot(0),
        })
        .into_ref();
        let sel: Plan = Arc::new(PhysicalSelection {
            conditions: vec![Arc::clone(&cor_expr)],
            ..Default::default()
        });
        assert!(b.cor_col_in_dist_plan(&[sel]));
        let proj: Plan = Arc::new(PhysicalProjection {
            exprs: vec![Arc::clone(&cor_expr)],
            ..Default::default()
        });
        assert!(b.cor_col_in_dist_plan(&[proj]));
        let topn: Plan = Arc::new(PhysicalTopN {
            by_items: vec![ByItems {
                expr: Arc::clone(&cor_expr),
                desc: false,
            }],
            ..Default::default()
        });
        assert!(b.cor_col_in_dist_plan(&[topn]));
        let scan_lm = PhysicalTableScan {
            late_materialization_filter_condition: vec![cor_expr],
            ..Default::default()
        };
        assert!(b.cor_col_in_dist_plan(&[Arc::new(scan_lm)]));
    }

    async fn run_pipeline_with_kill_check() {
        let sctx = MockSession::new(10);
        let ctx = ExecCtx::new(sctx.vars.stmt_ctx_arc());
        let tbl = mock_table_from_meta(test_table_info(10, false)).unwrap();
        let rows: Vec<Vec<i64>> = vec![
            vec![5, 50],
            vec![1, 10],
            vec![4, 40],
            vec![2, 20],
            vec![3, 30],
        ];
        let (sr, closed, _) = TrackedMockSelectResult::new(rows);
        let slot = Arc::new(parking_lot::Mutex::new(Some(sr)));
        let hook = SelectResultHook {
            select_result_func: Some(Arc::new(move |_, _, _, _, _| {
                let sr = slot.lock().take().unwrap();
                Box::pin(async move { Ok(Box::new(sr) as Box<dyn SelectResult>) })
            })),
        };
        let reader = bare_reader(&sctx, tbl, vec![], false, false, hook);
        let schema = reader.base.meta.schema();
        // Sort DESC on column 0, then Limit 3.
        let sort = crate::sortexec::sort::new_sort_exec(
            crate::internal::exec::executor::new_base_executor(
                sctx.get_session_vars(),
                &sctx.sql_killer(),
                Some(schema.clone()),
                2,
                vec![Box::new(reader)],
            ),
            vec![ByItems {
                expr: Expr::Column((*schema.columns[0]).clone()).into_ref(),
                desc: true,
            }],
            schema.clone(),
            sctx.get_expr_ctx(),
            sctx.vars.state.mem_tracker_bytes_limit,
            sctx.vars.executor_concurrency,
        );
        let mut limit = crate::select::LimitExec {
            base: crate::internal::exec::executor::new_base_executor(
                sctx.get_session_vars(),
                &sctx.sql_killer(),
                Some(schema),
                3,
                vec![Box::new(sort)],
            ),
            begin: 0,
            end: 3,
            cursor: 0,
            meet_first_batch: false,
            child_result: None,
            column_idxs_used_by_child: None,
            column_swap_helper: None,
        };
        limit.open(&ctx).await.unwrap();
        let mut chk = exec::new_first_chunk(&limit);
        let mut got = Vec::new();
        loop {
            exec::next(&ctx, &mut limit, &mut chk).await.unwrap();
            if chk.num_rows() == 0 {
                break;
            }
            for i in 0..chk.num_rows() {
                got.push(chk.get_row(i).get_int64(0));
            }
        }
        assert_eq!(got, vec![5, 4, 3]);
        limit.close().await.unwrap();
        assert!(closed.load(Ordering::SeqCst));

        // Verify SQLKiller triggers error via exec::next.
        let (sr_kill, _, _) = TrackedMockSelectResult::new(vec![vec![1, 10]]);
        let slot_k = Arc::new(parking_lot::Mutex::new(Some(sr_kill)));
        let hook_k = SelectResultHook {
            select_result_func: Some(Arc::new(move |_, _, _, _, _| {
                let sr = slot_k.lock().take().unwrap();
                Box::pin(async move { Ok(Box::new(sr) as Box<dyn SelectResult>) })
            })),
        };
        let tbl2 = mock_table_from_meta(test_table_info(10, false)).unwrap();
        let mut reader2 = bare_reader(&sctx, tbl2, vec![], false, false, hook_k);
        reader2.open(&ctx).await.unwrap();
        sctx.killer
            .send_kill_signal(tidb_memory::sqlkiller::QueryInterrupted);
        let mut chk2 = exec::new_first_chunk(&reader2);
        let err = exec::next(&ctx, &mut reader2, &mut chk2).await.unwrap_err();
        assert_eq!(err.code(), tidb_base::errno::ErrQueryInterrupted as isize);
        reader2.close().await.unwrap();
        let _ = (PhysicalLimit::default(), PhysicalSort::default());
    }

    #[tokio::test]
    async fn missing_table_in_infoschema_and_none_table_in_build_kv_req_error() {
        let tbl = test_table_info(42, false);
        let plan = make_table_reader_plan(Arc::clone(&tbl), vec![], false, false);
        let plan_ref: Plan = Arc::new(plan);

        // 1. Builder without an InfoSchema fails instead of silently reading table id 0.
        {
            let mut s = MockSession::new(10);
            let mut b = new_executor_builder(&s, None, Arc::new(TelemetryInfo::default()));
            assert!(b.build(&mut s, Some(&plan_ref)).await.is_none());
            let err = b.err.as_ref().expect("expected error when b.is is None");
            assert!(
                err.to_string()
                    .contains("table id 42 not found in infoschema"),
                "unexpected error: {err}"
            );
            assert!(s.vars.stmt_ctx().table_ids.is_empty());
        }

        // 2. Builder whose InfoSchema cannot resolve table_id (e.g. lazy-load miss) fails.
        {
            let mut s = MockSession::new(10);
            let other_tbl = test_table_info(99, false);
            let is = info_schema_with_table(other_tbl).await;
            let mut b = new_executor_builder(&s, Some(is), Arc::new(TelemetryInfo::default()));
            assert!(b.build(&mut s, Some(&plan_ref)).await.is_none());
            let err = b
                .err
                .as_ref()
                .expect("expected error when table_by_id returns None");
            assert!(
                err.to_string()
                    .contains("table id 42 not found in infoschema"),
                "unexpected error: {err}"
            );
            assert!(s.vars.stmt_ctx().table_ids.is_empty());
        }

        // 3. Direct build_kv_req with table == None returns an error (no table id 0 fallback).
        {
            let s = MockSession::new(10);
            let tbl_ref = mock_table_from_meta(tbl).unwrap();
            let mut reader = bare_reader(
                &s,
                tbl_ref,
                vec![],
                false,
                false,
                SelectResultHook::default(),
            );
            reader.table = None;
            let kv_ctx = tidb_kv::Context::background();
            let err = reader
                .build_kv_req(&kv_ctx, &[])
                .err()
                .expect("expected error when table is None");
            assert!(
                err.to_string().contains("table is None"),
                "unexpected error: {err}"
            );
        }
    }

    #[tokio::test]
    async fn table_reader_pipeline_and_kill_single_thread() {
        run_pipeline_with_kill_check().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn table_reader_pipeline_and_kill_multi_thread() {
        run_pipeline_with_kill_check().await;
    }
}
