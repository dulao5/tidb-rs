//! go-file: pkg/planner/core/task_base.go
//!
//! The three task types of the physical optimization (`RootTask`, `MppTask`, `CopTask`) and
//! the `base.Task` interface over them.
//!
//! - `base.Task` is the enum [`Task`] (`M-IFACE`): Go's `switch t.(type)` is a `match`. Go's
//!   tasks are pointers that `Copy()` copies shallowly; Rust tasks are values, `copy()` is
//!   Go's `Copy()` (note that `RootTask.Copy` drops `isEmpty`) and `Clone` is "the same
//!   task" (Go returns `tasks[0]` itself). The plans in a task are `Arc`s, so a copy shares
//!   them as Go's copy shares the pointers.
//! - `base.InvalidTask` (`&RootTask{}`, set by `core_init.go`) is [`Task::invalid_task`].
//! - `ConvertToRootTask` returns a `Result`: the index readers, the index merge reader and
//!   the MPP exchange are outside the first vertical slice and are boundary stubs on the path
//!   Go takes (`blocked`, error 1235).
//! - The operators' context (`ctx base.PlanContext`) is the planning context argument
//!   (`M-CTX`).

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_expression::{Column, ExprRef, Schema, column2_exprs};
use tidb_statistics::HistColl;

use crate::base::{PhysicalKind, PhysicalPlan};
use crate::core::physical_plans::{
    PhysPlanPartInfo, PhysicalProjection, PhysicalTableReader, PhysicalTableScan,
    expand_virtual_column,
};
use crate::physicalop::PhysicalPlanRef;
use crate::planctx::PlanCtx;
use crate::property::{
    MPPPartitionColumn, MPPPartitionType, PartialOrderMatchResult, PhysicalPropMatchResult,
};
use crate::util::path::IndexLookUpPushDownByType;

/// `size.SizeOfInterface`.
const SIZE_OF_INTERFACE: i64 = 16;
/// `size.SizeOfBool`.
const SIZE_OF_BOOL: i64 = 1;
/// `size.SizeOfInt`.
const SIZE_OF_INT: i64 = 8;
/// `size.SizeOfSlice`.
const SIZE_OF_SLICE: i64 = 24;
/// `size.SizeOfPointer`.
const SIZE_OF_POINTER: i64 = 8;
/// `size.SizeOfUint64`.
const SIZE_OF_UINT64: i64 = 8;

/// Task is a new version of `PhysicalPlanInfo`. It stores cost information for a task.
/// A task may be CopTask, RootTask, MPPTaskMeta or a ParallelTask.
#[derive(Clone, Debug)]
pub enum Task {
    /// `*RootTask`.
    Root(RootTask),
    /// `*CopTask`.
    Cop(CopTask),
    /// `*MppTask`.
    Mpp(MppTask),
}

impl Task {
    /// InvalidTask is just a common invalid singleton instance initialized by core's empty
    /// RootTask (`base.InvalidTask = &RootTask{}`, invalid if p is nil).
    // glue: base.InvalidTask (core_init.go)
    pub fn invalid_task() -> Task {
        Task::Root(RootTask::default())
    }

    /// Count returns current task's row count.
    // glue: dispatch (Go base.Task.Count)
    pub fn count(&self) -> f64 {
        match self {
            Task::Root(t) => t.count(),
            Task::Cop(t) => t.count(),
            Task::Mpp(t) => t.count(),
        }
    }

    /// Copy return a shallow copy of current task with the same pointer to p.
    // glue: dispatch (Go base.Task.Copy)
    pub fn copy(&self) -> Task {
        match self {
            Task::Root(t) => Task::Root(t.copy()),
            Task::Cop(t) => Task::Cop(t.copy()),
            Task::Mpp(t) => Task::Mpp(t.copy()),
        }
    }

    /// Plan returns current task's plan.
    // glue: dispatch (Go base.Task.Plan)
    pub fn plan(&self) -> Option<&PhysicalPlanRef> {
        match self {
            Task::Root(t) => t.plan(),
            Task::Cop(t) => t.plan(),
            Task::Mpp(t) => t.plan(),
        }
    }

    /// Invalid returns whether current task is invalid.
    // glue: dispatch (Go base.Task.Invalid)
    pub fn invalid(&self) -> bool {
        match self {
            Task::Root(t) => t.invalid(),
            Task::Cop(t) => t.invalid(),
            Task::Mpp(t) => t.invalid(),
        }
    }

