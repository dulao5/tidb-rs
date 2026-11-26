//! go-file: pkg/planner/core/plan_to_pb.go
//!
//! `ToPB` of the operators of the first vertical slice that run in the coprocessor (block
//! D10.9): the pushed-down part of a table reader becomes a list (TiKV) or a tree (TiFlash)
//! of `tipb.Executor`s, the DAG request the table reader executor sends.
//!
//! - The protobuf encoding must equal Go's byte for byte. gogoproto marshals the
//!   non-nullable proto2 fields of `tipb.Executor` (`tp`, `fine_grained_shuffle_stream_count`,
//!   `fine_grained_shuffle_batch_size`), `tipb.TopN.limit` and `tipb.Limit.limit` always, and
//!   Go sets the pointer fields `executor_id` (to `""` when there is none), `keep_order` and
//!   `is_fast_scan` always; the Rust messages set them explicitly, as `expr_to_pb.rs` does for
//!   `tipb.Expr` (`new_executor`).
//! - Expressions are converted with an empty evaluation state: a correlated column (Go reads
//!   `*col.Data`) only occurs under Apply, which the slice does not build (spec §5 item 4).
//! - The operators' `ToPB` are inherent methods here; the `PhysicalPlan` trait methods in
//!   `core/physical_plans.rs` dispatch to them.
//! - Fields the slice does not carry (D10.1 spec §3): `runtimeFilterList` and
//!   `maxWaitTimeMs` (TiFlash runtime filters: `RuntimeFilterListToPB` of a nil list gives an
//!   empty list, the wait time is 0) and `AnnIndexExtra` (vector indexes, nil).

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_base::errors;
use tidb_base::gofmt::Arg;
use tidb_base::kv::{StoreType, TiFlash, TiKV};
use tidb_expr_pushdown::expr_to_pb::{
    expressions_to_pb_list, projection_expressions_to_pb_list, sort_by_item_to_pb,
};
use tidb_expression::{EvalState, Expr, ExprRef};
use tidb_infoschema::tables::{
    build_table_scan_from_infos, set_pb_columns_default_value, try_get_common_pk_column_ids,
};
use tidb_infoschema::util::misc::columns_to_proto;
use tidb_model::{
    ColumnInfo, ExtraHandleID, ExtraPhysTblID, IndexInfo, new_extra_handle_col_info,
    new_extra_phys_tbl_id_col_info,
};
use tidb_sessionctx::planctx::BuildPBContext;
use tidb_sessionctx::ranger::Range;
use tipb::ExecType;

use crate::base::PhysicalPlan;
use crate::core::physical_plans::{
    PhysicalIndexScan, PhysicalLimit, PhysicalProjection, PhysicalSelection, PhysicalSort,
    PhysicalTableScan, PhysicalTopN,
};

/// `&tipb.Executor{Tp: tp, ExecutorId: &executorID, ...}`: the fields gogoproto always
/// marshals are set (see the module doc).
// glue: the struct literal of tipb.Executor (gogoproto non-nullable fields)
fn new_executor(tp: ExecType, executor_id: String) -> tipb::Executor {
    let mut e = tipb::Executor::default();
    e.set_tp(tp);
    e.set_executor_id(executor_id);
    e.set_fine_grained_shuffle_stream_count(0);
    e.set_fine_grained_shuffle_batch_size(0);
    e
}

/// `expression.SortByItemToPB` of an item the push down check accepted. Go appends a nil
/// `*tipb.ByItem` when the conversion fails, and marshalling the DAG then dereferences it;
/// Rust panics here (spec §5 item 5).
// glue: SortByItemToPB's nil result (Go panics when the DAG is marshalled)
fn sort_by_item(ctx: &BuildPBContext, expr: &Expr, desc: bool) -> tipb::ByItem {
    let client = ctx.get_client();
    let eval_ctx = ctx.get_expr_ctx().get_eval_ctx();
    sort_by_item_to_pb(&*eval_ctx, &EvalState::new(), &*client, expr, desc)
        .expect("SortByItemToPB of a pushed-down item")
}

