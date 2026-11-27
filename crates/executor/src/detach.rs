//! go-file: pkg/executor/detach.go
//!
//! Detaching an executor tree from the session, for a cursor that outlives its statement
//! (`recordSet.TryDetach`).
//!
//! With N2 an executor never holds the session: it reads the frozen statement context
//! (`ExecCtx`). So each executor's `Detach` is a copy of itself once it is ported (the
//! expression contexts are already static, Go's `IntoStatic()`); the per-executor methods of
//! this file are boundary stubs until their executors exist (D12.1, D12.2).

use std::sync::Arc;

use tidb_sessionctx::exprctx::BuildContextRef;

use crate::distsql::{
    IndexLookUpExecutor, IndexLookUpExecutorContext, IndexReaderExecutor,
    IndexReaderExecutorContext,
};
use crate::internal::exec::executor::Executor;
use crate::table_reader::{TableReaderExecutor, TableReaderExecutorContext};

/// Detach detaches the current executor from the session context. After detaching, the
/// session context can be used to execute another statement while this executor is still
/// running. The returning value shows whether this executor is able to be detached.
///
/// NOTE: the implementation of `Detach` should guarantee that no matter whether it returns
/// true or false, both the original executor and the returning executor should be able to be
/// used correctly. This restriction is to make sure that if `Detach(a)` returns `true`, while
/// other children of `a`'s parent returns `false`, the caller can still use the original one.
// go: Detach L1
pub fn detach(original_executor: &dyn Executor) -> Option<Box<dyn Executor>> {
    let mut new_executor = original_executor.detach()?;
    let children = original_executor.all_children();
    let mut new_children = Vec::with_capacity(children.len());
    for child in children {
        let detached = detach(child.as_ref())?;
        new_children.push(detached);
    }
    new_executor.set_all_children(new_children);
    Some(new_executor)
}

/// `tableReaderExecutorContext.Detach`: detaches the table reader's execution context from
/// the session. Under N2 (`M-CTX`) the expression context is already a static snapshot, so
/// this detaches `dctx`, `rctx`, and `build_pb_ctx` and clones the shared handles.
// go: tableReaderExecutorContext.Detach L2 (N2: ExprContext is already static)
pub fn table_reader_executor_context_detach(
    tre_ctx: &TableReaderExecutorContext,
) -> TableReaderExecutorContext {
    let static_expr_ctx: BuildContextRef = Arc::clone(&tre_ctx.ectx) as BuildContextRef;
    TableReaderExecutorContext {
        dctx: Arc::new(tre_ctx.dctx.detach()),
        rctx: Arc::new(tre_ctx.rctx.detach(Arc::clone(&static_expr_ctx))),
        build_pb_ctx: Arc::new(tre_ctx.build_pb_ctx.detach(static_expr_ctx)),
        ectx: Arc::clone(&tre_ctx.ectx),
        stmt_mem_tracker: tre_ctx.stmt_mem_tracker.clone(),
        info_schema: tre_ctx.info_schema.clone(),
        get_ddl_owner: tre_ctx.get_ddl_owner.clone(),
    }
}

/// `indexReaderExecutorContext.Detach`: detaches the index reader's execution context from
/// the session. Under N2 (`M-CTX`) the expression context is already a static snapshot, so
/// this detaches `dctx`, `rctx`, and `build_pb_ctx` and clones the shared handles.
// go: indexReaderExecutorContext.Detach L2 (N2: ExprContext is already static)
pub fn index_reader_executor_context_detach(
    ire_ctx: &IndexReaderExecutorContext,
) -> IndexReaderExecutorContext {
    let static_expr_ctx: BuildContextRef = Arc::clone(&ire_ctx.ectx) as BuildContextRef;
    IndexReaderExecutorContext {
        rctx: Arc::new(ire_ctx.rctx.detach(Arc::clone(&static_expr_ctx))),
        dctx: Arc::new(ire_ctx.dctx.detach()),
        ectx: Arc::clone(&ire_ctx.ectx),
        info_schema: ire_ctx.info_schema.as_ref().map(Arc::clone),
        build_pb_ctx: Arc::new(ire_ctx.build_pb_ctx.detach(static_expr_ctx)),
        stmt_mem_tracker: ire_ctx.stmt_mem_tracker.as_ref().map(Arc::clone),
    }
}