    /// ConvertToRootTask will convert current task as root type. An `Err` is a boundary
    /// stub (see the module doc).
    // glue: dispatch (Go base.Task.ConvertToRootTask)
    pub fn convert_to_root_task(&self, ctx: &PlanCtx<'_>) -> Result<Task, Error> {
        match self {
            Task::Root(t) => Ok(t.convert_to_root_task()),
            Task::Cop(t) => t.convert_to_root_task(ctx),
            Task::Mpp(t) => t.convert_to_root_task(ctx),
        }
    }

    /// MemoryUsage returns the memory usage of current task.
    // glue: dispatch (Go base.Task.MemoryUsage)
    pub fn memory_usage(&self) -> i64 {
        match self {
            Task::Root(t) => t.memory_usage(),
            Task::Cop(t) => t.memory_usage(),
            Task::Mpp(t) => t.memory_usage(),
        }
    }
}

/// The plan of a task, which Go dereferences without a check (a nil plan panics).
// glue: Go's nil pointer dereference
fn task_plan(p: Option<&PhysicalPlanRef>) -> &PhysicalPlanRef {
    p.expect("runtime error: invalid memory address or nil pointer dereference")
}

// ************************************* RootTask Start ******************************************

/// RootTask is the final sink node of a plan graph. It should be a single goroutine on tidb.
#[derive(Clone, Debug, Default)]
pub struct RootTask {
    p: Option<PhysicalPlanRef>,
    /// isEmpty indicates if this task contains a dual table and returns empty data.
    is_empty: bool,
    // TODO: The flag 'isEmpty' is only checked by Projection and UnionAll. We should support more cases in the future.
}

impl RootTask {
    /// The root task's plan, moved out of the task.
    // glue: N1 §3 item 9.3 (the best plan leaves the task before it is written)
    pub fn take_plan(&mut self) -> Option<PhysicalPlanRef> {
        self.p.take()
    }

    /// GetPlan returns the root task's plan.
    // go: RootTask.GetPlan L1
    pub fn get_plan(&self) -> Option<&PhysicalPlanRef> {
        self.p.as_ref()
    }

    /// SetPlan sets the root task' plan.
    // go: RootTask.SetPlan L1
    pub fn set_plan(&mut self, p: PhysicalPlanRef) {
        self.p = Some(p);
    }

    /// IsEmpty indicates whether root task is empty.
    // go: RootTask.IsEmpty L1
    pub fn is_empty(&self) -> bool {
        self.is_empty
    }

    /// SetEmpty set the root task as empty.
    // go: RootTask.SetEmpty L1
    pub fn set_empty(&mut self, x: bool) {
        self.is_empty = x;
    }

    /// Copy implements Task interface.
    // go: RootTask.Copy L1
    pub fn copy(&self) -> RootTask {
        RootTask {
            p: self.p.clone(),
            is_empty: false,
        }
    }

    /// ConvertToRootTask implements Task interface.
    // go: RootTask.ConvertToRootTask L1 [M-CTX]
    pub fn convert_to_root_task(&self) -> Task {
        Task::Root(self.copy())
    }

    /// Invalid implements Task interface.
    // go: RootTask.Invalid L1
    pub fn invalid(&self) -> bool {
        self.p.is_none()
    }

    /// Count implements Task interface.
    ///
    /// Panics without a plan or stats, as Go dereferences the nil pointer.
    // go: RootTask.Count L1
    pub fn count(&self) -> f64 {
        task_plan(self.p.as_ref())
            .stats_info()
            .expect("runtime error: invalid memory address or nil pointer dereference")
            .row_count
    }

    /// Plan implements Task interface.
    // go: RootTask.Plan L1
    pub fn plan(&self) -> Option<&PhysicalPlanRef> {
        self.p.as_ref()
    }

    /// MemoryUsage return the memory usage of rootTask
    // go: RootTask.MemoryUsage L1
    pub fn memory_usage(&self) -> i64 {
        let mut sum = SIZE_OF_INTERFACE + SIZE_OF_BOOL;
        if let Some(p) = &self.p {
            sum += p.memory_usage();
        }
        sum
    }
}

// ************************************* RootTask End ******************************************

// ************************************* MPPTask Start ******************************************

