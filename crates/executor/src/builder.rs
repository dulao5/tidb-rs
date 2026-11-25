//! go-file: pkg/executor/builder.go
//!
//! The executor builder: a physical plan to an executor tree. Part VS1-13 has the builder
//! itself and its dispatch (`build`); each `buildXxx` comes with its executor (VS1-12 point
//! get, VS1-34 table reader, VS1-35 selection / projection / limit / table dual, VS1-36 sort
//! / top-n) and is a boundary stub until then. VS1-36 adds `buildSort` / `buildTopN`.
//!
//! VS1-12 adds the point get (`buildPointGet` is in [`crate::point_get`], as in Go) and the
//! helpers of this file it uses: `NewRowDecoder`, the snapshot (`getSnapshot`,
//! `getSnapshotTS`), the temporary / cached table checks, the index usage reporter, the
//! replica read adjusters, `buildHandleColsForExec`.
//!
//! `build` is async: building a reader takes the statement's snapshot, and the transaction
//! provider may have to wait for the TSO to give it (`GetSnapshotWithStmtReadTS`); Go blocks
//! the goroutine there, here the session's task awaits (N3).

// Go identifiers for package constants (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use std::collections::HashMap;
use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_base::config;
use tidb_chunk::chunk::{self, Chunk};
use tidb_chunk::chunk_util::new_column_swap_helper;
use tidb_expression::aggregation::{AggFuncDesc, AggFunctionMode, is_all_first_row};
use tidb_expression::eval::RpnState;
use tidb_expression::evaluator::new_evaluator_suite;
use tidb_expression::expression::EvalState;
use tidb_expression::{Column, Expr, ExprRef, Schema};
use tidb_infoschema::table::column::get_col_origin_default_value;
use tidb_infoschema::tables::{find_primary_index, try_get_common_pk_column_ids};
use tidb_kv::kv::Snapshot;
use tidb_kv::option::{self as kvopt, ReplicaReadLeader, ReplicaReadMixed, ReplicaReadType};
use tidb_kv::txn_scope_var::GlobalTxnScope;
use tidb_model::{
    ColumnInfo, ExtraHandleID, TableCacheStatusDisable, TableInfo, TempTableLocal, TempTableNone,
};
use tidb_planner::PhysicalKind;
use tidb_planner::base::PhysicalPlan;
use tidb_planner::core::physical_plans::{
    PhysicalLimit, PhysicalProjection, PhysicalSelection, PhysicalTableDual,
};
use tidb_planner::util::handle_cols::{HandleCols, new_common_handle_cols, new_int_handle_cols};
use tidb_sessionctx::exprctx::ExprContextRef;
use tidb_tablecodec::rowcodec::decoder::{ChunkDecoder, ColInfo, DefDatumFn, new_chunk_decoder};
use tidb_types::field_type::new_field_type;
use tikv_client::proto::metapb;

use crate::adapter::TelemetryInfo;
use crate::glue::{InfoSchemaRef, Plan, SessionContext};
use crate::internal::exec::executor::{
    self as exec, Executor, new_base_executor, new_base_executor_v2,
};
use crate::internal::exec::indexusage::IndexUsageReporter;
use crate::projection::{ProjectionExec, new_projection_executor_context};
use crate::select::{LimitExec, SelectionExec, TableDualExec, new_selection_executor_context};
use tidb_planner::core::physical_plans::{PhysicalSort, PhysicalTopN};
use tidb_planner::core::point_get_plan::PointGetPlan;
use tidb_planner::util::byitem::ByItems;

use crate::aggfuncs;
use crate::aggregate::agg_hash_executor::new_hash_agg_exec;
use crate::aggregate::agg_stream_executor::new_stream_agg_exec;
use crate::aggregate::{HashAggExec, StreamAggExec};
use crate::internal::vecgroupchecker::vec_group_checker::new_vec_group_checker;
use crate::sortexec::SortExec;
use crate::sortexec::sort::new_sort_exec;
use crate::sortexec::topn::new_top_n_exec;

/// `placement.DCLabelKey` (`pkg/ddl/placement/common.go`): the store label of a zone.
const DCLabelKey: &str = "zone";

/// executorBuilder builds an Executor from a Plan.
/// The InfoSchema must not change during execution.
///
/// Go keeps the session (`sctx`) in the builder; here the build functions take it (the
/// builder lives for one `build` call on the session's task).
pub struct ExecutorBuilder {
    pub is: Option<InfoSchemaRef>,
    /// err is set when there is error happened during Executor building process.
    pub err: Option<Error>,
    pub has_lock: bool,
    pub ti: Arc<TelemetryInfo>,
    /// isStaleness means whether this statement use stale read.
    pub is_staleness: bool,
    pub txn_scope: String,
    pub read_replica_scope: String,
    pub in_update_stmt: bool,
    pub in_delete_stmt: bool,
    pub in_insert_stmt: bool,
    pub in_select_lock_stmt: bool,
    /// forDataReaderBuilder indicates whether the builder is used by a dataReaderBuilder.
    pub for_data_reader_builder: bool,
    pub data_reader_ts: u64,
    /// Used when building MPPGather.
    pub encounter_union_scan: bool,
}

// go: newExecutorBuilder L1 [M-GOCTX]
pub fn new_executor_builder(
    sctx: &dyn SessionContext,
    is: Option<InfoSchemaRef>,
    ti: Arc<TelemetryInfo>,
) -> ExecutorBuilder {
    ExecutorBuilder {
        is,
        ti,
        is_staleness: sctx.is_stmt_staleness(),
        txn_scope: sctx.get_txn_scope(),
        read_replica_scope: sctx.get_read_replica_scope(),
        err: None,
        has_lock: false,
        in_update_stmt: false,
        in_delete_stmt: false,
        in_insert_stmt: false,
        in_select_lock_stmt: false,
        for_data_reader_builder: false,
        data_reader_ts: 0,
        encounter_union_scan: false,
    }
}