impl PhysicalSelection {
    /// ToPB implements PhysicalPlan ToPB interface.
    // go: PhysicalSelection.ToPB L1 [M-CTX]
    pub fn to_pb(
        &self,
        ctx: &BuildPBContext,
        store_type: StoreType,
    ) -> Result<tipb::Executor, Error> {
        let client = ctx.get_client();
        let eval_ctx = ctx.get_expr_ctx().get_eval_ctx();
        let conditions =
            expressions_to_pb_list(&*eval_ctx, &EvalState::new(), &self.conditions, &*client)?;
        let mut sel_exec = tipb::Selection::default();
        sel_exec.set_conditions(conditions.into());
        let mut executor_id = String::new();
        if store_type == TiFlash {
            sel_exec.set_child(self.children()[0].to_pb(ctx, store_type)?);
            executor_id = self.explain_id(None);
        }
        let mut e = new_executor(ExecType::TypeSelection, executor_id);
        e.set_selection(sel_exec);
        Ok(e)
    }
}

impl PhysicalProjection {
    /// ToPB implements PhysicalPlan ToPB interface.
    // go: PhysicalProjection.ToPB L1 [M-CTX]
    pub fn to_pb(
        &self,
        ctx: &BuildPBContext,
        store_type: StoreType,
    ) -> Result<tipb::Executor, Error> {
        let client = ctx.get_client();
        let eval_ctx = ctx.get_expr_ctx().get_eval_ctx();
        let exprs = projection_expressions_to_pb_list(
            &*eval_ctx,
            &EvalState::new(),
            &self.exprs,
            &*client,
        )?;
        let mut proj_exec = tipb::Projection::default();
        proj_exec.set_exprs(exprs.into());
        if !(store_type == TiFlash || store_type == TiKV) {
            return Err(errors::errorf(
                "the projection can only be pushed down to TiFlash or TiKV now, not %s",
                &[Arg::from(store_type.name())],
            ));
        }
        proj_exec.set_child(self.children()[0].to_pb(ctx, store_type)?);
        let executor_id = self.explain_id(None);
        let mut e = new_executor(ExecType::TypeProjection, executor_id);
        e.set_projection(proj_exec);
        Ok(e)
    }
}

impl PhysicalTopN {
    /// ToPB implements PhysicalPlan ToPB interface.
    // go: PhysicalTopN.ToPB L1 [M-CTX]
    pub fn to_pb(
        &self,
        ctx: &BuildPBContext,
        store_type: StoreType,
    ) -> Result<tipb::Executor, Error> {
        let mut top_n_exec = tipb::TopN::default();
        top_n_exec.set_limit(self.count);
        for item in &self.by_items {
            top_n_exec
                .mut_order_by()
                .push(sort_by_item(ctx, &item.expr, item.desc));
        }
        for item in &self.partition_by {
            let col = Expr::Column((*item.col).clone());
            top_n_exec
                .mut_partition_by()
                .push(sort_by_item(ctx, &col, item.desc));
        }
        let mut executor_id = String::new();
        if store_type == TiFlash {
            top_n_exec.set_child(self.children()[0].to_pb(ctx, store_type)?);
            executor_id = self.explain_id(None);
        }
        let mut e = new_executor(ExecType::TypeTopN, executor_id);
        e.set_top_n(top_n_exec);
        Ok(e)
    }
}

impl PhysicalLimit {
    /// ToPB implements PhysicalPlan ToPB interface.
    // go: PhysicalLimit.ToPB L1 [M-CTX]
    pub fn to_pb(
        &self,
        ctx: &BuildPBContext,
        store_type: StoreType,
    ) -> Result<tipb::Executor, Error> {
        let client = ctx.get_client();
        let mut limit_exec = tipb::Limit::default();
        limit_exec.set_limit(self.count);
        let mut executor_id = String::new();
        for item in &self.partition_by {
            let col = Expr::Column((*item.col).clone());
            limit_exec
                .mut_partition_by()
                .push(sort_by_item(ctx, &col, item.desc));
        }
        if let Some(prefix_col) = &self.prefix_col {
            let truncate_key_exprs: Vec<ExprRef> =
                vec![Arc::new(Expr::Column((**prefix_col).clone()))];
            let eval_ctx = ctx.get_expr_ctx().get_eval_ctx();
            let truncate_key_exprs_pb = expressions_to_pb_list(
                &*eval_ctx,
                &EvalState::new(),
                &truncate_key_exprs,
                &*client,
            )?;
            limit_exec.set_truncate_key_expr(truncate_key_exprs_pb.into());
        }
        if store_type == TiFlash {
            limit_exec.set_child(self.children()[0].to_pb(ctx, store_type)?);
            executor_id = self.explain_id(None);
        }
        let mut e = new_executor(ExecType::TypeLimit, executor_id);
        e.set_limit(limit_exec);
        Ok(e)
    }
}

