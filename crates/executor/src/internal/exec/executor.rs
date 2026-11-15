//! go-file: pkg/executor/internal/exec/executor.go
//!
//! The executor interface and the base executor (block D12.0, VS1-13).
//!
//! - Go's `Executor` interface is the [`Executor`] trait. Open / Next / Close are async (N3
//!   §3.1 item 1) and take the statement's [`ExecCtx`]; the other methods are default methods
//!   over the embedded [`BaseExecutorV2`] (`M-SELF`: an executor implements [`Executor::base`]
//!   / [`Executor::base_mut`], as Go embeds `BaseExecutorV2`).
//! - Executors do not hold the session (N2): Go's `BaseExecutor.ctx` (`sessionctx.Context`)
//!   is not kept; what the base executor reads from the session is taken when it is built
//!   (the chunk sizes, the statement context, the SQL killer).
//! - Go's chunk allocator (`vars.GetChunkAllocator()`) is a GC workaround (`M-POOL`): chunks
//!   are allocated directly.
//! - Go recovers panics in `Open` / `Next` / `Close`; here a panic is caught at the task
//!   boundary only (`M-PANIC`).

use std::borrow::Borrow;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use async_trait::async_trait;
use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_chunk::chunk::{self, Chunk};
use tidb_execdetails::execdetails::{BasicRuntimeStats, RuntimeStatsColl};
use tidb_expression::Schema;
use tidb_memory::sqlkiller::SqlKiller;
use tidb_parser::types::FieldType;
use tidb_sessionctx::stmtctx::StatementContext;
use tidb_sessionctx::variable::session::SessionVars;

use crate::exec::ExecCtx;

/// Executor is the physical implementation of an algebra operator.
///
/// In TiDB, all algebra operators are implemented as iterators, i.e., they support a simple
/// Open-Next-Close protocol. See this paper for more details:
///
/// "Volcano-An Extensible and Parallel Query Evaluation System"
///
/// Different from Volcano's execution model, a "Next" function call in TiDB will return a
/// batch of rows, other than a single row in Volcano.
/// NOTE: Executors must call "chk.Reset()" before appending their results to it.
///
/// Go allows `Close` concurrently with `Next`; here `&mut self` forbids it, so an external stop
/// goes through the cancellation in [`ExecCtx`] and the owner calls `close` (N3 §2).
#[async_trait]
pub trait Executor: Send {
    /// The embedded `BaseExecutorV2` (`M-SELF`).
    // glue: M-SELF (Go struct embedding)
    fn base(&self) -> &BaseExecutorV2;
    /// The embedded `BaseExecutorV2` (`M-SELF`).
    // glue: M-SELF (Go struct embedding)
    fn base_mut(&mut self) -> &mut BaseExecutorV2;