impl ExecutorBuilder {
    /// The executor of `p`; on failure `b.err` is set and `None` returned, as Go.
    ///
    /// Plans other than the VS1 operators have no kind yet (`PhysicalKind` grows with the
    /// operators); in Go they reach the `default` branch's error the same way.
    ///
    /// The executors with children (VS1-34…VS1-36) build them recursively: such a call is
    /// boxed (`Box::pin(self.build(..))`), an async fn cannot recurse unboxed.
    // go: executorBuilder.build L1
    pub async fn build(
        &mut self,
        sctx: &mut dyn SessionContext,
        p: Option<&Plan>,
    ) -> Option<Box<dyn Executor>> {
        // case nil: return nil
        let v = p?;
        // Go's `default` branch first returns a test mock's executor
        // (`testutil.MockPhysicalPlan`), whatever kind the mock reports.
        #[cfg(test)]
        if let Some(mp) = v
            .as_any()
            .downcast_ref::<crate::adapter::tests::MockPhysicalPlan>()
        {
            return Some(mp.get_executor());
        }
        #[cfg(any(test, feature = "test-util"))]
        if let Some(mp) = v
            .as_any()
            .downcast_ref::<crate::testutil::MockPhysicalPlan>()
            && mp.kind == PhysicalKind::TableReader
        {
            return Some(mp.get_executor());
        }
        if v.kind() == PhysicalKind::PointGet {
            // b.buildPointGet(v): `case *plannercore.PointGetPlan`.
            let Some(pg) = v.as_any().downcast_ref::<PointGetPlan>() else {
                // A plan whose kind is PointGet is a PointGetPlan (only a test mock is not).
                self.err = Some(Error::plain(
                    "a PointGet plan that is not a *core.PointGetPlan".to_owned(),
                ));
                return None;
            };
            return self.build_point_get(sctx, pg).await;
        }
        if v.kind() == PhysicalKind::Sort {
            // b.buildSort(v): `case *plannercore.PhysicalSort`.
            let Some(sort) = v.as_any().downcast_ref::<PhysicalSort>() else {
                self.err = Some(Error::plain(
                    "a Sort plan that is not a *core.PhysicalSort".to_owned(),
                ));
                return None;
            };
            return self.build_sort(sctx, sort).await;
        }
        if v.kind() == PhysicalKind::TopN {
            // b.buildTopN(v): `case *plannercore.PhysicalTopN`.
            let Some(top_n) = v.as_any().downcast_ref::<PhysicalTopN>() else {
                self.err = Some(Error::plain(
                    "a TopN plan that is not a *core.PhysicalTopN".to_owned(),
                ));
                return None;
            };
            return self.build_top_n(sctx, top_n).await;
        }
        match v.kind() {
            PhysicalKind::Limit => {
                if let Some(v) = v.as_any().downcast_ref::<PhysicalLimit>() {
                    return self.build_limit(sctx, v).await;
                }
            }
            PhysicalKind::Selection => {
                if let Some(v) = v.as_any().downcast_ref::<PhysicalSelection>() {
                    return self.build_selection(sctx, v).await;
                }
            }
            PhysicalKind::Projection => {
                if let Some(v) = v.as_any().downcast_ref::<PhysicalProjection>() {
                    return self.build_projection(sctx, v).await;
                }
            }
            PhysicalKind::TableDual => {
                if let Some(v) = v.as_any().downcast_ref::<PhysicalTableDual>() {
                    return self.build_table_dual(sctx, v);
                }
            }
            _ => {}
        }
        // glue: stub, blocked-by:D12.1, D12.0 (the buildXxx of each executor)
        let (block, what) = match v.kind() {
            PhysicalKind::PointGet => unreachable!("built above"),
            // A plan of these kinds that is not their physical plan type (only a test mock):
            // Go's type switch takes the `default` branch.
            PhysicalKind::Limit
            | PhysicalKind::Selection
            | PhysicalKind::Projection
            | PhysicalKind::TableDual => {
                self.err = Some(Error::plain(format!(
                    "a {:?} plan of another plan type",
                    v.kind()
                )));
                return None;
            }
            PhysicalKind::Sort | PhysicalKind::TopN => unreachable!("built above"),
            // b.buildTableReader(v)
            PhysicalKind::TableReader => ("D12.1", "the table reader executor (VS1-34)"),
            // A table scan is pushed down inside a reader, never built on its own: Go's
            // `default` branch.
            PhysicalKind::TableScan => {
                self.err = Some(
                    tidb_base::dbterror::exeerrors::ErrUnknownPlan.gen_with_stack(
                        "Unknown Plan %T",
                        vec![tidb_base::gofmt::Arg::Stringer(
                            Vec::new(),
                            "*core.PhysicalTableScan",
                        )],
                    ),
                );
                return None;
            }
            _ => ("D12.0", "an executor for this plan"),
        };
        self.err = Some(blocked(block, what));
        None
    }
}

impl ExecutorBuilder {
    /// The child's executor of a plan with one child (Go `b.build(v.Children()[0])`).
    // glue: the recursive build of an executor's child (boxed: an async fn cannot recurse)
    async fn build_child(
        &mut self,
        sctx: &mut dyn SessionContext,
        v: &dyn PhysicalPlan,
    ) -> Option<Box<dyn Executor>> {
        let child = v.children().first().cloned();
        Box::pin(self.build(sctx, child.as_ref())).await
    }