impl PhysicalTableScan {
    /// ToPB implements PhysicalPlan ToPB interface.
    ///
    /// Go's first branch (`partitionTableScanToPBForFlash`) also checks
    /// `StmtCtx.UseDynamicPartitionPrune()`, which `ToPB` cannot read here (the context
    /// argument is a `BuildPBContext`); partitioned tables stop in the plan builder
    /// (`blocked-by:D09.12`), so the branch is a boundary for both pruning modes. The
    /// TiFlash table scan counters (`telemetry`) are not ported.
    // go: PhysicalTableScan.ToPB L2 (the partition branch is blocked-by:D09.12; no telemetry counters)
    pub fn to_pb(
        &self,
        ctx: &BuildPBContext,
        store_type: StoreType,
    ) -> Result<tipb::Executor, Error> {
        if store_type == TiFlash
            && self.table.get_partition_info().is_some()
            && self.is_mpp_or_batch_cop
        {
            // glue: blocked-by:D09.12 (partitionTableScanToPBForFlash: the TiFlash scan of a partitioned table)
            return Err(blocked(
                "D09.12",
                "the TiFlash table scan of a partitioned table (partitionTableScanToPBForFlash)",
            ));
        }
        let columns: Vec<&ColumnInfo> = self.columns.iter().map(|c| &**c).collect();
        let mut ts_exec =
            build_table_scan_from_infos(&self.table, &columns, self.store_type == TiFlash);
        ts_exec.set_desc(self.desc);
        let keep_order = self.keep_order;
        ts_exec.set_keep_order(keep_order);
        ts_exec.set_is_fast_scan(ctx.tiflash_fast_scan);

        if !self.late_materialization_filter_condition.is_empty() {
            let client = ctx.get_client();
            let eval_ctx = ctx.get_expr_ctx().get_eval_ctx();
            let conditions = expressions_to_pb_list(
                &*eval_ctx,
                &EvalState::new(),
                &self.late_materialization_filter_condition,
                &*client,
            )?;
            ts_exec.set_pushed_down_filter_conditions(conditions.into());
        }

        // p.AnnIndexExtra is not carried (nil); RuntimeFilterListToPB of the nil
        // runtimeFilterList is an empty list; int32(p.maxWaitTimeMs) is 0 (module doc).
        ts_exec.set_max_wait_time_ms(0);

        if self.is_partition {
            ts_exec.set_table_id(self.physical_table_id);
        }
        let mut executor_id = String::new();
        if store_type == TiFlash {
            executor_id = self.explain_id(None);
        }
        let err = set_pb_columns_default_value(
            &*ctx.get_expr_ctx(),
            ts_exec.mut_columns().as_mut_slice(),
            &columns,
        );
        let mut e = new_executor(ExecType::TypeTableScan, executor_id);
        e.set_tbl_scan(ts_exec);
        err?;
        Ok(e)
    }
}

/// checkCoverIndex checks whether we can pass unique info to TiKV. We should push it if and only
/// if the length of range and index are equal.
// go: checkCoverIndex L1
pub fn check_cover_index(idx: &IndexInfo, ranges: &[Range]) -> bool {
    // If the index is (c1, c2) but the query range only contains c1, it is not a unique get.
    if !idx.unique {
        return false;
    }
    let idx_cols_len = idx.columns.as_deref().map_or(0, <[_]>::len);
    for rg in ranges {
        if rg.low_val.len() != idx_cols_len {
            return false;
        }
        for v in &rg.low_val {
            if v.is_null() {
                // a unique index may have duplicated rows with NULLs, so we cannot set the unique
                // attribute to true when the range has NULL (upstream issue `29650`).
                return false;
            }
        }
        for v in &rg.high_val {
            if v.is_null() {
                return false;
            }
        }
    }
    true
}