    // glue: Go interface method Executor.NewChunk
    fn new_chunk(&self) -> Chunk {
        self.base().alloc.new_chunk()
    }
    // glue: Go interface method Executor.NewChunkWithCapacity
    fn new_chunk_with_capacity(
        &self,
        fields: &[Arc<FieldType>],
        capacity: isize,
        max_cache_size: isize,
    ) -> Chunk {
        self.base()
            .alloc
            .new_chunk_with_capacity(fields, capacity, max_cache_size)
    }
    // glue: Go interface method Executor.RuntimeStats
    fn runtime_stats(&self) -> Option<&Arc<BasicRuntimeStats>> {
        self.base().stats.runtime_stats()
    }
    // glue: Go interface method Executor.HandleSQLKillerSignal
    fn handle_sql_killer_signal(&self) -> Result<(), Error> {
        self.base().killer.handle_sql_killer_signal()
    }
    // glue: Go interface method Executor.RegisterSQLAndPlanInExecForTopSQL
    fn register_sql_and_plan_in_exec_for_top_sql(&self) {
        self.base()
            .stats
            .register_sql_and_plan_in_exec_for_top_sql()
    }
    // glue: Go interface method Executor.AllChildren
    fn all_children(&self) -> &[Box<dyn Executor>] {
        self.base().meta.all_children()
    }
    /// `AllChildren()` for driving the children (Go calls their methods through the slice).
    // glue: Go interface method Executor.AllChildren (mutable use)
    fn all_children_mut(&mut self) -> &mut [Box<dyn Executor>] {
        &mut self.base_mut().meta.children
    }
    // glue: Go interface method Executor.SetAllChildren
    fn set_all_children(&mut self, children: Vec<Box<dyn Executor>>) {
        self.base_mut().meta.set_all_children(children)
    }
    /// `Open` initializes children recursively and "ctx" information.
    // glue: Go interface method Executor.Open
    async fn open(&mut self, ctx: &ExecCtx) -> Result<(), Error> {
        self.base_mut().open(ctx).await
    }
    /// `Next` fetches the next chunk of rows into `req`; an empty chunk means the end.
    // glue: Go interface method Executor.Next
    async fn next(&mut self, ctx: &ExecCtx, req: &mut Chunk) -> Result<(), Error> {
        self.base_mut().next(ctx, req).await
    }
    /// `Close` resets "ctx" information and releases resources (joins the worker tasks).
    // glue: Go interface method Executor.Close
    async fn close(&mut self) -> Result<(), Error> {
        self.base_mut().close().await
    }
    // glue: Go interface method Executor.Schema
    fn schema(&self) -> Schema {
        self.base().meta.schema()
    }
    // glue: Go interface method Executor.RetFieldTypes
    fn ret_field_types(&self) -> &[Arc<FieldType>] {
        self.base().meta.ret_field_types()
    }
    // glue: Go interface method Executor.InitCap
    fn init_cap(&self) -> isize {
        self.base().alloc.init_cap()
    }
    // glue: Go interface method Executor.MaxChunkSize
    fn max_chunk_size(&self) -> isize {
        self.base().alloc.max_chunk_size()
    }
    /// Detach detaches the current executor from the session context without considering its
    /// children. `None` is Go's `(nil, false)`.
    ///
    /// It has to make sure, no matter whether it returns true or false, both the original
    /// executor and the returning executor should be able to be used correctly.
    // glue: Go interface method Executor.Detach
    fn detach(&self) -> Option<Box<dyn Executor>> {
        self.base().detach()
    }
    /// Go's type assertion `e.(*PointGetExecutor)` (`M-IFACE`): the point get executor
    /// (D12.1) returns itself.
    // glue: M-IFACE (type assertion to *PointGetExecutor)
    fn as_point_get(&mut self) -> Option<&mut dyn crate::glue::PointGetExecutor> {
        None
    }
}

/// Go's `retFieldTypes []*types.FieldType`, shared by an executor's meta and its chunk
/// allocator.
type RetFieldTypes = Arc<[Arc<FieldType>]>;

/// executorChunkAllocator is a helper to implement `Chunk` related methods in `Executor`
/// interface. Go's `AllocPool` is not kept (`M-POOL`).
///
/// `retFieldTypes` is the executor meta's slice (Go assigns the same `[]*types.FieldType`
/// to both): one `Arc<[Arc<FieldType>]>`, whose types are the schema columns' `RetType`s
/// (N13 §8).
#[derive(Clone, Debug, Default)]
pub struct ExecutorChunkAllocator {
    ret_field_types: RetFieldTypes,
    init_cap: isize,
    max_chunk_size: isize,
}

/// newExecutorChunkAllocator creates a new `executorChunkAllocator`
// go: newExecutorChunkAllocator L2 (no allocator pool, M-POOL)
fn new_executor_chunk_allocator(
    vars: &SessionVars,
    ret_field_types: RetFieldTypes,
) -> ExecutorChunkAllocator {
    ExecutorChunkAllocator {
        init_cap: vars.init_chunk_size as isize,
        max_chunk_size: vars.max_chunk_size as isize,
        ret_field_types,
    }
}

impl ExecutorChunkAllocator {
    /// InitCap returns the initial capacity for chunk
    // go: executorChunkAllocator.InitCap L1
    pub fn init_cap(&self) -> isize {
        self.init_cap
    }

    /// SetInitCap sets the initial capacity for chunk
    // go: executorChunkAllocator.SetInitCap L1
    pub fn set_init_cap(&mut self, c: isize) {
        self.init_cap = c;
    }

    /// MaxChunkSize returns the max chunk size.
    // go: executorChunkAllocator.MaxChunkSize L1
    pub fn max_chunk_size(&self) -> isize {
        self.max_chunk_size
    }