    // go: executorBuilder.buildLimit L1 [M-INT]
    pub async fn build_limit(
        &mut self,
        sctx: &mut dyn SessionContext,
        v: &PhysicalLimit,
    ) -> Option<Box<dyn Executor>> {
        let child_exec = self.build_child(sctx, v).await;
        if self.err.is_some() {
            return None;
        }
        let vars = sctx.get_session_vars();
        let n = v.count.min(vars.max_chunk_size as u64) as isize;
        let mut base = new_base_executor(
            vars,
            &sctx.sql_killer(),
            Some(v.schema().clone()),
            v.id() as isize,
            child_exec.into_iter().collect(),
        );
        base.base.alloc.set_init_cap(n);
        let mut e = LimitExec {
            base,
            begin: v.offset,
            end: v.offset.wrapping_add(v.count),
            cursor: 0,
            meet_first_batch: false,
            child_result: None,
            column_idxs_used_by_child: None,
            column_swap_helper: None,
        };

        let child_schema = v.children()[0].schema();
        let child_schema_len = child_schema.len();
        let child_used_schema =
            mark_children_used_cols(&v.schema().columns, &[child_schema]).swap_remove(0);
        if child_used_schema.len() == child_schema_len {
            // indicates that all columns are used. LimitExec will improve performance for this condition.
            e.column_idxs_used_by_child = None;
        } else {
            // construct a project evaluator to do the inline projection
            e.column_swap_helper = Some(new_column_swap_helper(&child_used_schema));
            e.column_idxs_used_by_child = Some(child_used_schema);
        }
        Some(Box::new(e))
    }

    // go: executorBuilder.buildSelection L1
    pub async fn build_selection(
        &mut self,
        sctx: &mut dyn SessionContext,
        v: &PhysicalSelection,
    ) -> Option<Box<dyn Executor>> {
        let child_exec = self.build_child(sctx, v).await;
        if self.err.is_some() {
            return None;
        }
        let e = SelectionExec {
            ctx: new_selection_executor_context(&*sctx),
            base: new_base_executor_v2(
                sctx.get_session_vars(),
                &sctx.sql_killer(),
                Some(v.schema().clone()),
                v.id() as isize,
                child_exec.into_iter().collect(),
            ),
            batched: false,
            filters: v.conditions.clone(),
            selected: Vec::new(),
            input_row: None,
            child_result: None,
            mem_tracker: None,
            eval_state: EvalState::new(),
            rpn: RpnState::default(),
        };
        Some(Box::new(e))
    }

    // go: executorBuilder.buildProjection L1
    pub async fn build_projection(
        &mut self,
        sctx: &mut dyn SessionContext,
        v: &PhysicalProjection,
    ) -> Option<Box<dyn Executor>> {
        let child_exec = self.build_child(sctx, v).await;
        if self.err.is_some() {
            return None;
        }
        let vars = sctx.get_session_vars();
        let mut e = ProjectionExec::new(
            new_projection_executor_context(&*sctx),
            new_base_executor_v2(
                vars,
                &sctx.sql_killer(),
                Some(v.schema().clone()),
                v.id() as isize,
                child_exec.into_iter().collect(),
            ),
            vars.projection_concurrency(),
            new_evaluator_suite(&v.exprs, v.avoid_column_evaluator),
            v.calculate_no_delay,
        );

        // If the calculation row count for this Projection operator is smaller
        // than a Chunk size, we turn back to the un-parallel Projection
        // implementation to reduce the goroutine overhead.
        if (v.stats_count() as i64) < vars.max_chunk_size {
            e.num_workers = 0;
        }

        // Use un-parallel projection for query that write on memdb to avoid data race.
        // See also `26832` (upstream issue).
        if self.in_update_stmt || self.in_delete_stmt || self.in_insert_stmt || self.has_lock {
            e.num_workers = 0;
        }
        Some(Box::new(e))
    }

    // go: executorBuilder.buildTableDual L1
    pub fn build_table_dual(
        &mut self,
        sctx: &dyn SessionContext,
        v: &PhysicalTableDual,
    ) -> Option<Box<dyn Executor>> {
        if v.row_count != 0 && v.row_count != 1 {
            self.err = Some(Error::plain(format!(
                "buildTableDual failed, invalid row count for dual table: {}",
                v.row_count
            )));
            return None;
        }
        let mut base = new_base_executor_v2(
            sctx.get_session_vars(),
            &sctx.sql_killer(),
            Some(v.schema().clone()),
            v.id() as isize,
            Vec::new(),
        );
        base.alloc.set_init_cap(v.row_count);
        let e = TableDualExec {
            base,
            num_dual_rows: v.row_count,
            num_returned: 0,
        };
        Some(Box::new(e))
    }
}

/// markChildrenUsedCols compares each child with the output schema, and mark
/// each column of the child is used by output or not.
///
/// Listed with the table reader (D12.1); ported here with its first caller, the limit
/// executor's builder.
// go: markChildrenUsedCols L1
pub fn mark_children_used_cols(
    output_cols: &[Arc<Column>],
    child_schemas: &[&Schema],
) -> Vec<Vec<usize>> {
    let mut children_used = Vec::with_capacity(child_schemas.len());
    let mut marked_offsets: HashMap<isize, usize> = HashMap::new();
    // keep the original maybe reversed order.
    for (original_idx, col) in output_cols.iter().enumerate() {
        marked_offsets.insert(col.index, original_idx);
    }
    let mut prefix_len: isize = 0;
    // for example here.
    // left child schema: [col11]
    // right child schema: [col21, col22]
    // output schema is [col11, col22, col21], if not records the original derived order after physical resolve index.
    // the lused will be [0], the rused will be [0,1], while the actual order is dismissed, [1,0] is correct for rused.
    for child_schema in child_schemas {
        // intPair{first, second}
        let mut used_idx_pair: Vec<(usize, usize)> = Vec::with_capacity(child_schema.columns.len());
        for i in 0..child_schema.columns.len() {
            if let Some(&original_idx) = marked_offsets.get(&(prefix_len + i as isize)) {
                used_idx_pair.push((original_idx, i));
            }
        }
        // sort the used idxes according their original indexes derived after resolveIndex.
        used_idx_pair.sort_by_key(|a| a.0);
        let mut used_idx = Vec::with_capacity(child_schema.columns.len());
        for one in &used_idx_pair {
            used_idx.push(one.1);
        }
        children_used.push(used_idx);
        prefix_len += child_schema.len() as isize;
    }
    children_used
}