/// `indexLookUpExecutorContext.Detach`: detaches the index lookup's execution context from
/// the session. Under N2 (`M-CTX`) the expression context is already a static snapshot, so
/// this detaches `table_ctx` and returns the detached context cleanly (spec §14 item 6).
// go: indexLookUpExecutorContext.Detach L2 (N2: ExprContext is already static; returns detached context cleanly per spec §14.6)
pub fn index_look_up_executor_context_detach(
    ilu_ctx: &IndexLookUpExecutorContext,
) -> IndexLookUpExecutorContext {
    let mut new_ctx = ilu_ctx.clone();
    new_ctx.table_ctx = table_reader_executor_context_detach(&ilu_ctx.table_ctx);
    new_ctx
}

/// `projectionExecutorContext.Detach` (D12.2, VS1-35).
// go: projectionExecutorContext.Detach L2 (boundary stub, blocked-by:D12.2)
pub fn projection_executor_context_detach() -> Option<()> {
    // glue: stub, blocked-by:D12.2 (ProjectionExec)
    None
}

/// `selectionExecutorContext.Detach` (D12.2, VS1-35).
// go: selectionExecutorContext.Detach L2 (boundary stub, blocked-by:D12.2)
pub fn selection_executor_context_detach() -> Option<()> {
    // glue: stub, blocked-by:D12.2 (SelectionExec)
    None
}

/// `TableReaderExecutor.Detach`: detaches the table reader from the session context.
// go: TableReaderExecutor.Detach L1
pub fn table_reader_executor_detach(e: &TableReaderExecutor) -> Option<Box<dyn Executor>> {
    let new_exec = TableReaderExecutor {
        ctx: table_reader_executor_context_detach(&e.ctx),
        base: e.base.clone_without_children(),
        index_usage_reporter: e.index_usage_reporter.clone(),
        table: e.table.clone(),
        kv_range_builder: e.kv_range_builder.clone(),
        ranges: e.ranges.clone(),
        grouped_ranges: e.grouped_ranges.clone(),
        group_by_col_idxs: e.group_by_col_idxs.clone(),
        kv_ranges: e.kv_ranges.clone(),
        dag_pb: e.dag_pb.clone(),
        start_ts: e.start_ts,
        txn_scope: e.txn_scope.clone(),
        read_replica_scope: e.read_replica_scope.clone(),
        is_staleness: e.is_staleness,
        net_data_size: e.net_data_size,
        columns: e.columns.clone(),
        result_handler: None,
        plans: e.plans.clone(),
        table_plan: e.table_plan.clone(),
        mem_tracker: e.mem_tracker.clone(),
        select_result_hook: e.select_result_hook.clone(),
        keep_order: e.keep_order,
        desc: e.desc,
        by_items: e.by_items.clone(),
        paging: e.paging,
        store_type: e.store_type,
        cor_col_in_filter: e.cor_col_in_filter,
        cor_col_in_access: e.cor_col_in_access,
        virtual_column_index: e.virtual_column_index.clone(),
        virtual_column_ret_field_types: e.virtual_column_ret_field_types.clone(),
        batch_cop: e.batch_cop,
        dummy: e.dummy,
    };
    Some(Box::new(new_exec))
}

/// `IndexReaderExecutor.Detach`: detaches the index reader from the session context.
// go: IndexReaderExecutor.Detach L1
pub fn index_reader_executor_detach(e: &IndexReaderExecutor) -> Option<Box<dyn Executor>> {
    let new_exec = IndexReaderExecutor {
        ctx: index_reader_executor_context_detach(&e.ctx),
        base: e.base.clone_without_children(),
        index_usage_reporter: e.index_usage_reporter.clone(),
        table: e.table.as_ref().map(Arc::clone),
        index: Arc::clone(&e.index),
        physical_table_id: e.physical_table_id,
        ranges: e.ranges.clone(),
        grouped_ranges: e.grouped_ranges.clone(),
        partitions: e.partitions.clone(),
        part_range_map: e.part_range_map.clone(),
        kv_ranges: e.kv_ranges.clone(),
        dag_pb: e.dag_pb.clone(),
        start_ts: e.start_ts,
        txn_scope: e.txn_scope.clone(),
        read_replica_scope: e.read_replica_scope.clone(),
        is_staleness: e.is_staleness,
        net_data_size: e.net_data_size,
        result: None,
        columns: e.columns.clone(),
        output_columns: e.output_columns.clone(),
        partition_id_map: e.partition_id_map.clone(),
        paging: e.paging,
        keep_order: e.keep_order,
        desc: e.desc,
        by_items: e.by_items.clone(),
        cor_col_in_filter: e.cor_col_in_filter,
        cor_col_in_access: e.cor_col_in_access,
        idx_cols: e.idx_cols.clone(),
        col_lens: e.col_lens.clone(),
        plans: e.plans.clone(),
        mem_tracker: e.mem_tracker.as_ref().map(Arc::clone),
        select_result_hook: e.select_result_hook.clone(),
        dummy: e.dummy,
    };
    Some(Box::new(new_exec))
}