    /// SetMaxChunkSize sets the max chunk size.
    // go: executorChunkAllocator.SetMaxChunkSize L1
    pub fn set_max_chunk_size(&mut self, size: isize) {
        self.max_chunk_size = size;
    }

    /// NewChunk creates a new chunk according to the executor configuration
    // go: executorChunkAllocator.NewChunk L1
    pub fn new_chunk(&self) -> Chunk {
        self.new_chunk_with_capacity(
            &self.ret_field_types,
            self.init_cap(),
            self.max_chunk_size(),
        )
    }

    /// NewChunkWithCapacity allows the caller to allocate the chunk with any types, capacity
    /// and max size in the pool
    // go: executorChunkAllocator.NewChunkWithCapacity L2 (allocated directly, M-POOL)
    pub fn new_chunk_with_capacity<F: Borrow<FieldType>>(
        &self,
        fields: &[F],
        capacity: isize,
        max_cache_size: isize,
    ) -> Chunk {
        chunk::new(
            fields,
            capacity.max(0) as usize,
            max_cache_size.max(0) as usize,
        )
    }
}

/// executorMeta is a helper to store metadata for an execturo and implement the getter
#[derive(Default)]
pub struct ExecutorMeta {
    /// `nil` is `None`.
    schema: Option<Schema>,
    children: Vec<Box<dyn Executor>>,
    ret_field_types: RetFieldTypes,
    id: isize,
}

/// newExecutorMeta creates a new `executorMeta`
// go: newExecutorMeta L1
fn new_executor_meta(
    schema: Option<Schema>,
    id: isize,
    children: Vec<Box<dyn Executor>>,
) -> ExecutorMeta {
    let mut e = ExecutorMeta {
        id,
        schema,
        children,
        ret_field_types: RetFieldTypes::default(),
    };
    if let Some(schema) = &e.schema {
        let cols = &schema.columns;
        e.ret_field_types = cols.iter().map(|c| Arc::clone(&c.ret_type)).collect();
    }
    e
}

impl ExecutorMeta {
    /// NewChunkWithCapacity allows the caller to allocate the chunk with any types, capacity
    /// and max size in the pool
    // go: executorMeta.RetFieldTypes L1
    pub fn ret_field_types(&self) -> &[Arc<FieldType>] {
        &self.ret_field_types
    }

    /// ID returns the id of an executor.
    // go: executorMeta.ID L1
    pub fn id(&self) -> isize {
        self.id
    }

    /// AllChildren returns all children.
    // go: executorMeta.AllChildren L1
    pub fn all_children(&self) -> &[Box<dyn Executor>] {
        &self.children
    }

    /// SetAllChildren sets the children for an executor.
    // go: executorMeta.SetAllChildren L1
    pub fn set_all_children(&mut self, children: Vec<Box<dyn Executor>>) {
        self.children = children;
    }

    /// ChildrenLen returns the length of children.
    // go: executorMeta.ChildrenLen L1
    pub fn children_len(&self) -> isize {
        self.children.len() as isize
    }

    /// EmptyChildren judges whether the children is empty.
    // go: executorMeta.EmptyChildren L1
    pub fn empty_children(&self) -> bool {
        self.children.is_empty()
    }

    /// SetChildren sets a child for an executor.
    // go: executorMeta.SetChildren L1
    pub fn set_children(&mut self, idx: isize, ex: Box<dyn Executor>) {
        self.children[idx as usize] = ex;
    }

    /// Children returns the children for an executor.
    // go: executorMeta.Children L1
    pub fn children(&mut self, idx: isize) -> &mut Box<dyn Executor> {
        &mut self.children[idx as usize]
    }

    /// Schema returns the current BaseExecutor's schema. If it is nil, then create and return
    /// a new one.
    // go: executorMeta.Schema L1
    pub fn schema(&self) -> Schema {
        match &self.schema {
            None => tidb_expression::schema::new_schema(Vec::new()),
            Some(s) => s.clone(),
        }
    }

    /// GetSchema gets the schema.
    // go: executorMeta.GetSchema L1
    pub fn get_schema(&self) -> Option<&Schema> {
        self.schema.as_ref()
    }
}