impl ExecutorBuilder {
    /// buildSort: a sort executor over the executor of the plan's child.
    ///
    /// Go keeps the session in the executor; here the executor gets the session values it
    /// reads (the expression context, the memory limit). The executor counter
    /// (`executor_metrics.ExecutorCounterSortExec`) is not in the executor's metrics yet (the
    /// metric vectors of D14.1).
    // go: executorBuilder.buildSort L2 (the executor gets the session values it reads; the executor counter is not ported, see the comment)
    async fn build_sort(
        &mut self,
        sctx: &mut dyn SessionContext,
        v: &PhysicalSort,
    ) -> Option<Box<dyn Executor>> {
        use tidb_planner::PhysicalPlan as _;
        let child_exec = Box::pin(self.build(sctx, Some(&v.children()[0]))).await;
        if self.err.is_some() {
            return None;
        }
        let sort_exec = new_sort_exec_of(sctx, v.schema(), v.id(), child_exec, &v.by_items);
        Some(Box::new(sort_exec))
    }

    /// buildTopN: a top-n executor over the executor of the plan's child. See `build_sort` for
    /// the session values and the executor counter (`ExecutorCounterTopNExec`).
    // go: executorBuilder.buildTopN L2 (the executor gets the session values it reads; the executor counter is not ported, see build_sort)
    async fn build_top_n(
        &mut self,
        sctx: &mut dyn SessionContext,
        v: &PhysicalTopN,
    ) -> Option<Box<dyn Executor>> {
        use tidb_planner::PhysicalPlan as _;
        let child_exec = Box::pin(self.build(sctx, Some(&v.children()[0]))).await;
        if self.err.is_some() {
            return None;
        }
        let sort_exec = new_sort_exec_of(sctx, v.schema(), v.id(), child_exec, &v.by_items);
        let mut t = new_top_n_exec(
            sort_exec,
            PhysicalLimit {
                count: v.count,
                offset: v.offset,
                ..PhysicalLimit::default()
            },
            sctx.get_session_vars().executor_concurrency as isize,
        );
        if let Some(prefix_col) = &v.prefix_col {
            t.rank_info.truncate_key_exprs = vec![Expr::Column((**prefix_col).clone()).into_ref()];
            t.rank_info.truncate_key_prefix_char_counts = vec![v.prefix_len];
        }
        Some(Box::new(t))
    }

    /// buildStreamAggFromChildExec: the stream aggregation executor over `child_exec`.
    ///
    /// Go's `buildStreamAgg` (build the child, then this function) is not ported: the
    /// physical stream aggregation plan (`PhysicalStreamAgg`, the physical operators D10.1)
    /// does not exist yet, so no plan kind dispatches to it, and the planner falls back
    /// before an aggregate reaches the builder (the aggregation rules, D09.4); `build`'s
    /// default arm returns `blocked` for any plan it cannot build
    /// (`specs/D12.3-agg-executors/functions.tsv`, its `not_ported` column).
    ///
    /// Go takes the `*PhysicalStreamAgg`; it is not ported (D10.1), so the caller passes the
    /// fields Go reads: the schema, the plan id, the group-by items and the aggregate
    /// functions. `v.IsFinalAgg()` (D10.1) is computed on the aggregate functions as Go does.
    /// The executor gets the session values it reads (N2). An aggregate function that
    /// `aggfuncs.Build` cannot build yet (its temporary `blocked` error, the aggregate
    /// functions D12.4) sets `b.err` and builds nothing; Go's `Build` has no error. The
    /// executor counter (`executor_metrics.ExecutorStreamAggExec`) is not ported (the metric
    /// vectors of D14.1), as `buildSort`'s.
    // go: executorBuilder.buildStreamAggFromChildExec L2 (the plan's fields are passed in, PhysicalStreamAgg is D10.1; Build's temporary blocked error sets b.err; the executor counter is not ported)
    pub fn build_stream_agg_from_child_exec(
        &mut self,
        sctx: &dyn SessionContext,
        child_exec: Option<Box<dyn Executor>>,
        schema: &Schema,
        id: i32,
        group_by_items: &[ExprRef],
        agg_funcs: &[AggFuncDesc],
    ) -> Option<StreamAggExec> {
        let expr_ctx = sctx.get_expr_ctx();
        let killer = sctx.sql_killer();
        let vars = sctx.get_session_vars();
        let child_exec = child_exec.expect("invalid memory address or nil pointer dereference");
        let mut e = new_stream_agg_exec(
            new_base_executor(
                vars,
                &killer,
                Some(schema.clone()),
                id as isize,
                vec![child_exec],
            ),
            new_vec_group_checker(
                expr_ctx.get_eval_ctx(),
                vars.enable_vectorized_expression,
                group_by_items.to_vec(),
            ),
            expr_ctx.get_eval_ctx(),
            vars.track_aggregate_memory_usage,
        );
        e.agg_funcs = Vec::with_capacity(agg_funcs.len());

        if !group_by_items.is_empty() || is_all_first_row(agg_funcs) {
            e.default_val = None;
        } else {
            // Only do this for final agg, see issue `#35295`, `#30923`
            if is_final_agg(agg_funcs) {
                e.default_val = Some(chunk::new(exec::ret_types(&e), 1, 1));
            }
        }
        for (i, agg_desc) in agg_funcs.iter().enumerate() {
            let agg_func = match aggfuncs::build(&*expr_ctx, agg_desc, i as isize) {
                Ok(agg_func) => agg_func,
                Err(err) => {
                    self.err = Some(err);
                    return None;
                }
            };
            e.agg_funcs.push(agg_func);
            if let Some(default_val) = &mut e.default_val {
                let value = agg_desc.get_default_value();
                default_val.append_datum(i, &value);
            }
        }

        Some(e)
    }