/// `IndexLookUpExecutor.Detach`: detaches the index lookup executor from the session context.
// go: IndexLookUpExecutor.Detach L1
pub fn index_look_up_executor_detach(e: &IndexLookUpExecutor) -> Option<Box<dyn Executor>> {
    let new_exec = IndexLookUpExecutor {
        ctx: index_look_up_executor_context_detach(&e.ctx),
        base: e.base.clone_without_children(),
        index_usage_reporter: e.index_usage_reporter.clone(),
        table: e.table.as_ref().map(Arc::clone),
        index: Arc::clone(&e.index),
        ranges: e.ranges.clone(),
        grouped_ranges: e.grouped_ranges.clone(),
        dag_pb: e.dag_pb.clone(),
        start_ts: e.start_ts,
        txn_scope: e.txn_scope.clone(),
        read_replica_scope: e.read_replica_scope.clone(),
        is_staleness: e.is_staleness,
        handle_idx: e.handle_idx.clone(),
        handle_cols: e.handle_cols.clone(),
        primary_key_index: e.primary_key_index.as_ref().map(Arc::clone),
        table_request: e.table_request.clone(),
        columns: e.columns.clone(),
        partition_id_map: e.partition_id_map.clone(),
        data_reader_builder: e.data_reader_builder.clone(),
        idx_net_data_size: e.idx_net_data_size,
        avg_row_size: e.avg_row_size,
        partition_table_mode: e.partition_table_mode,
        pruned_partitions: e.pruned_partitions.clone(),
        partition_range_map: e.partition_range_map.clone(),
        finished_tx: None,
        result_rx: None,
        result_curr: None,
        idx_worker_handle: None,
        tbl_worker_handles: Vec::new(),
        mem_tracker: e.mem_tracker.as_ref().map(Arc::clone),
        check_index_value: e.check_index_value.clone(),
        grouped_kv_ranges: e.grouped_kv_ranges.clone(),
        worker_started: false,
        by_items: e.by_items.clone(),
        keep_order: e.keep_order,
        desc: e.desc,
        index_paging: e.index_paging,
        cor_col_in_idx_side: e.cor_col_in_idx_side,
        cor_col_in_tbl_side: e.cor_col_in_tbl_side,
        cor_col_in_access: e.cor_col_in_access,
        idx_plans: e.idx_plans.clone(),
        idx_plan_un_nature_orders: e.idx_plan_un_nature_orders.clone(),
        tbl_plans: e.tbl_plans.clone(),
        idx_cols: e.idx_cols.clone(),
        col_lens: e.col_lens.clone(),
        pushed_limit: e.pushed_limit,
        stats: None,
        select_result_hook: e.select_result_hook.clone(),
        dummy: e.dummy,
        index_look_up_push_down: e.index_look_up_push_down,
    };
    Some(Box::new(new_exec))
}

/// `ProjectionExec.Detach` (D12.2, VS1-35).
// go: ProjectionExec.Detach L2 (boundary stub, blocked-by:D12.2)
pub fn projection_exec_detach() -> Option<Box<dyn Executor>> {
    // glue: stub, blocked-by:D12.2 (ProjectionExec)
    None
}

/// `SelectionExec.Detach` (D12.2, VS1-35).
// go: SelectionExec.Detach L2 (boundary stub, blocked-by:D12.2)
pub fn selection_exec_detach() -> Option<Box<dyn Executor>> {
    // glue: stub, blocked-by:D12.2 (SelectionExec)
    None
}