/// executorStats is a helper to implement the stats related methods for `Executor`
///
/// The statement's normalized SQL / plan and their digests are read by the Top SQL
/// registration only (D14.2, not ported): they are not copied.
#[derive(Clone, Default)]
pub struct ExecutorStats {
    runtime_stats: Option<Arc<BasicRuntimeStats>>,
    /// The statement context, for its `IsSQLAndPlanRegistered` flag (Go keeps a pointer to
    /// the field).
    stmt_ctx: Option<Arc<StatementContext>>,
    in_restricted_sql: bool,
}

/// newExecutorStats creates a new `executorStats`
// go: newExecutorStats L2 (digests not copied: their reader is Top SQL, D14.2)
fn new_executor_stats(stmt_ctx: &Arc<StatementContext>, id: isize) -> ExecutorStats {
    let mut e = ExecutorStats {
        stmt_ctx: Some(Arc::clone(stmt_ctx)),
        in_restricted_sql: stmt_ctx.in_restricted_sql,
        runtime_stats: None,
    };

    if let Some(coll) = &stmt_ctx.runtime_stats_coll
        && id > 0
    {
        e.runtime_stats = Some(coll.get_basic_runtime_stats(id as i64));
    }

    e
}

/// `topsqlstate.TopSQLEnabled()`: Top SQL is D14.2, not ported, so it is never enabled.
// glue: stub, blocked-by:D14.2 (Top SQL)
fn top_sql_enabled() -> bool {
    false
}

impl ExecutorStats {
    /// RuntimeStats returns the runtime stats of an executor.
    // go: executorStats.RuntimeStats L1
    pub fn runtime_stats(&self) -> Option<&Arc<BasicRuntimeStats>> {
        self.runtime_stats.as_ref()
    }

    /// RegisterSQLAndPlanInExecForTopSQL registers the current SQL and Plan on top sql
    // go: executorStats.RegisterSQLAndPlanInExecForTopSQL L1
    pub fn register_sql_and_plan_in_exec_for_top_sql(&self) {
        if top_sql_enabled()
            && let Some(sc) = &self.stmt_ctx
            && sc
                .is_sql_and_plan_registered
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        {
            // topsql.RegisterSQL / RegisterPlan (D14.2).
            let _ = self.in_restricted_sql;
        }
    }
}

/// executorKillerHandler is a helper to implement the killer related methods for `Executor`.
#[derive(Clone)]
pub struct ExecutorKillerHandler {
    handler: Arc<SqlKiller>,
}

impl ExecutorKillerHandler {
    // go: executorKillerHandler.HandleSQLKillerSignal L1
    pub fn handle_sql_killer_signal(&self) -> Result<(), Error> {
        self.handler.handle_signal()
    }
}

// go: newExecutorKillerHandler L1
fn new_executor_killer_handler(handler: Arc<SqlKiller>) -> ExecutorKillerHandler {
    ExecutorKillerHandler { handler }
}

/// BaseExecutorV2 is a simplified version of `BaseExecutor`, which doesn't contain a full
/// session context
pub struct BaseExecutorV2 {
    pub meta: ExecutorMeta,
    pub killer: ExecutorKillerHandler,
    pub stats: ExecutorStats,
    pub alloc: ExecutorChunkAllocator,
}

/// NewBaseExecutorV2 creates a new BaseExecutorV2 instance.
///
/// Go reads the SQL killer from `vars.SQLKiller`; it is the session's shared part here (N2
/// `SessionShared`), passed in.
// go: NewBaseExecutorV2 L2 (the SQL killer is passed in: it is not in SessionVars, N2)
pub fn new_base_executor_v2(
    vars: &SessionVars,
    killer: &Arc<SqlKiller>,
    schema: Option<Schema>,
    id: isize,
    children: Vec<Box<dyn Executor>>,
) -> BaseExecutorV2 {
    let executor_meta = new_executor_meta(schema, id, children);
    let ret = Arc::clone(&executor_meta.ret_field_types);
    BaseExecutorV2 {
        meta: executor_meta,
        stats: new_executor_stats(&vars.stmt_ctx_arc(), id),
        alloc: new_executor_chunk_allocator(vars, ret),
        killer: new_executor_killer_handler(Arc::clone(killer)),
    }
}