    /// buildHashAggFromChildExec: the hash aggregation executor over `child_exec`.
    ///
    /// Go's `buildHashAgg` (build the child, then this function) is not ported yet:
    /// `PhysicalHashAgg` (D10.1) does not exist yet, so no plan kind dispatches to it and
    /// `build`'s default arm already returns `blocked` (same as `buildStreamAgg`, spec §6
    /// item 16). `group_concat`'s shared `truncated` flag wiring (`builder.go:2084-2089`) is
    /// unreachable until D12.4 ports `group_concat`, because `aggfuncs::build` returns
    /// `blocked-by:D12.4` first. The executor counter (`ExecutorCounterHashAggExec`) is not
    /// ported (D14.1 metrics).
    // go: executorBuilder.buildHashAggFromChildExec L2 (plan fields passed in, PhysicalHashAgg is D10.1; Build's temporary blocked error sets b.err; executor counter not ported)
    pub fn build_hash_agg_from_child_exec(
        &mut self,
        sctx: &dyn SessionContext,
        child_exec: Option<Box<dyn Executor>>,
        schema: &Schema,
        id: i32,
        group_by_items: &[ExprRef],
        agg_funcs: &[AggFuncDesc],
    ) -> Option<HashAggExec> {
        let expr_ctx = sctx.get_expr_ctx();
        let killer = sctx.sql_killer();
        let vars = sctx.get_session_vars();
        let child_exec = child_exec.expect("invalid memory address or nil pointer dereference");
        let final_con = vars.hash_agg_final_concurrency();
        let partial_con = vars.hash_agg_partial_concurrency();

        let mut e = new_hash_agg_exec(
            new_base_executor(
                vars,
                &killer,
                Some(schema.clone()),
                id as isize,
                vec![child_exec],
            ),
            group_by_items.to_vec(),
            Arc::clone(&expr_ctx),
            vars.enable_vectorized_expression,
            vars.track_aggregate_memory_usage,
            vars.enable_parallel_hashagg_spill,
            partial_con.max(0) as usize,
            final_con.max(0) as usize,
        );
        e.partial_agg_funcs = Vec::with_capacity(agg_funcs.len());

        if !group_by_items.is_empty() || is_all_first_row(agg_funcs) {
            e.default_val = None;
        } else if is_final_agg(agg_funcs) {
            e.default_val = Some(chunk::new(exec::ret_types(&e), 1, 1));
        }

        for agg_desc in agg_funcs {
            if agg_desc.has_distinct || !agg_desc.order_by_items.is_empty() {
                e.is_unparallel_exec = true;
            }
        }
        // When both tidb_hashagg_final_concurrency and tidb_hashagg_partial_concurrency are
        // 1 (or either is <= 0), execute single-threaded.
        if final_con <= 0 || partial_con <= 0 || (final_con == 1 && partial_con == 1) {
            e.is_unparallel_exec = true;
        }

        let mut partial_ordinal: isize = 0;
        for (i, agg_desc) in agg_funcs.iter().enumerate() {
            if e.is_unparallel_exec {
                let agg_func = match aggfuncs::build(&*expr_ctx, agg_desc, i as isize) {
                    Ok(agg_func) => agg_func.map(Arc::from),
                    Err(err) => {
                        self.err = Some(err);
                        return None;
                    }
                };
                e.partial_agg_funcs.push(agg_func);
            } else {
                let mut ordinal = vec![partial_ordinal];
                partial_ordinal += 1;
                if agg_desc.base.name == tidb_expression::ast_funcs::AggFuncAvg {
                    ordinal.push(partial_ordinal + 1);
                    partial_ordinal += 1;
                }
                let (partial_agg_desc, final_desc) = agg_desc.split(&ordinal);
                let partial_agg_func =
                    match aggfuncs::build(&*expr_ctx, &partial_agg_desc, i as isize) {
                        Ok(agg_func) => agg_func.map(Arc::from),
                        Err(err) => {
                            self.err = Some(err);
                            return None;
                        }
                    };
                let final_agg_func = match aggfuncs::build(&*expr_ctx, &final_desc, i as isize) {
                    Ok(agg_func) => agg_func.map(Arc::from),
                    Err(err) => {
                        self.err = Some(err);
                        return None;
                    }
                };
                e.partial_agg_funcs.push(partial_agg_func);
                e.final_agg_funcs.push(final_agg_func);
            }
            if let Some(default_val) = &mut e.default_val {
                let value = agg_desc.get_default_value();
                default_val.append_datum(i, &value);
            }
        }

        Some(e)
    }
}

/// `v.IsFinalAgg()` of the stream aggregation plan: the plan (D10.1) is not ported, so it is
/// computed on the plan's aggregate functions, as Go's `basePhysicalAgg.IsFinalAgg` does.
// glue: basePhysicalAgg.IsFinalAgg on the plan's aggregate functions (the plan is D10.1)
fn is_final_agg(agg_funcs: &[AggFuncDesc]) -> bool {
    if let Some(first) = agg_funcs.first() {
        return first.mode == AggFunctionMode::FinalMode
            || first.mode == AggFunctionMode::CompleteMode;
    }
    false
}

/// `sortexec.SortExec{BaseExecutor: exec.NewBaseExecutor(b.sctx, v.Schema(), v.ID(),
/// childExec), ByItems: v.ByItems, ExecSchema: v.Schema()}`, shared by `buildSort` and
/// `buildTopN`.
// glue: the SortExec composite literal of buildSort / buildTopN
fn new_sort_exec_of(
    sctx: &dyn SessionContext,
    schema: &Schema,
    id: i32,
    child_exec: Option<Box<dyn Executor>>,
    by_items: &[ByItems],
) -> SortExec {
    let killer = sctx.sql_killer();
    let vars = sctx.get_session_vars();
    let child_exec = child_exec.expect("invalid memory address or nil pointer dereference");
    new_sort_exec(
        new_base_executor(
            vars,
            &killer,
            Some(schema.clone()),
            id as isize,
            vec![child_exec],
        ),
        by_items.to_vec(),
        schema.clone(),
        sctx.get_expr_ctx(),
        vars.state.mem_tracker_bytes_limit,
        vars.executor_concurrency,
    )
}