/// MppTask can not :
/// 1. keep order
/// 2. support double read
/// 3. consider virtual columns.
/// 4. TODO: partition prune after close
#[derive(Clone, Debug, Default)]
pub struct MppTask {
    /// p.
    pub p: Option<PhysicalPlanRef>,

    /// partTp.
    pub part_tp: MPPPartitionType,
    /// hashCols.
    pub hash_cols: Vec<Arc<MPPPartitionColumn>>,

    /// rootTaskConds record filters of TableScan that cannot be pushed down to TiFlash.
    ///
    /// For logical plan like: HashAgg -> Selection -> TableScan, if filters in Selection cannot be pushed to TiFlash.
    /// Planner will generate physical plan like: PhysicalHashAgg -> PhysicalSelection -> TableReader -> PhysicalTableScan(cop tiflash)
    /// Because planner will make mppTask invalid directly then use copTask directly.
    ///
    /// But in DisaggregatedTiFlash mode, cop and batchCop protocol is disabled, so we have to consider this situation for mppTask.
    /// When generating PhysicalTableScan, if prop.TaskTp is RootTaskType, mppTask will be converted to rootTask,
    /// and filters in rootTaskConds will be added in a Selection which will be executed in TiDB.
    /// So physical plan be like: PhysicalHashAgg -> PhysicalSelection -> TableReader -> ExchangeSender -> PhysicalTableScan(mpp tiflash)
    pub root_task_conds: Vec<ExprRef>,
    /// tblColHists.
    pub tbl_col_hists: Option<Arc<HistColl>>,
}

impl MppTask {
    /// Count implements Task interface.
    ///
    /// Panics without a plan or stats, as Go dereferences the nil pointer.
    // go: MppTask.Count L1
    pub fn count(&self) -> f64 {
        task_plan(self.p.as_ref())
            .stats_info()
            .expect("runtime error: invalid memory address or nil pointer dereference")
            .row_count
    }

    /// Copy implements Task interface.
    // go: MppTask.Copy L1
    pub fn copy(&self) -> MppTask {
        self.clone()
    }

    /// Plan implements Task interface.
    // go: MppTask.Plan L1
    pub fn plan(&self) -> Option<&PhysicalPlanRef> {
        self.p.as_ref()
    }

    /// Invalid implements Task interface.
    // go: MppTask.Invalid L1
    pub fn invalid(&self) -> bool {
        self.p.is_none()
    }

    /// ConvertToRootTask implements Task interface.
    // go: MppTask.ConvertToRootTask L1 [M-CTX]
    pub fn convert_to_root_task(&self, ctx: &PlanCtx<'_>) -> Result<Task, Error> {
        Ok(Task::Root(self.copy().convert_to_root_task_impl(ctx)?))
    }

    /// MemoryUsage return the memory usage of mppTask
    // go: MppTask.MemoryUsage L1
    pub fn memory_usage(&self) -> i64 {
        let mut sum = SIZE_OF_INTERFACE
            + SIZE_OF_INT
            + SIZE_OF_SLICE
            + self.hash_cols.capacity() as i64 * SIZE_OF_POINTER;
        if let Some(p) = &self.p {
            sum += p.memory_usage();
        }
        sum
    }

    /// ConvertToRootTaskImpl implements Task interface.
    ///
    /// The MPP gather (`PhysicalExchangeSender` under a TiFlash `PhysicalTableReader`,
    /// `collectPartitionInfosFromMPPPlan`) is outside the first vertical slice.
    // glue: stub of MppTask.ConvertToRootTaskImpl, blocked-by:D10.1 (the MPP exchange sender and the TiFlash reader are outside the first slice)
    pub fn convert_to_root_task_impl(&mut self, ctx: &PlanCtx<'_>) -> Result<RootTask, Error> {
        let _ = ctx;
        Err(blocked(
            "D10.1",
            "MppTask.ConvertToRootTaskImpl (the MPP exchange sender)",
        ))
    }
}

// ************************************* MPPTask End ******************************************

// ************************************* CopTask Start ******************************************