impl BaseExecutorV2 {
    /// Open initializes children recursively and "childrenResults" according to children's
    /// schemas.
    // go: BaseExecutorV2.Open L1
    pub async fn open(&mut self, ctx: &ExecCtx) -> Result<(), Error> {
        for child in &mut self.meta.children {
            open(ctx, child.as_mut()).await?;
        }
        Ok(())
    }

    /// Close closes all executors and release all resources.
    // go: BaseExecutorV2.Close L1
    pub async fn close(&mut self) -> Result<(), Error> {
        let mut first_err = None;
        for src in &mut self.meta.children {
            if let Err(err) = close(src.as_mut()).await
                && first_err.is_none()
            {
                first_err = Some(err);
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Next fills multiple rows into a chunk.
    // go: BaseExecutorV2.Next L1
    pub async fn next(&mut self, _ctx: &ExecCtx, _req: &mut Chunk) -> Result<(), Error> {
        Ok(())
    }

    /// Detach detaches the current executor from the session context.
    // go: BaseExecutorV2.Detach L1
    pub fn detach(&self) -> Option<Box<dyn Executor>> {
        None
    }

    /// BuildNewBaseExecutorV2 builds a new `BaseExecutorV2` based on the configuration of the
    /// current base executor. It's used to build a new sub-executor from an existing executor.
    /// For example, the `IndexLookUpExecutor` will use this function to build
    /// `TableReaderExecutor`
    // go: BaseExecutorV2.BuildNewBaseExecutorV2 L1
    pub fn build_new_base_executor_v2(
        &self,
        stmt_runtime_stats_coll: Option<&Arc<RuntimeStatsColl>>,
        schema: Option<Schema>,
        id: isize,
        children: Vec<Box<dyn Executor>>,
    ) -> BaseExecutorV2 {
        let new_executor_meta = new_executor_meta(schema, id, children);

        let mut new_executor_stats = self.stats.clone();
        if let Some(coll) = stmt_runtime_stats_coll
            && id > 0
        {
            new_executor_stats.runtime_stats = Some(coll.get_basic_runtime_stats(id as i64));
        }

        let mut new_chunk_allocator = self.alloc.clone();
        new_chunk_allocator.ret_field_types = Arc::clone(&new_executor_meta.ret_field_types);
        BaseExecutorV2 {
            meta: new_executor_meta,
            stats: new_executor_stats,
            alloc: new_chunk_allocator,
            killer: self.killer.clone(),
        }
    }
}

/// BaseExecutor holds common information for executors.
///
/// Go's `ctx sessionctx.Context` is not kept (N2): executors read what they need from the
/// session when they are built.
pub struct BaseExecutor {
    pub base: BaseExecutorV2,
}

/// NewBaseExecutor creates a new BaseExecutor instance.
// go: NewBaseExecutor L2 (no session reference, N2; the SQL killer is passed in)
pub fn new_base_executor(
    vars: &SessionVars,
    killer: &Arc<SqlKiller>,
    schema: Option<Schema>,
    id: isize,
    children: Vec<Box<dyn Executor>>,
) -> BaseExecutor {
    BaseExecutor {
        base: new_base_executor_v2(vars, killer, schema, id, children),
    }
}

impl BaseExecutor {
    /// Ctx return ```sessionctx.Context``` of Executor
    ///
    /// Executors do not hold the session (N2): the statement context is what they share
    /// with it.
    // go: BaseExecutor.Ctx L2 (the statement context, not the session: N2)
    pub fn ctx(&self) -> Option<&Arc<StatementContext>> {
        self.base.stats.stmt_ctx.as_ref()
    }

    /// UpdateDeltaForTableID updates the delta info for the table with tableID.
    ///
    /// The write path's table delta (`TransactionContext.UpdateDeltaForTable`, block X: not
    /// ported, tidb-rs is read-only).
    // go: BaseExecutor.UpdateDeltaForTableID L2 (boundary stub, blocked-by:X)
    pub fn update_delta_for_table_id(&self, id: i64) -> Result<(), Error> {
        let _ = id;
        Err(blocked("X", "table delta of the write path"))
    }

    /// GetSysSession gets a system session context from executor.
    // go: BaseExecutor.GetSysSession L2 (boundary stub, blocked-by:D06.5)
    pub fn get_sys_session(&self) -> Result<(), Error> {
        Err(blocked("D06.5", "the domain's system session pool"))
    }

    /// ReleaseSysSession releases a system session context to executor.
    // go: BaseExecutor.ReleaseSysSession L2 (boundary stub, blocked-by:D06.5)
    pub fn release_sys_session(&self) -> Result<(), Error> {
        Err(blocked("D06.5", "the domain's system session pool"))
    }
}

/// TryNewCacheChunk tries to get a cached chunk
// go: TryNewCacheChunk L1
pub fn try_new_cache_chunk(e: &dyn Executor) -> Chunk {
    e.new_chunk()
}

/// RetTypes returns all output column types.
// go: RetTypes L1
pub fn ret_types(e: &dyn Executor) -> &[Arc<FieldType>] {
    e.ret_field_types()
}

/// NewFirstChunk creates a new chunk to buffer current executor's result.
// go: NewFirstChunk L1
pub fn new_first_chunk(e: &dyn Executor) -> Chunk {
    chunk::new(
        e.ret_field_types(),
        e.init_cap().max(0) as usize,
        e.max_chunk_size().max(0) as usize,
    )
}

/// Open is a wrapper function on e.Open(), it handles some common codes.
// go: Open L2 (panics are caught at the task boundary, M-PANIC)
pub async fn open(ctx: &ExecCtx, e: &mut dyn Executor) -> Result<(), Error> {
    e.open(ctx).await
}

/// Next is a wrapper function on e.Next(), it handles some common codes.
// go: Next L2 (panics are caught at the task boundary, M-PANIC; no tracing region)
pub async fn next(ctx: &ExecCtx, e: &mut dyn Executor, req: &mut Chunk) -> Result<(), Error> {
    let start = e.runtime_stats().map(|_| Instant::now());

    let result = async {
        e.handle_sql_killer_signal()?;

        e.register_sql_and_plan_in_exec_for_top_sql();
        e.next(ctx, req).await?;

        // recheck whether the session/query is killed during the Next()
        e.handle_sql_killer_signal()
    }
    .await;
    if let (Some(start), Some(stats)) = (start, e.runtime_stats()) {
        stats.record(start.elapsed().as_nanos() as i64, req.num_rows() as i64);
    }
    result
}

/// Close is a wrapper function on e.Close(), it handles some common codes.
// go: Close L2 (panics are caught at the task boundary, M-PANIC)
pub async fn close(e: &mut dyn Executor) -> Result<(), Error> {
    e.close().await
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tidb_base::mysql::TypeLonglong;
    use tidb_expression::{Column, Schema};
    use tidb_memory::sqlkiller::SqlKiller;
    use tidb_sessionctx::variable::session::SessionVars;
    use tidb_types::field_type::new_field_type;

    use super::new_base_executor_v2;

    /// Go assigns `cols[i].RetType` to `retFieldTypes[i]` and gives the same slice to the
    /// chunk allocator: no `FieldType` is copied (N13 §8).
    #[test]
    fn ret_field_types_share_the_schema_types() {
        let vars = SessionVars::new(Box::new(crate::adapter::tests::Acc));
        let schema = Schema {
            columns: (0..3)
                .map(|i| {
                    Arc::new(Column {
                        unique_id: i,
                        ret_type: Arc::new(new_field_type(TypeLonglong)),
                        ..Column::default()
                    })
                })
                .collect(),
            ..Schema::default()
        };
        let b = new_base_executor_v2(
            &vars,
            &SqlKiller::new(),
            Some(schema.clone()),
            1,
            Vec::new(),
        );
        assert!(Arc::ptr_eq(
            &b.meta.ret_field_types,
            &b.alloc.ret_field_types
        ));
        for (t, c) in b.meta.ret_field_types().iter().zip(&schema.columns) {
            assert!(Arc::ptr_eq(t, &c.ret_type));
        }
        let nb = b.build_new_base_executor_v2(None, Some(schema.clone()), 2, Vec::new());
        assert!(Arc::ptr_eq(
            &nb.meta.ret_field_types,
            &nb.alloc.ret_field_types
        ));
        assert!(Arc::ptr_eq(
            &nb.meta.ret_field_types()[0],
            &schema.columns[0].ret_type
        ));
        assert_eq!(b.alloc.new_chunk().num_cols(), 3);
    }
}