/// buildHandleColsForExec: the handle columns of an executor reading `allColInfo` of
/// `tblInfo` (the extra handle column last for an int handle).
///
/// Go points `RetType` at the table's `c.FieldType`; the model keeps a column's field type
/// by value, so the expression columns get a shared copy each (built once per executor, not
/// per row).
// go: buildHandleColsForExec L1 [M-SHARE]
pub fn build_handle_cols_for_exec(
    tbl_info: &Arc<TableInfo>,
    all_col_info: &[Arc<ColumnInfo>],
) -> HandleCols {
    if !tbl_info.is_common_handle {
        let extra_col_pos = all_col_info.len() as isize - 1;
        let int_col = Column {
            index: extra_col_pos,
            ret_type: Arc::new(new_field_type(tidb_base::mysql::TypeLonglong)),
            ..Column::default()
        };
        return new_int_handle_cols(Arc::new(int_col));
    }
    let tbl_columns = tbl_info.columns.as_deref().unwrap_or(&[]);
    let mut tbl_cols: Vec<Column> = tbl_columns
        .iter()
        .map(|c| Column {
            ret_type: Arc::new(c.field_type.clone()),
            id: c.id,
            ..Column::default()
        })
        .collect();
    let pk_idx = find_primary_index(tbl_info).expect("a clustered table has a primary index");
    for c in pk_idx.columns.as_deref().unwrap_or(&[]) {
        for (j, col_info) in all_col_info.iter().enumerate() {
            if col_info.name.l == c.name.l {
                tbl_cols[c.offset as usize].index = j as isize;
            }
        }
    }
    let tbl_cols: Vec<Arc<Column>> = tbl_cols.into_iter().map(Arc::new).collect();
    HandleCols::Common(new_common_handle_cols(
        Arc::clone(tbl_info),
        Arc::clone(pk_idx),
        &tbl_cols,
    ))
}

impl ExecutorBuilder {
    /// `getSnapshotTS` returns for-update-ts if in insert/update/delete/lock statement
    /// otherwise the isolation read ts. Please notice that in RC isolation, the above two ts
    /// are the same.
    ///
    /// The for-update TS is the write path's (DML and locking reads, block X): tidb-rs never
    /// builds those executors.
    // go: executorBuilder.getSnapshotTS L2 (the for-update TS of DML / locking reads is blocked-by:X)
    pub async fn get_snapshot_ts(&mut self, sctx: &mut dyn SessionContext) -> Result<u64, Error> {
        if self.for_data_reader_builder {
            return Ok(self.data_reader_ts);
        }

        if self.in_insert_stmt
            || self.in_update_stmt
            || self.in_delete_stmt
            || self.in_select_lock_stmt
        {
            // txnManager.GetStmtForUpdateTS()
            return Err(blocked("X", "the for-update TS of a locking read or DML"));
        }
        sctx.get_stmt_read_ts().await
    }

    /// getSnapshot get the appropriate snapshot from txnManager and set the relevant
    /// snapshot options before return.
    // go: executorBuilder.getSnapshot L2 (the for-update snapshot of DML / locking reads is blocked-by:X)
    pub async fn get_snapshot(
        &mut self,
        sctx: &mut dyn SessionContext,
    ) -> Result<Arc<dyn Snapshot>, Error> {
        let snapshot = if self.in_insert_stmt
            || self.in_update_stmt
            || self.in_delete_stmt
            || self.in_select_lock_stmt
        {
            // txnManager.GetSnapshotWithStmtForUpdateTS()
            return Err(blocked("X", "a for-update snapshot (DML, locking read)"));
        } else {
            sctx.get_snapshot_with_stmt_read_ts().await?
        };

        let sess_vars = sctx.get_session_vars();
        let replica_read_type = sess_vars.get_replica_read();
        snapshot.set_option(
            kvopt::ReadReplicaScope,
            Box::new(self.read_replica_scope.clone()),
        );
        snapshot.set_option(kvopt::TaskID, Box::new(sess_vars.stmt_ctx().task_id));
        snapshot.set_option(
            kvopt::TiKVClientReadTimeout,
            Box::new(sess_vars.get_tikv_client_read_timeout()),
        );
        snapshot.set_option(
            kvopt::ResourceGroupName,
            Box::new(sess_vars.stmt_ctx().resource_group_name.clone()),
        );
        snapshot.set_option(
            kvopt::ExplicitRequestSourceType,
            Box::new(sess_vars.explicit_request_source_type.clone()),
        );

        if replica_read_type.is_closest_read() && self.read_replica_scope != GlobalTxnScope {
            snapshot.set_option(
                kvopt::MatchStoreLabels,
                Box::new(vec![metapb::StoreLabel {
                    key: DCLabelKey.to_owned(),
                    value: self.read_replica_scope.clone(),
                }]),
            );
        }

        Ok(snapshot)
    }

    // go: executorBuilder.buildIndexUsageReporter L1
    pub fn build_index_usage_reporter(
        &self,
        sctx: &dyn SessionContext,
        plan: &PointGetPlan,
    ) -> Option<IndexUsageReporter> {
        build_index_usage_reporter(sctx, plan)
    }

    // go: executorBuilder.validCanReadTemporaryOrCacheTable L1
    pub fn valid_can_read_temporary_or_cache_table(
        &self,
        sctx: &dyn SessionContext,
        tbl: &TableInfo,
    ) -> Result<(), Error> {
        self.valid_can_read_temporary_table(sctx, tbl)?;
        self.valid_can_read_cache_table(sctx, tbl)
    }

    // go: executorBuilder.validCanReadCacheTable L1
    pub fn valid_can_read_cache_table(
        &self,
        sctx: &dyn SessionContext,
        tbl: &TableInfo,
    ) -> Result<(), Error> {
        if tbl.table_cache_status_type == TableCacheStatusDisable {
            return Ok(());
        }

        let session_vars = sctx.get_session_vars();

        // Temporary table can't switch into cache table. so the following code will not cause confusion
        if session_vars.state.txn_ctx.is_staleness || self.is_staleness {
            return Err(Error::plain("can not stale read cache table"));
        }

        Ok(())
    }