/// CopTask is a task that runs in a distributed kv store.
/// TODO: In future, we should split copTask to indexTask and tableTask.
///
/// The fields are written by the access path code of `FindBestTask` (D10.2), hence public.
#[derive(Clone, Debug, Default)]
pub struct CopTask {
    /// indexPlan.
    pub index_plan: Option<PhysicalPlanRef>,
    /// tablePlan.
    pub table_plan: Option<PhysicalPlanRef>,
    /// indexLookUpPushDownBy.
    pub index_look_up_push_down_by: IndexLookUpPushDownByType,
    /// indexPlanFinished means we have finished index plan.
    pub index_plan_finished: bool,
    /// keepOrder indicates if the plan scans data by order.
    pub keep_order: bool,
    /// needExtraProj means an extra prune is needed because
    /// in double read / index merge cases, they may output one more column for handle(row id).
    pub need_extra_proj: bool,
    /// originSchema is the target schema to be projected to when needExtraProj is true.
    pub origin_schema: Option<Arc<Schema>>,

    /// extraHandleCol.
    pub extra_handle_col: Option<Arc<Column>>,
    /// commonHandleCols.
    pub common_handle_cols: Vec<Arc<Column>>,
    /// tblColHists stores the original stats of DataSource, it is used to get
    /// average row width when computing network cost.
    pub tbl_col_hists: Option<Arc<HistColl>>,
    /// tblCols stores the original columns of DataSource before being pruned, it
    /// is used to compute average row width when computing scan cost.
    pub tbl_cols: Vec<Arc<Column>>,

    /// idxMergePartPlans. `None` is Go's nil slice.
    pub idx_merge_part_plans: Option<Vec<PhysicalPlanRef>>,
    /// idxMergeIsIntersection.
    pub idx_merge_is_intersection: bool,
    /// idxMergeAccessMVIndex.
    pub idx_merge_access_mv_index: bool,
    /// idxMergeMatchWithAdvisorySortItems indicates the IndexMerge property matching
    /// used advisory sort items (i.e. no SortItems but AdvisorySortItems was set).
    pub idx_merge_match_with_advisory_sort_items: bool,
    /// idxMergePartPlansMatchResults stores each partial path's matchProperty result.
    /// Set by convertToIndexMergeScan. Length equals len(idxMergePartPlans)
    /// or 0.
    /// 0 length may be caused by cases like Intersection type IndexMerge, which can't satisfy any order property. When
    /// its length is 0, it should be considered as all property.PropNotMatched.
    pub idx_merge_part_plans_match_results: Vec<PhysicalPropMatchResult>,

    /// rootTaskConds stores select conditions containing virtual columns.
    /// These conditions can't push to TiKV, so we have to add a selection for rootTask
    pub root_task_conds: Vec<ExprRef>,

    /// For table partition.
    pub phys_plan_part_info: Option<Arc<PhysPlanPartInfo>>,

    /// expectCnt is the expected row count of upper task, 0 for unlimited.
    /// It's used for deciding whether using paging distsql.
    pub expect_cnt: u64,

    /// partialOrderMatchResult stores the match result for partial order optimization.
    pub partial_order_match_result: Option<Arc<PartialOrderMatchResult>>,
}

impl CopTask {
    /// `len(t.idxMergePartPlans)`.
    // glue: Go's len of a nil slice
    pub fn idx_merge_part_plans_len(&self) -> usize {
        self.idx_merge_part_plans.as_ref().map_or(0, Vec::len)
    }

    /// Invalid implements Task interface.
    // go: CopTask.Invalid L1
    pub fn invalid(&self) -> bool {
        self.table_plan.is_none()
            && self.index_plan.is_none()
            && self.idx_merge_part_plans_len() == 0
    }

    /// Count implements Task interface.
    ///
    /// Panics without the plan or stats, as Go dereferences the nil pointer.
    // go: CopTask.Count L1
    pub fn count(&self) -> f64 {
        let p = if self.index_plan_finished {
            task_plan(self.table_plan.as_ref())
        } else {
            task_plan(self.index_plan.as_ref())
        };
        p.stats_info()
            .expect("runtime error: invalid memory address or nil pointer dereference")
            .row_count
    }

    /// Copy implements Task interface.
    // go: CopTask.Copy L1
    pub fn copy(&self) -> CopTask {
        self.clone()
    }

    /// Plan implements Task interface.
    /// copTask plan should be careful with indexMergeReader, whose real plan is stored in
    /// idxMergePartPlans, when its indexPlanFinished is marked with false.
    // go: CopTask.Plan L1
    pub fn plan(&self) -> Option<&PhysicalPlanRef> {
        if self.index_plan_finished {
            return self.table_plan.as_ref();
        }
        self.index_plan.as_ref()
    }

