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

/// `indexReaderExecutorContext.Detach`: the index reader's context (D12.1).
// go: indexReaderExecutorContext.Detach L2 (boundary stub, blocked-by:D12.1)
pub fn index_reader_executor_context_detach() -> Option<()> {
    // glue: stub, blocked-by:D12.1 (IndexReaderExecutor)
    None
}

/// `indexLookUpExecutorContext.Detach`: the index lookup's context (D12.1).
// go: indexLookUpExecutorContext.Detach L2 (boundary stub, blocked-by:D12.1)
pub fn index_look_up_executor_context_detach() -> Option<()> {
    // glue: stub, blocked-by:D12.1 (IndexLookUpExecutor)
    None
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
        index_usage_reporter: None,
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

/// `IndexReaderExecutor.Detach` (D12.1).
// go: IndexReaderExecutor.Detach L2 (boundary stub, blocked-by:D12.1)
pub fn index_reader_executor_detach() -> Option<Box<dyn Executor>> {
    // glue: stub, blocked-by:D12.1 (IndexReaderExecutor)
    None
}

/// `IndexLookUpExecutor.Detach` (D12.1).
// go: IndexLookUpExecutor.Detach L2 (boundary stub, blocked-by:D12.1)
pub fn index_look_up_executor_detach() -> Option<Box<dyn Executor>> {
    // glue: stub, blocked-by:D12.1 (IndexLookUpExecutor)
    None
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
