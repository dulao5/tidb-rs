//! go-file: pkg/executor/detach.go
//!
//! Detaching an executor tree from the session, for a cursor that outlives its statement
//! (`recordSet.TryDetach`).
//!
//! With N2 an executor never holds the session: it reads the frozen statement context
//! (`ExecCtx`). So each executor's `Detach` is a copy of itself once it is ported (the
//! expression contexts are already static, Go's `IntoStatic()`); the per-executor methods of
//! this file are boundary stubs until their executors exist (D12.1, D12.2).

use crate::internal::exec::executor::Executor;

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

/// `tableReaderExecutorContext.Detach`: the table reader's context (D12.1, VS1-34).
// go: tableReaderExecutorContext.Detach L2 (boundary stub, blocked-by:D12.1)
pub fn table_reader_executor_context_detach() -> Option<()> {
    // glue: stub, blocked-by:D12.1 (TableReaderExecutor)
    None
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

/// `TableReaderExecutor.Detach` (D12.1, VS1-34).
// go: TableReaderExecutor.Detach L2 (boundary stub, blocked-by:D12.1)
pub fn table_reader_executor_detach() -> Option<Box<dyn Executor>> {
    // glue: stub, blocked-by:D12.1 (TableReaderExecutor)
    None
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