    // go: executorBuilder.validCanReadTemporaryTable L1
    pub fn valid_can_read_temporary_table(
        &self,
        sctx: &dyn SessionContext,
        tbl: &TableInfo,
    ) -> Result<(), Error> {
        if tbl.temp_table_type == TempTableNone {
            return Ok(());
        }

        // Some tools like dumpling use history read to dump all table's records and will be fail if we return an error.
        // So we do not check SnapshotTS here

        let session_vars = sctx.get_session_vars();

        if tbl.temp_table_type == TempTableLocal && session_vars.snapshot_ts != 0 {
            return Err(Error::plain(
                "can not read local temporary table when 'tidb_snapshot' is set",
            ));
        }

        if session_vars.state.txn_ctx.is_staleness || self.is_staleness {
            return Err(Error::plain("can not stale read temporary table"));
        }

        Ok(())
    }

    /// The cache of a cached table (`table.CachedTable.TryReadFromCache` /
    /// `UpdateLockForRead`): cached tables are D06.4 part 2 (their `TableFromMeta` gives a
    /// marker without a cache), so the builder stops here with `b.err` set.
    // glue: stub of executorBuilder.getCacheTable, blocked-by:D06.4 (the cache of a cached table)
    pub fn get_cache_table(&mut self, tbl_info: &TableInfo, start_ts: u64) -> Option<()> {
        let _ = (tbl_info, start_ts);
        self.err = Some(blocked("D06.4", "reading a cached table"));
        None
    }
}

/// buildIndexUsageReporter: the index usage reporter of a reader, when the statement
/// collects index usage.
///
/// The statement's index usage collector (`StmtCtx.IndexUsageCollector`,
/// `indexusage.StmtIndexUsageCollector`) is statistics (D11.2): no statement has one, so
/// there is no reporter (Go's nil), and the table statistics are not preloaded for it
/// (`plan.LoadTableStats`).
// go: buildIndexUsageReporter L2 (no statement has an index usage collector: D11.2)
pub fn build_index_usage_reporter(
    sctx: &dyn SessionContext,
    plan: &PointGetPlan,
) -> Option<IndexUsageReporter> {
    let _ = (sctx, plan);
    // sc.IndexUsageCollector != nil && sc.RuntimeStatsColl != nil: never (D11.2).
    None
}

/// newClosestReadAdjuster let the request be sent to closest replica(within the same zone)
/// if response size exceeds certain threshold.
///
/// Only the table reader (VS1-34) uses it, with its `DistSQLContext` (D07.3) and the
/// request's `MatchStoreLabels` (not in `kv.Request` yet).
// glue: stub of newClosestReadAdjuster, blocked-by:D07.3 (the closest replica read adjuster of a coprocessor request)
pub fn new_closest_read_adjuster() -> Result<(), Error> {
    Err(blocked(
        "D07.3",
        "the closest replica read adjuster of a coprocessor request",
    ))
}

/// NewRowDecoder creates a chunk decoder for new row format row value decode.
///
/// `ctx` is what Go reads from the session: its expression context (the default values of
/// the columns missing from a row) and its time zone. The column infos share the schema
/// columns' field types (`Ft: col.RetType`, N13): built once per executor, never per row.
// go: NewRowDecoder L1 [M-CTX, M-SHARE]
pub fn new_row_decoder(
    expr_ctx: &ExprContextRef,
    loc: Arc<tidb_base::gostd::time::Location>,
    schema: &Schema,
    tbl: &Arc<TableInfo>,
) -> ChunkDecoder {
    let get_col_info_by_id = |tbl: &TableInfo, col_id: i64| -> Option<Arc<ColumnInfo>> {
        for col in tbl.columns.iter().flatten() {
            if col.id == col_id {
                return Some(Arc::clone(col));
            }
        }
        None
    };
    let mut pk_cols: Vec<i64> = Vec::new();
    let mut req_cols: Vec<ColInfo> = Vec::with_capacity(schema.columns.len());
    for col in &schema.columns {
        let is_pk = (tbl.pk_is_handle
            && tidb_base::mysql::has_pri_key_flag(col.ret_type.get_flag()))
            || col.id == ExtraHandleID as i64;
        if is_pk {
            pk_cols.push(col.id);
        }
        let mut is_generated_col = false;
        if col.virtual_expr.is_some() {
            is_generated_col = true;
        }
        req_cols.push(ColInfo {
            id: col.id,
            virtual_gen_col: is_generated_col,
            ft: Arc::clone(&col.ret_type),
            ..ColInfo::default()
        });
    }
    if pk_cols.is_empty() {
        pk_cols = try_get_common_pk_column_ids(tbl).unwrap_or_default();
        if pk_cols.is_empty() {
            pk_cols = vec![-1];
        }
    }
    // The closure owns what it reads (it lives as long as the decoder): the column IDs of
    // reqCols, the table and the expression context.
    let req_col_ids: Vec<i64> = req_cols.iter().map(|c| c.id).collect();
    let tbl = Arc::clone(tbl);
    let expr_ctx = Arc::clone(expr_ctx);
    let def_val: DefDatumFn = Box::new(move |i: usize, chk: &mut Chunk| -> Result<(), Error> {
        if req_col_ids[i] < 0 {
            // model.ExtraHandleID, ExtraPhysTblID... etc
            // Don't set the default value for that column.
            chk.append_null(i);
            return Ok(());
        }

        let ci = get_col_info_by_id(&tbl, req_col_ids[i]);
        // Go dereferences the nil column info of an ID the table does not have (a panic).
        let ci = ci.expect("NewRowDecoder: column info of a schema column");
        let d = get_col_origin_default_value(&*expr_ctx, &ci)?;
        chk.append_datum(i, &d);
        Ok(())
    });
    new_chunk_decoder(req_cols, pk_cols, Some(def_val), Some(loc))
}