    /// `Plan()` moved out of the task (the task is consumed).
    // glue: N1 §3 item 9.3 (the best plan leaves the task before it is written)
    pub fn into_plan(self) -> Option<PhysicalPlanRef> {
        if self.index_plan_finished {
            return self.table_plan;
        }
        self.index_plan
    }

    /// MemoryUsage return the memory usage of copTask
    // go: CopTask.MemoryUsage L1
    pub fn memory_usage(&self) -> i64 {
        let idx_merge_cap = self.idx_merge_part_plans.as_ref().map_or(0, Vec::capacity) as i64;
        let mut sum = SIZE_OF_INTERFACE
            * (2 + idx_merge_cap + self.root_task_conds.capacity() as i64)
            + SIZE_OF_BOOL * 3
            + SIZE_OF_UINT64
            + SIZE_OF_POINTER
                * (3 + self.common_handle_cols.capacity() as i64 + self.tbl_cols.capacity() as i64)
            + SIZE_OF_SLICE * 4
            + PhysPlanPartInfo::memory_usage(self.phys_plan_part_info.as_deref());
        if let Some(p) = &self.index_plan {
            sum += p.memory_usage();
        }
        if let Some(p) = &self.table_plan {
            sum += p.memory_usage();
        }
        if let Some(s) = &self.origin_schema {
            sum += s.memory_usage();
        }
        if let Some(c) = &self.extra_handle_col {
            sum += c.memory_usage();
        }

        for col in &self.common_handle_cols {
            sum += col.memory_usage();
        }
        for col in &self.tbl_cols {
            sum += col.memory_usage();
        }
        for p in self.idx_merge_part_plans.iter().flatten() {
            sum += p.memory_usage();
        }
        for expr in &self.root_task_conds {
            sum += expr.memory_usage();
        }
        sum
    }

    /// ConvertToRootTask implements Task interface.
    // go: CopTask.ConvertToRootTask L1 [M-CTX]
    pub fn convert_to_root_task(&self, ctx: &PlanCtx<'_>) -> Result<Task, Error> {
        // copy one to avoid changing itself.
        Ok(Task::Root(self.copy().convert_to_root_task_impl(ctx)?))
    }