impl PhysicalIndexScan {
    /// ToPB implements PhysicalPlan ToPB interface.
    ///
    /// Does NOT call `new_executor`: Go returns `&tipb.Executor{Tp: ..., IdxScan: ...}` with
    /// `ExecutorId == nil`, while gogoproto still encodes the non-nullable 0-valued
    /// `FineGrainedShuffleStreamCount` and `FineGrainedShuffleBatchSize` fields.
    // go: PhysicalIndexScan.ToPB L1 [M-CTX]
    pub fn to_pb(
        &self,
        _ctx: &BuildPBContext,
        _store_type: StoreType,
    ) -> Result<tipb::Executor, Error> {
        let mut owned_cols: Vec<Arc<ColumnInfo>> = Vec::with_capacity(self.schema().len());
        let public_cols = self.table.cols();
        for col in &self.schema().columns {
            if col.id == ExtraHandleID as i64 {
                owned_cols.push(Arc::new(new_extra_handle_col_info()));
            } else if col.id == ExtraPhysTblID as i64 {
                owned_cols.push(Arc::new(new_extra_phys_tbl_id_col_info()));
            } else {
                let info = public_cols
                    .iter()
                    .flatten()
                    .find(|info| info.id == col.id)
                    .expect("runtime error: invalid memory address or nil pointer dereference");
                owned_cols.push(Arc::clone(info));
            }
        }
        let col_refs: Vec<&ColumnInfo> = owned_cols.iter().map(|c| &**c).collect();
        let mut pk_col_ids: Vec<i64> = Vec::new();
        if self.need_common_handle {
            pk_col_ids = try_get_common_pk_column_ids(&self.table).unwrap_or_default();
        }
        let mut idx_exec = tipb::IndexScan::default();
        idx_exec.set_table_id(self.table.id);
        idx_exec.set_index_id(self.index.id);
        idx_exec
            .set_columns(columns_to_proto(&col_refs, self.table.pk_is_handle, true, false).into());
        idx_exec.set_desc(self.desc);
        if !pk_col_ids.is_empty() {
            idx_exec.set_primary_column_ids(pk_col_ids);
        }
        if self.is_partition {
            idx_exec.set_table_id(self.physical_table_id);
        }
        let unique = check_cover_index(&self.index, &self.ranges);
        idx_exec.set_unique(unique);

        let mut e = tipb::Executor::default();
        e.set_tp(ExecType::TypeIndexScan);
        e.set_idx_scan(idx_exec);
        e.set_fine_grained_shuffle_stream_count(0);
        e.set_fine_grained_shuffle_batch_size(0);
        Ok(e)
    }
}

/// FindColumnInfoByID finds ColumnInfo in cols by ID.
// go: FindColumnInfoByID L1
pub fn find_column_info_by_id(col_infos: &[Arc<ColumnInfo>], id: i64) -> Option<&Arc<ColumnInfo>> {
    col_infos.iter().find(|info| info.id == id)
}

impl PhysicalSort {
    /// ToPB implements PhysicalPlan ToPB interface.
    // go: PhysicalSort.ToPB L1 [M-CTX]
    pub fn to_pb(
        &self,
        ctx: &BuildPBContext,
        store_type: StoreType,
    ) -> Result<tipb::Executor, Error> {
        if !self.is_partial_sort {
            return Err(errors::errorf(
                "sort %s can't convert to pb, because it isn't a partial sort",
                &[Arg::from(self.base.plan.explain_id(None))],
            ));
        }

        let mut sort_exec = tipb::Sort::default();
        for item in &self.by_items {
            sort_exec
                .mut_by_items()
                .push(sort_by_item(ctx, &item.expr, item.desc));
        }
        let is_partial_sort = self.is_partial_sort;
        sort_exec.set_is_partial_sort(is_partial_sort);

        sort_exec.set_child(self.children()[0].to_pb(ctx, store_type)?);
        let executor_id = self.explain_id(None);
        let mut e = new_executor(ExecType::TypeSort, executor_id);
        e.set_sort(sort_exec);
        e.set_fine_grained_shuffle_stream_count(
            self.base.tiflash_fine_grained_shuffle_stream_count,
        );
        e.set_fine_grained_shuffle_batch_size(ctx.tiflash_fine_grained_shuffle_batch_size);
        Ok(e)
    }
}