/// The function `newReplicaReadAdjuster` returns: the store labels to match (client-go's
/// `tikv.WithMatchLabels`, `None`: no option) and the replica read type of a batch of
/// `count` keys.
pub type ReplicaReadAdjuster =
    Arc<dyn Fn(isize) -> (Option<Vec<metapb::StoreLabel>>, ReplicaReadType) + Send + Sync>;

/// newReplicaReadAdjuster: read from the closest replica when the estimated response size
/// reaches `tidb_adaptive_closest_read_threshold`, else from the leader.
///
/// client-go's `tikv.StoreSelectorOption` is the label list it selects with; the threshold
/// is read from the session when the adjuster is built (Go reads it when it runs: same
/// value, the statement does not change it).
// go: newReplicaReadAdjuster L2 (the store selector option is its label list; the threshold is read when built)
pub fn new_replica_read_adjuster(
    sctx: &dyn SessionContext,
    avg_row_size: f64,
) -> ReplicaReadAdjuster {
    let threshold = sctx.get_session_vars().replica_closest_read_threshold;
    Arc::new(move |count: isize| {
        if (avg_row_size * count as f64) as i64 >= threshold {
            return (
                Some(vec![metapb::StoreLabel {
                    key: DCLabelKey.to_owned(),
                    value: config::get_txn_scope_from_config(),
                }]),
                ReplicaReadMixed,
            );
        }
        // fallback to read from leader if the request is small
        (None, ReplicaReadLeader)
    })
}

// go: isCommonHandleRead L1
pub fn is_common_handle_read(tbl: &TableInfo, idx: &tidb_model::IndexInfo) -> bool {
    tbl.is_common_handle && idx.primary
}

/// getPhysicalTableID: a physical table's (partition's) ID, else the table's.
// go: getPhysicalTableID L1 [M-IFACE]
pub fn get_physical_table_id(t: &dyn tidb_infoschema::table::Table) -> i64 {
    if let Some(p) = t.as_physical_table() {
        return p.get_physical_id();
    }
    t.meta().id
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::tests::{MockSession, test_plan};

    #[tokio::test]
    async fn dispatch_reports_the_executor_block() {
        let mut s = MockSession::new(u64::MAX);
        let mut b = new_executor_builder(&s, None, Arc::new(TelemetryInfo::default()));
        assert!(b.build(&mut s, None).await.is_none());
        assert!(b.err.is_none());
        for (kind, block) in [(PhysicalKind::TableReader, "D12.1")] {
            let mut b = new_executor_builder(&s, None, Arc::new(TelemetryInfo::default()));
            assert!(b.build(&mut s, Some(&test_plan(kind))).await.is_none());
            let err = b.err.expect("blocked");
            assert_eq!(
                tidb_base::blocked::blocked_by(&err),
                Some(block),
                "{kind:?}"
            );
        }
        let mut b = new_executor_builder(&s, None, Arc::new(TelemetryInfo::default()));
        assert!(
            b.build(&mut s, Some(&test_plan(PhysicalKind::TableScan)))
                .await
                .is_none()
        );
        assert_eq!(
            b.err.unwrap().to_string(),
            "[executor:8114]Unknown Plan *core.PhysicalTableScan"
        );
    }

    /// `buildSort` / `buildTopN` build the executor over the child's executor; both run (the
    /// sort on its parallel path, sort.go:170, with the session's worker count).
    #[tokio::test]
    async fn builds_sort_and_top_n() {
        use tidb_planner::physicalop::BasePhysicalPlan;

        let mut s = MockSession::new(u64::MAX);
        let (child, _) = crate::testutil::mock_plan(5);
        let key = Expr::Column((*child.schema().columns[0]).clone()).into_ref();
        let by_items = vec![ByItems {
            expr: key,
            desc: true,
        }];
        let base = || {
            let mut base = BasePhysicalPlan::default();
            base.children = vec![Arc::clone(&child)];
            base
        };
        let ctx = crate::exec::ExecCtx::new(Arc::new(tidb_sessionctx::stmtctx::new_stmt_ctx()));

        let top_n: Plan = Arc::new(PhysicalTopN {
            base: base(),
            by_items: by_items.clone(),
            offset: 1,
            count: 2,
            ..PhysicalTopN::default()
        });
        let mut b = new_executor_builder(&s, None, Arc::new(TelemetryInfo::default()));
        let mut e = b.build(&mut s, Some(&top_n)).await.expect("built");
        assert!(b.err.is_none());
        e.open(&ctx).await.unwrap();
        let mut chk = crate::internal::exec::executor::new_first_chunk(e.as_ref());
        e.next(&ctx, &mut chk).await.unwrap();
        let got: Vec<i64> = (0..chk.num_rows())
            .map(|i| chk.get_row(i).get_int64(0))
            .collect();
        assert_eq!(got, vec![3, 2]);
        e.close().await.unwrap();

        let sort: Plan = Arc::new(PhysicalSort {
            base: base(),
            by_items,
            is_partial_sort: false,
        });
        let mut b = new_executor_builder(&s, None, Arc::new(TelemetryInfo::default()));
        let mut e = b.build(&mut s, Some(&sort)).await.expect("built");
        e.open(&ctx).await.unwrap();
        let mut got = Vec::new();
        loop {
            e.next(&ctx, &mut chk).await.unwrap();
            if chk.num_rows() == 0 {
                break;
            }
            got.extend((0..chk.num_rows()).map(|i| chk.get_row(i).get_int64(0)));
        }
        assert_eq!(got, vec![4, 3, 2, 1, 0]);
        e.close().await.unwrap();

        let mut b = new_executor_builder(&s, None, Arc::new(TelemetryInfo::default()));
        assert!(
            b.build(&mut s, Some(&test_plan(PhysicalKind::TopN)))
                .await
                .is_none()
        );
        assert_eq!(
            b.err.unwrap().to_string(),
            "a TopN plan that is not a *core.PhysicalTopN"
        );
    }
}