    /// `convertToRootTaskImpl`.
    ///
    /// - Go rewrites the leaf `PhysicalTableScan`'s `Columns` and schema in place with
    ///   `ExpandVirtualColumn`. Without virtual generated columns the rewrite gives the same
    ///   columns back, so Rust computes it on a copy and leaves the (shared) scan alone. When
    ///   the scan does gain columns, Go's in-place write is also seen by every other task
    ///   sharing the scan (the next conversion no longer adds the extra projection): that
    ///   aliasing has no representation on the `Arc` tree yet (spec §5), so the conversion
    ///   is a boundary stub.
    /// - The index merge reader, the index lookup reader and the index reader are outside the
    ///   first vertical slice (boundary stubs on the branch Go takes); a join in the table
    ///   plan (`PhysicalHashJoin`, TiFlash) likewise.
    // go: CopTask.convertToRootTaskImpl L2 (index readers / joins / virtual column expansion are blocked-by:D10.1)
    pub fn convert_to_root_task_impl(&mut self, ctx: &PlanCtx<'_>) -> Result<RootTask, Error> {
        // copTasks are run in parallel, to make the estimated cost closer to execution time, we amortize
        // the cost to cop iterator workers. According to `CopClient::Send`, the concurrency
        // is Min(DistSQLScanConcurrency, numRegionsInvolvedInScan), since we cannot infer
        // the number of regions involved, we simply use DistSQLScanConcurrency.
        self.finish_index_plan()?;
        // Network cost of transferring rows of table scan to TiDB.
        if let Some(table_plan) = &self.table_plan {
            let ts = leaf_table_scan(table_plan)?;
            let prev_column_len = ts.columns.len();
            let mut schema = ts.schema().clone_schema();
            let table_columns = ts.table.columns.as_deref().unwrap_or(&[]);
            let columns = expand_virtual_column(&ts.columns, &mut schema, table_columns);
            // Go: `if !t.needExtraProj && len(ts.Columns) > prevColumnLen` adds a projection to
            // make sure not to output extract columns (`t.originSchema = prevSchema`).
            if columns.len() > prev_column_len {
                // glue: blocked-by:D10.1 (Go expands the shared scan in place, seen by every task holding it; spec §5)
                return Err(blocked(
                    "D10.1",
                    "convertToRootTask of a table scan with virtual generated columns (in-place expansion of a shared scan)",
                ));
            }
        }
        let mut new_task = RootTask::default();
        if self.idx_merge_part_plans.is_some() {
            // glue: blocked-by:D10.1 (PhysicalIndexMergeReader is outside the first slice)
            return Err(blocked(
                "D10.1",
                "convertToRootTask of an index merge (PhysicalIndexMergeReader)",
            ));
        }
        if self.index_plan.is_some() && self.table_plan.is_some() {
            // glue: blocked-by:D10.1 (buildIndexLookUpTask, PhysicalIndexLookUpReader is outside the first slice)
            return Err(blocked(
                "D10.1",
                "convertToRootTask of an index lookup (PhysicalIndexLookUpReader)",
            ));
        } else if self.index_plan.is_some() {
            // glue: blocked-by:D10.1 (PhysicalIndexReader is outside the first slice)
            return Err(blocked(
                "D10.1",
                "convertToRootTask of an index read (PhysicalIndexReader)",
            ));
        } else {
            let table_plan = task_plan(self.table_plan.as_ref());
            let ts = leaf_table_scan(table_plan)?;
            let mut p = PhysicalTableReader {
                table_plan: Some(Arc::clone(table_plan)),
                store_type: ts.store_type,
                is_common_handle: ts.table.is_common_handle,
                ..Default::default()
            }
            .init(ctx, table_plan.query_block_offset());
            p.plan_part_info = self.phys_plan_part_info.as_ref().map(Arc::clone);
            p.set_stats(table_plan.stats_info().map(Arc::clone));

            // If agg was pushed down in Attach2Task(), the partial agg was placed on the top of tablePlan, the final agg was
            // placed above the PhysicalTableReader, and the schema should have been set correctly for them, the schema of
            // partial agg contains the columns needed by the final agg.
            // If we add the projection here, the projection will be between the final agg and the partial agg, then the
            // schema will be broken, the final agg will fail to find needed columns in ResolveIndices().
            // Besides, the agg would only be pushed down if it doesn't contain virtual columns, so virtual column should not be affected.
            //
            // PhysicalHashAgg / PhysicalStreamAgg are not ported (aggregation is outside the
            // first slice), so no table plan is one.
            let agg_pushed_down = false;

            if self.need_extra_proj && !agg_pushed_down {
                let origin_schema = self
                    .origin_schema
                    .as_ref()
                    .expect("runtime error: invalid memory address or nil pointer dereference");
                let mut proj = PhysicalProjection {
                    exprs: column2_exprs(&origin_schema.columns),
                    ..Default::default()
                }
                .init(
                    ctx,
                    ts.stats_info().map(Arc::clone),
                    ts.query_block_offset(),
                    vec![None],
                );
                proj.set_schema(origin_schema.clone_schema());
                proj.set_children(vec![Arc::new(p)]);
                new_task.set_plan(Arc::new(proj));
            } else {
                new_task.set_plan(Arc::new(p));
            }
        }

        self.handle_root_task_conds(ctx, &mut new_task)?;
        Ok(new_task)
    }
}

/// Go's walk from the table plan's root to its leaf `PhysicalTableScan`
/// (`tp.(*PhysicalTableScan)`); a node with several children is a `PhysicalHashJoin`
/// (TiFlash), outside the first slice.
// glue: the leaf walk of CopTask.convertToRootTaskImpl
fn leaf_table_scan(table_plan: &PhysicalPlanRef) -> Result<&PhysicalTableScan, Error> {
    let mut tp = table_plan;
    while !tp.children().is_empty() {
        if tp.children().len() == 1 {
            tp = &tp.children()[0];
        } else {
            // glue: blocked-by:D10.1 (PhysicalHashJoin in a TiFlash table plan is outside the first slice)
            return Err(blocked(
                "D10.1",
                "a join in the table plan of a cop task (PhysicalHashJoin)",
            ));
        }
    }
    if tp.kind() != PhysicalKind::TableScan {
        panic!(
            "interface conversion: base.PhysicalPlan is {}, not *core.PhysicalTableScan",
            tp.tp()
        );
    }
    Ok(tp
        .as_any()
        .downcast_ref::<PhysicalTableScan>()
        .expect("PhysicalTableScan"))
}

// ************************************* CopTask End ******************************************
