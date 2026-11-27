//! go-file: pkg/planner/core/task.go
//!
//! `Attach2Task` of the operators of the first vertical slice (Selection, Projection,
//! Limit, TopN, Sort, NominalSort and the `BasePhysicalPlan` default) and the cop task
//! helpers they use. The join, aggregation, window, union, expand, CTE and MPP parts of the
//! file are outside the slice and not ported yet.
//!
//! - Go's interface method `p.Attach2Task(tasks...)` is the dispatch function
//!   [`attach2_task`] over `p.kind()` (`M-IFACE`). The plan is passed as an owned
//!   [`PhysicalPlanRef`]: `exhaustPhysicalPlans` builds a fresh candidate for every call, so
//!   `make_mut_plan` writes it in place (Go writes `p` in place).
//! - `Attach2Task` returns a `Result`: the index readers, the index merge reader and the
//!   MPP exchange are boundary stubs on the branch Go takes (`blocked`, error 1235), and the
//!   push-down checks read the push-down context, which is a boundary until the planning
//!   context has a store client (D09.13).
//! - The context is the planning context argument (`M-CTX`).

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::{blocked, blocked_by};
use tidb_base::gostd::math;
use tidb_base::kv::{self, StoreType};
use tidb_chunk::codec::estimate_type_width;
use tidb_expr_pushdown::infer_pushdown::can_exprs_push_down;
use tidb_expression::util::extract_columns;
use tidb_expression::{Column, Expr, ExprRef};
use tidb_store::util::paging::calculate_seek_cnt;

use crate::base::{PhysicalKind, PhysicalPlan};
use crate::cardinality::row_size::get_avg_row_size_data_in_disk_by_rows;
use crate::cardinality::selectivity::selectivity;
use crate::core::find_best_task::get_prop_by_order_by_items;
use crate::core::physical_plans::{
    NominalSort, PhysicalLimit, PhysicalProjection, PhysicalSelection, PhysicalSort,
    PhysicalTableScan, PhysicalTopN, expand_virtual_column,
};
use crate::core::task_base::{CopTask, MppTask, RootTask, Task};
use crate::physicalop::{PhysicalPlanRef, make_mut_plan};
use crate::planctx::PlanCtx;
use crate::property::{SortItem, StatsInfo};
use crate::util::ByItems;
use crate::util::misc::derive_limit_stats;

/// `cost.SelectionFactor`: the default selectivity of a filter.
const SELECTION_FACTOR: f64 = 0.8;

/// `infoschema.ClusterTableSlowLog`.
const CLUSTER_TABLE_SLOW_LOG: &str = "CLUSTER_SLOW_QUERY";

/// The plan of a task or node, which Go dereferences without a check.
// glue: Go's nil pointer dereference
fn deref<T>(p: Option<T>) -> T {
    p.expect("runtime error: invalid memory address or nil pointer dereference")
}

/// The stats of a plan, which Go dereferences without a check.
// glue: Go's nil pointer dereference
fn stats_of(p: &dyn PhysicalPlan) -> &Arc<StatsInfo> {
    deref(p.stats_info())
}

/// `p.(*T)` on a plan of kind `T` (`M-IFACE`).
// glue: Go type assertion
fn downcast<T: 'static>(p: &dyn PhysicalPlan) -> &T {
    p.as_any().downcast_ref::<T>().expect("type assertion")
}

/// `p.(*T)` for writing: the node in place while the caller owns it (`M-PLAN`).
// glue: Go type assertion
fn downcast_mut<T: 'static>(p: &mut PhysicalPlanRef) -> &mut T {
    make_mut_plan(p)
        .as_any_mut()
        .downcast_mut::<T>()
        .expect("type assertion")
}

/// `p.Attach2Task(tasks...)` (`base.PhysicalPlan.Attach2Task`): makes the current physical
/// plan the father of the task's physical plan and updates the cost of the current task.
/// If the child's task is cop task, some operator may close this task and return a new
/// rootTask.
///
/// Go's `PointGetPlan.Attach2Task` returns a nil task (a point get is never attached: it is a
/// task of its own); a nil task has no value here, so that arm panics, as the caller's first
/// use of Go's nil task would.
// glue: dispatch (Go base.PhysicalPlan.Attach2Task)
pub fn attach2_task(ctx: &PlanCtx<'_>, p: PhysicalPlanRef, tasks: &[Task]) -> Result<Task, Error> {
    match p.kind() {
        PhysicalKind::Selection => attach2_task_selection(ctx, p, tasks),
        PhysicalKind::Projection => attach2_task_projection(ctx, p, tasks),
        PhysicalKind::Limit => attach2_task_limit(ctx, p, tasks),
        PhysicalKind::TopN => attach2_task_top_n(ctx, p, tasks),
        PhysicalKind::Sort => Ok(attach2_task_sort(p, tasks)),
        PhysicalKind::NominalSort => Ok(attach2_task_nominal_sort(p, tasks)),
        PhysicalKind::PointGet => {
            panic!("runtime error: invalid memory address or nil pointer dereference")
        }
        _ => crate::physicalop::base_physical_plan::attach2_task(ctx, p, tasks),
    }
}

/// `attachPlan2Task`: puts `p` on top of the task's plan (the table or index side of a cop
/// task, as `indexPlanFinished` says).
///
/// The task's plan is moved into `p`'s children, so it keeps its reference count.
// go: attachPlan2Task L1 [M-PLAN, M-IFACE]
pub fn attach_plan2_task(mut p: PhysicalPlanRef, mut t: Task) -> Task {
    match &mut t {
        Task::Cop(v) => {
            if v.index_plan_finished {
                let child = deref(v.table_plan.take());
                make_mut_plan(&mut p).set_children(vec![child]);
                v.table_plan = Some(p);
            } else {
                let child = deref(v.index_plan.take());
                make_mut_plan(&mut p).set_children(vec![child]);
                v.index_plan = Some(p);
            }
        }
        Task::Root(v) => {
            let child = Arc::clone(deref(v.get_plan()));
            make_mut_plan(&mut p).set_children(vec![child]);
            v.set_plan(p);
        }
        Task::Mpp(v) => {
            let child = deref(v.p.take());
            make_mut_plan(&mut p).set_children(vec![child]);
            v.p = Some(p);
        }
    }
    t
}

impl CopTask {
    /// finishIndexPlan means we no longer add plan to index plan, and compute the network cost for it.
    ///
    /// Preserves `origin_stats.stats_version` in a cloned `StatsInfo` for the table scan without
    /// mutating the index plan's shared `StatsInfo` (N1 §3.4, D10.2 spec §11.4).
    // go: CopTask.finishIndexPlan L1 [M-PLAN]
    pub fn finish_index_plan(&mut self) -> Result<(), Error> {
        if self.index_plan_finished {
            return Ok(());
        }
        self.index_plan_finished = true;
        // index merge case is specially handled for now.
        // We need a elegant way to solve the stats of index merge in this case.
        if let (Some(table_plan), Some(index_plan)) = (&mut self.table_plan, &self.index_plan) {
            let index_stats = index_plan.stats_info().map(Arc::clone);
            let ts = make_mut_plan(table_plan)
                .as_any_mut()
                .downcast_mut::<PhysicalTableScan>()
                .expect("interface conversion: base.PhysicalPlan is not *core.PhysicalTableScan");
            let origin_stats_version = ts.stats_info().map(|s| s.stats_version);
            if let Some(stats) = index_stats {
                let mut new_stats = (*stats).clone();
                if let Some(ver) = origin_stats_version {
                    // keep the original stats version
                    new_stats.stats_version = ver;
                }
                ts.set_stats(Some(Arc::new(new_stats)));
            } else {
                ts.set_stats(None);
            }
        }
        Ok(())
    }

    /// `getStoreType`.
    // go: CopTask.getStoreType L1 [M-IFACE]
    pub fn get_store_type(&self) -> StoreType {
        let Some(table_plan) = &self.table_plan else {
            return kv::TiKV;
        };
        let mut tp = table_plan;
        while !tp.children().is_empty() {
            if tp.children().len() > 1 {
                return kv::TiFlash;
            }
            tp = &tp.children()[0];
        }
        if tp.kind() == PhysicalKind::TableScan {
            return downcast::<PhysicalTableScan>(tp.as_ref()).store_type;
        }
        kv::TiKV
    }
}

/// RowSize for cost model ver2 is simplified, always use this function to calculate row size.
// go: getAvgRowSize L1
pub fn get_avg_row_size(stats: &StatsInfo, cols: &[Arc<Column>]) -> f64 {
    let mut size = 0.0;
    if let Some(hist_coll) = &stats.hist_coll {
        size = math::max(get_avg_row_size_data_in_disk_by_rows(hist_coll, cols), 0.0);
    } else {
        // Estimate using just the type info.
        for col in cols {
            size += math::max(estimate_type_width(col.get_static_type()) as f64, 0.0);
        }
    }
    size
}

/// `getProj`: a projection of all columns of `p`, on top of `p`.
// go: getProj L1 [M-CTX]
pub fn get_proj(ctx: &PlanCtx<'_>, p: PhysicalPlanRef) -> PhysicalProjection {
    let mut proj = PhysicalProjection {
        exprs: Vec::with_capacity(p.schema().columns.len()),
        ..Default::default()
    }
    .init(
        ctx,
        p.stats_info().map(Arc::clone),
        p.query_block_offset(),
        Vec::new(),
    );
    for col in &p.schema().columns {
        proj.exprs.push(Arc::new(Expr::Column((**col).clone())));
    }
    proj.set_schema(p.schema().clone_schema());
    proj.set_children(vec![p]);
    proj
}

/// `appendExpr`: appends `expr` to the projection and a new column for it to its schema.
///
/// Go's column shares the expression's `*FieldType`; the column gets a copy (`M-FT`).
// go: appendExpr L1 [M-CTX, M-FT]
pub fn append_expr(ctx: &PlanCtx<'_>, p: &mut PhysicalProjection, expr: ExprRef) -> Arc<Column> {
    p.exprs.push(Arc::clone(&expr));

    let ectx = ctx.eval_ctx();
    let col = Column {
        unique_id: ctx.vars.alloc_plan_column_id(),
        ret_type: Arc::new(expr.get_type(&*ectx).into_owned()),
        ..Column::default()
    };
    col.collation_info.set_coercibility(expr.coercibility());
    let col = Arc::new(col);
    let mut schema = p.schema().clone_schema();
    schema.append(vec![Arc::clone(&col)]);
    p.set_schema(schema);
    col
}

/// `extractRows`: the row count of the leaves under `p`'s children.
// go: extractRows L1
pub fn extract_rows(p: &dyn PhysicalPlan) -> f64 {
    let mut f = 0.0;
    for c in p.children() {
        if !c.children().is_empty() {
            f += extract_rows(c.as_ref());
        } else {
            f += stats_of(c.as_ref()).row_count;
        }
    }
    f
}

/// calcPagingCost calculates the cost for paging processing which may increase the seekCnt and reduce scanned rows.
// go: calcPagingCost L1 [M-CTX, M-F2I, M-FMA]
pub fn calc_paging_cost(
    ctx: &PlanCtx<'_>,
    index_plan: &dyn PhysicalPlan,
    mut expect_cnt: u64,
) -> f64 {
    let sess_vars = &*ctx.vars;
    let index_rows = index_plan.stats_count();
    let source_rows = extract_rows(index_plan);
    // with paging, the scanned rows is always less than or equal to source rows.
    if tidb_base::gostd::conv::f64_to_u64(source_rows) < expect_cnt {
        // [M-F2I]
        expect_cnt = tidb_base::gostd::conv::f64_to_u64(source_rows); // [M-F2I]
    }
    let seek_cnt = calculate_seek_cnt(expect_cnt);
    let mut index_selectivity = 1.0;
    if source_rows > index_rows {
        index_selectivity = index_rows / source_rows;
    }
    let seek_factor = sess_vars.get_seek_factor(None);
    let cpu_factor = sess_vars.get_cpu_factor();
    // [M-FMA] task.go:583
    let mut paging_cst = paging_madd(expect_cnt as f64, cpu_factor, seek_cnt * seek_factor);
    // pagingCst *= indexSelectivity (fused with the subtraction below where Go fuses it)

    // we want the diff between idxCst and pagingCst here,
    // however, the idxCst does not contain seekFactor, so a seekFactor needs to be removed
    // [M-FMA] task.go:588
    paging_cst = paging_msub(
        paging_cst,
        index_selectivity,
        sess_vars.get_seek_factor(None),
    );
    math::max(paging_cst, 0.0)
}

/// `x * y + z` the way Go gc compiles task.go:583 (gc multiplies `seekCnt*seekFactor` first
/// and fuses the second product): fused on arm64, ppc64le, s390x, riscv64,
/// loong64, not on amd64 (GOAMD64=v1) (mapping rule `M-FMA`, tracker
/// `data/go_fma_sites.tsv`). Rust never contracts by itself.
// glue: M-FMA helper
#[inline(always)]
fn paging_madd(x: f64, y: f64, z: f64) -> f64 {
    #[cfg(any(
        target_arch = "aarch64",
        all(target_arch = "powerpc64", target_endian = "little"),
        target_arch = "s390x",
        target_arch = "riscv64",
        target_arch = "loongarch64"
    ))]
    {
        x.mul_add(y, z)
    }
    #[cfg(not(any(
        target_arch = "aarch64",
        all(target_arch = "powerpc64", target_endian = "little"),
        target_arch = "s390x",
        target_arch = "riscv64",
        target_arch = "loongarch64"
    )))]
    {
        x * y + z
    }
}

/// `(x * y) - z` the way Go gc compiles `pagingCst *= indexSelectivity` with task.go:588
/// (`pagingCst-seekFactor`): fused on the same targets as task.go:583 (`FNMSUBD` on arm64),
/// a separate multiply and subtract on amd64 (GOAMD64=v1) (mapping rule `M-FMA`).
// glue: M-FMA helper
#[inline(always)]
fn paging_msub(x: f64, y: f64, z: f64) -> f64 {
    #[cfg(any(
        target_arch = "aarch64",
        all(target_arch = "powerpc64", target_endian = "little"),
        target_arch = "s390x",
        target_arch = "riscv64",
        target_arch = "loongarch64"
    ))]
    {
        x.mul_add(y, -z)
    }
    #[cfg(not(any(
        target_arch = "aarch64",
        all(target_arch = "powerpc64", target_endian = "little"),
        target_arch = "s390x",
        target_arch = "riscv64",
        target_arch = "loongarch64"
    )))]
    {
        let p = x * y;
        p - z
    }
}

impl CopTask {
    /// `handleRootTaskConds`: a Selection of the conditions that cannot be pushed to the
    /// store, on top of the root task.
    ///
    /// Go falls back to `SelectionFactor` on any selectivity error; a boundary stub
    /// (`blocked`) of the selectivity code is returned instead, so that the fallback is not
    /// taken silently.
    // go: CopTask.handleRootTaskConds L1 [M-CTX]
    pub fn handle_root_task_conds(
        &self,
        ctx: &PlanCtx<'_>,
        new_task: &mut RootTask,
    ) -> Result<(), Error> {
        if !self.root_task_conds.is_empty() {
            let coll = deref(self.tbl_col_hists.as_ref());
            let sel_factor = match selectivity(ctx, coll, &self.root_task_conds, &[]) {
                Ok((s, _)) => s,
                Err(err) if blocked_by(&err).is_some() => return Err(err),
                Err(err) => {
                    tidb_base::log_debug!(
                        "calculate selectivity failed, use selection factor",
                        "error" => &err,
                    );
                    SELECTION_FACTOR
                }
            };
            let plan = Arc::clone(deref(new_task.get_plan()));
            let mut sel = PhysicalSelection {
                conditions: self.root_task_conds.clone(),
                ..Default::default()
            }
            .init(
                ctx,
                Some(Arc::new(stats_of(plan.as_ref()).scale(sel_factor))),
                plan.query_block_offset(),
                Vec::new(),
            );
            sel.from_data_source = true;
            sel.set_children(vec![plan]);
            new_task.set_plan(Arc::new(sel));
        }
        Ok(())
    }
}

/// Attach2Task attach limit to different cases.
/// For Normal Index Lookup
/// 1: attach the limit to table side or index side of normal index lookup cop task. (normal case, old code, no more
/// explanation here)
///
/// For Index Merge:
/// 2: attach the limit to **table** side for index merge intersection case, cause intersection will invalidate the
/// fetched limit+offset rows from each partial index plan, you can not decide how many you want in advance for partial
/// index path, actually. After we sink limit to table side, we still need an upper root limit to control the real limit
/// count admission.
///
/// 3: attach the limit to **index** side for index merge union case, because each index plan will output the fetched
/// limit+offset (* N path) rows, you still need an embedded pushedLimit inside index merge reader to cut it down.
///
/// 4: attach the limit to the TOP of root index merge operator if there is some root condition exists for index merge
/// intersection/union case.
///
/// The index merge cases (2, 3, 4) are outside the first slice: a cop task with partial
/// index merge plans is a boundary stub. Go's pushed-down Limit shares its child's schema
/// object ("don't use clone()"); it gets a copy here, which is equal: the virtual column
/// expansion that would change the shared schema is itself a boundary stub
/// (`convertToRootTaskImpl`).
// go: PhysicalLimit.Attach2Task L2 (the index merge branches are blocked-by:D10.1)
fn attach2_task_limit(
    ctx: &PlanCtx<'_>,
    p: PhysicalPlanRef,
    tasks: &[Task],
) -> Result<Task, Error> {
    let limit = downcast::<PhysicalLimit>(p.as_ref());
    let mut t = tasks[0].copy();
    let mut new_partition_by: Vec<SortItem> = Vec::with_capacity(limit.get_partition_by().len());
    for expr in limit.get_partition_by() {
        new_partition_by.push(expr.clone_item());
    }

    let mut sunk = false;
    if let Task::Cop(cop) = &t {
        let mut cop = cop.clone();
        if cop.idx_merge_part_plans_len() == 0 {
            // For double read which requires order being kept, the limit cannot be pushed down to the table side,
            // because handles would be reordered before being sent to table scan.
            if (!cop.keep_order || !cop.index_plan_finished || cop.index_plan.is_none())
                && cop.root_task_conds.is_empty()
            {
                // When limit is pushed down, we should remove its offset.
                let new_count = limit.offset.wrapping_add(limit.count);
                let child = Arc::clone(deref(cop.plan()));
                let child_profile = stats_of(child.as_ref());
                // Strictly speaking, for the row count of stats, we should multiply newCount with "regionNum",
                // but "regionNum" is unknown since the copTask can be a double read, so we ignore it now.
                let stats = derive_limit_stats(child_profile, new_count as f64);
                let mut pushed_down_limit = PhysicalLimit {
                    partition_by: new_partition_by.clone(),
                    count: new_count,
                    ..Default::default()
                }
                .init(
                    ctx,
                    Some(Arc::new(stats)),
                    limit.query_block_offset(),
                    Vec::new(),
                );
                // Don't use clone() so that Limit and its children share the same schema. Otherwise the virtual generated column may not be resolved right.
                // (Go sets it after attaching, from the same child.)
                pushed_down_limit.set_schema(child.schema().clone_schema());
                drop(child);
                let Task::Cop(c) = attach_plan2_task(Arc::new(pushed_down_limit), Task::Cop(cop))
                else {
                    unreachable!("attachPlan2Task returns the task it is given")
                };
                cop = c;
            }
            t = cop.convert_to_root_task(ctx)?;
            sunk = sink_into_index_look_up(ctx, limit, &mut t);
        } else {
            // glue: blocked-by:D10.1 (the limit push down into an index merge reader; index merge is outside the first slice)
            return Err(blocked(
                "D10.1",
                "PhysicalLimit.Attach2Task on an index merge (sinkIntoIndexMerge)",
            ));
        }
    } else if let Task::Mpp(mpp) = &t {
        let new_count = limit.offset.wrapping_add(limit.count);
        let child = Arc::clone(deref(mpp.plan()));
        let child_profile = stats_of(child.as_ref());
        let stats = derive_limit_stats(child_profile, new_count as f64);
        let mut pushed_down_limit = PhysicalLimit {
            count: new_count,
            partition_by: new_partition_by,
            ..Default::default()
        }
        .init(
            ctx,
            Some(Arc::new(stats)),
            limit.query_block_offset(),
            Vec::new(),
        );
        pushed_down_limit.set_schema(child.schema().clone_schema());
        drop(child);
        let mpp = attach_plan2_task(Arc::new(pushed_down_limit), Task::Mpp(mpp.clone()));
        t = mpp.convert_to_root_task(ctx)?;
    }
    if sunk {
        return Ok(t);
    }
    // Skip limit with partition on the root. This is a derived topN and window function
    // will take care of the filter.
    if !limit.get_partition_by().is_empty() {
        return Ok(t);
    }
    Ok(attach_plan2_task(p, t))
}

/// `buildIndexLookUpTask`.
// go: buildIndexLookUpTask L1 [M-CTX, M-PLAN]
pub(crate) fn build_index_look_up_task(ctx: &PlanCtx<'_>, t: &CopTask) -> RootTask {
    let mut new_task = RootTask::default();
    let table_plan = Arc::clone(deref(t.table_plan.as_ref()));
    let index_plan = Arc::clone(deref(t.index_plan.as_ref()));
    let qb_offset = table_plan.query_block_offset();
    let p = crate::core::physical_plans::PhysicalIndexLookUpReader {
        table_plan: Some(table_plan),
        index_plan: Some(index_plan),
        extra_handle_col: t.extra_handle_col.clone(),
        common_handle_cols: t.common_handle_cols.clone(),
        expected_cnt: t.expect_cnt,
        keep_order: t.keep_order,
        plan_part_info: t.phys_plan_part_info.clone(),
        ..Default::default()
    }
    .init(ctx, qb_offset, t.index_look_up_push_down_by);
    // Do not inject the extra Projection even if t.needExtraProj is set, or the schema between the phase-1 agg and
    // the final agg would be broken. Please reference comments for the similar logic in
    // (*copTask).convertToRootTaskImpl() for the PhysicalTableReader case.
    // (`PhysicalHashAgg`, `PhysicalStreamAgg`: not ported yet.)
    let agg_pushed_down = false;

    if t.need_extra_proj && !agg_pushed_down {
        let schema = t
            .origin_schema
            .as_ref()
            .expect("runtime error: invalid memory address or nil pointer dereference");
        let p_stats = p.stats_info().map(Arc::clone);
        let mut proj = PhysicalProjection {
            exprs: tidb_expression::column2_exprs(&schema.columns),
            ..Default::default()
        }
        .init(ctx, p_stats, qb_offset, Vec::new());
        proj.set_schema((**schema).clone_schema());
        proj.set_children(vec![Arc::new(p)]);
        new_task.set_plan(Arc::new(proj));
    } else {
        new_task.set_plan(Arc::new(p));
    }
    new_task
}

/// setTableScanToTableRowIDScan is to update the isChildOfIndexLookUp attribute of PhysicalTableScan child.
// go: setTableScanToTableRowIDScan L1 [M-PLAN]
pub(crate) fn set_table_scan_to_table_row_id_scan(p: &mut PhysicalPlanRef) {
    if p.kind() == PhysicalKind::TableScan {
        let ts = make_mut_plan(p)
            .as_any_mut()
            .downcast_mut::<PhysicalTableScan>()
            .expect("PhysicalTableScan");
        ts.set_is_child_of_index_look_up(true);
    } else {
        crate::map_children(p, |mut child| {
            set_table_scan_to_table_row_id_scan(&mut child);
            child
        });
    }
}

/// `sinkIntoIndexLookUp`.
///
/// Mutates `reader` (clearing `table_plans` before writing `table_plan`'s `TableScan` stats and
/// rebuilding `table_plans` afterwards) BEFORE optionally wrapping `root` in `extraProj` so `Arc`
/// refcounts stay unique (N1 §3.4, D10.2 spec §11.3 item 4).
// go: PhysicalLimit.sinkIntoIndexLookUp L1 [M-CTX, M-PLAN]
fn sink_into_index_look_up(ctx: &PlanCtx<'_>, limit: &PhysicalLimit, t: &mut Task) -> bool {
    let Task::Root(root) = t else {
        panic!("interface conversion: base.Task is not *core.RootTask");
    };
    let is_double_read;
    let is_proj;
    let need_extra_proj;
    {
        let plan = deref(root.get_plan());
        let reader_ref: &crate::core::physical_plans::PhysicalIndexLookUpReader = if let Some(r) =
            plan.as_any()
                .downcast_ref::<crate::core::physical_plans::PhysicalIndexLookUpReader>()
        {
            is_double_read = true;
            is_proj = false;
            r
        } else if plan.kind() == PhysicalKind::Projection {
            is_proj = true;
            let Some(r) = plan.children().first().and_then(|c| {
                c.as_any()
                    .downcast_ref::<crate::core::physical_plans::PhysicalIndexLookUpReader>()
            }) else {
                return false;
            };
            is_double_read = true;
            r
        } else {
            return false;
        };
        let _ = is_double_read;
        // We can sink Limit into IndexLookUpReader only if tablePlan contains no Selection.
        let Some(tp) = reader_ref.table_plan.as_ref() else {
            return false;
        };
        if tp.kind() != PhysicalKind::TableScan {
            return false;
        }
        need_extra_proj = limit.schema().len() != reader_ref.schema().len();
    }

    let limit_stats = limit.stats_info().map(Arc::clone);
    let pushed_limit = crate::core::physical_plans::PushedDownLimit {
        offset: limit.offset,
        count: limit.count,
    };

    let apply_to_reader = |reader_plan: &mut PhysicalPlanRef| {
        let reader = make_mut_plan(reader_plan)
            .as_any_mut()
            .downcast_mut::<crate::core::physical_plans::PhysicalIndexLookUpReader>()
            .expect("PhysicalIndexLookUpReader");
        reader.pushed_limit = Some(pushed_limit);
        // Clear table_plans BEFORE mutating table_plan to avoid CoW splitting TableRowIDScan.
        reader.table_plans.clear();
        let tp = reader.table_plan.as_mut().expect("tablePlan");
        let ts = make_mut_plan(tp)
            .as_any_mut()
            .downcast_mut::<PhysicalTableScan>()
            .expect("PhysicalTableScan");
        let origin_stats = ts.stats_info().map(Arc::clone);
        let origin_rc = origin_stats
            .as_ref()
            .expect("runtime error: invalid memory address or nil pointer dereference")
            .row_count;
        let limit_rc = limit_stats
            .as_ref()
            .expect("runtime error: invalid memory address or nil pointer dereference")
            .row_count;
        if origin_rc >= limit_rc {
            // Only reset the table scan stats when its row estimation is larger than the limit count.
            // When indexLookUp push down is enabled, some rows have been looked up in TiKV side,
            // and the rows processed by the TiDB table scan may be less than the limit count.
            let mut new_stats = (**limit_stats.as_ref().expect("stats")).clone();
            if let Some(orig) = origin_stats {
                // keep the original stats version
                new_stats.stats_version = orig.stats_version;
            }
            ts.set_stats(Some(Arc::new(new_stats)));
        }
        reader.table_plans = crate::core::initialize::flatten_list_push_down_plan(tp);
        reader.set_stats(limit_stats.clone());
    };

    let root_plan = root.p.as_mut().expect("root plan");
    if is_proj {
        let proj = make_mut_plan(root_plan);
        apply_to_reader(&mut proj.children_mut()[0]);
        proj.set_stats(limit_stats.clone());
    } else {
        apply_to_reader(root_plan);
    }

    if need_extra_proj {
        let limit_schema = limit.schema().clone_schema();
        let mut extra_proj = PhysicalProjection {
            exprs: tidb_expression::column2_exprs(&limit_schema.columns),
            ..Default::default()
        }
        .init(ctx, limit_stats, limit.query_block_offset(), Vec::new());
        extra_proj.set_schema(limit_schema);
        // If the root.p is already a Projection. We left the optimization for the later Projection Elimination.
        let prev = root.p.take().expect("root plan");
        extra_proj.set_children(vec![prev]);
        root.set_plan(Arc::new(extra_proj));
    }

    true
}

/// Attach2Task implements PhysicalPlan interface.
// go: PhysicalSort.Attach2Task L1 [M-PLAN]
fn attach2_task_sort(p: PhysicalPlanRef, tasks: &[Task]) -> Task {
    debug_assert!(p.as_any().is::<PhysicalSort>());
    let mut t = tasks[0].copy();
    t = attach_plan2_task(p, t);
    t
}

/// Attach2Task implements PhysicalPlan interface.
// go: NominalSort.Attach2Task L1 [M-PLAN]
fn attach2_task_nominal_sort(p: PhysicalPlanRef, tasks: &[Task]) -> Task {
    if downcast::<NominalSort>(p.as_ref()).only_column {
        return tasks[0].clone();
    }
    let mut t = tasks[0].copy();
    t = attach_plan2_task(p, t);
    t
}

impl PhysicalTopN {
    /// `getPushedDownTopN`: a copy of this TopN with `Count` = offset + count, on top of
    /// `child_plan`.
    // go: PhysicalTopN.getPushedDownTopN L1 [M-CTX]
    fn get_pushed_down_top_n(
        &self,
        ctx: &PlanCtx<'_>,
        child_plan: PhysicalPlanRef,
    ) -> PhysicalTopN {
        let mut new_by_items: Vec<ByItems> = Vec::with_capacity(self.by_items.len());
        for expr in &self.by_items {
            new_by_items.push(expr.clone_item());
        }
        let mut new_partition_by: Vec<SortItem> = Vec::with_capacity(self.get_partition_by().len());
        for expr in self.get_partition_by() {
            new_partition_by.push(expr.clone_item());
        }
        let new_count = self.offset.wrapping_add(self.count);
        let child_profile = stats_of(child_plan.as_ref());
        // Strictly speaking, for the row count of pushed down TopN, we should multiply newCount with "regionNum",
        // but "regionNum" is unknown since the copTask can be a double read, so we ignore it now.
        let stats = derive_limit_stats(child_profile, new_count as f64);
        let mut top_n = PhysicalTopN {
            by_items: new_by_items,
            partition_by: new_partition_by,
            count: new_count,
            ..Default::default()
        }
        .init(
            ctx,
            Some(Arc::new(stats)),
            self.query_block_offset(),
            vec![self.get_child_req_props(0).map(Arc::clone)],
        );
        top_n.set_children(vec![child_plan]);
        top_n
    }

    /// canPushToIndexPlan checks if this TopN can be pushed to the index side of copTask.
    /// It can be pushed to the index side when all columns used by ByItems are available from the index side and there's no prefix index column.
    // go: PhysicalTopN.canPushToIndexPlan L1
    fn can_push_to_index_plan(
        index_plan: Option<&PhysicalPlanRef>,
        by_item_cols: &[Arc<Column>],
    ) -> bool {
        // If we call canPushToIndexPlan and there's no index plan, we should go into the index merge case.
        // Index merge case is specially handled for now. So we directly return false here.
        // So we directly return false.
        let Some(index_plan) = index_plan else {
            return false;
        };
        let schema = index_plan.schema();
        for col in by_item_cols {
            let pos = schema.column_index(col);
            if pos == -1 {
                return false;
            }
            if schema.columns[pos as usize].is_prefix {
                return false;
            }
        }
        true
    }

    /// canExpressionConvertedToPB checks whether each of the the expression in TopN can be converted to pb.
    // go: PhysicalTopN.canExpressionConvertedToPB L1 [M-CTX]
    fn can_expression_converted_to_pb(
        &self,
        ctx: &PlanCtx<'_>,
        store_tp: StoreType,
    ) -> Result<bool, Error> {
        let mut exprs = Vec::with_capacity(self.by_items.len());
        for item in &self.by_items {
            exprs.push(Arc::clone(&item.expr));
        }
        Ok(can_exprs_push_down(&ctx.push_down_ctx()?, &exprs, store_tp))
    }

    /// containVirtualColumn checks whether TopN.ByItems contains virtual generated columns.
    // go: PhysicalTopN.containVirtualColumn L1
    fn contain_virtual_column(&self, t_cols: &[Arc<Column>]) -> bool {
        let mut t_col_set = std::collections::HashSet::with_capacity(t_cols.len());
        for t_col in t_cols {
            if t_col.id > 0 && t_col.virtual_expr.is_some() {
                t_col_set.insert(t_col.id);
            }
        }
        for by in &self.by_items {
            let cols = extract_columns(&by.expr);
            for col in cols {
                if t_col_set.contains(&col.id) {
                    // A column with ID > 0 indicates that the column can be resolved by data source.
                    return true;
                }
            }
        }
        false
    }

    /// canPushDownToTiKV checks whether this topN can be pushed down to TiKV.
    // go: PhysicalTopN.canPushDownToTiKV L1 [M-CTX]
    fn can_push_down_to_ti_kv(&self, ctx: &PlanCtx<'_>, cop_task: &CopTask) -> Result<bool, Error> {
        if !self.can_expression_converted_to_pb(ctx, kv::TiKV)? {
            return Ok(false);
        }
        if !cop_task.root_task_conds.is_empty() {
            return Ok(false);
        }
        if !cop_task.index_plan_finished && cop_task.idx_merge_part_plans_len() > 0 {
            for partial_plan in cop_task.idx_merge_part_plans.iter().flatten() {
                if self.contain_virtual_column(&partial_plan.schema().columns) {
                    return Ok(false);
                }
            }
        } else if self.contain_virtual_column(&deref(cop_task.plan()).schema().columns) {
            return Ok(false);
        }
        Ok(true)
    }

    /// canPushDownToTiFlash checks whether this topN can be pushed down to TiFlash.
    // go: PhysicalTopN.canPushDownToTiFlash L1 [M-CTX]
    fn can_push_down_to_ti_flash(
        &self,
        ctx: &PlanCtx<'_>,
        mpp_task: &MppTask,
    ) -> Result<bool, Error> {
        if !self.can_expression_converted_to_pb(ctx, kv::TiFlash)? {
            return Ok(false);
        }
        if self.contain_virtual_column(&deref(mpp_task.plan()).schema().columns) {
            return Ok(false);
        }
        Ok(true)
    }
}

/// The leaf of a cop task's table plan for writing, and whether its parent is a Selection
/// (Go's `selOnTblScan` after the walk down `Children()[0]`).
// glue: the leaf walk of PhysicalTopN.pushLimitDownToTiDBCop
fn leaf_of(p: &PhysicalPlanRef) -> (&PhysicalPlanRef, Option<&PhysicalPlanRef>) {
    let mut sel_on_tbl_scan = None;
    let mut final_tbl_scan_plan = p;
    while !final_tbl_scan_plan.children().is_empty() {
        sel_on_tbl_scan =
            (final_tbl_scan_plan.kind() == PhysicalKind::Selection).then_some(final_tbl_scan_plan);
        final_tbl_scan_plan = &final_tbl_scan_plan.children()[0];
    }
    (final_tbl_scan_plan, sel_on_tbl_scan)
}

/// `f` on the leaf of `p` for writing (copy-on-write along the path, `M-PLAN`).
// glue: the in-place writes of PhysicalTopN.pushLimitDownToTiDBCop
fn with_leaf_mut(p: &mut PhysicalPlanRef, f: &mut dyn FnMut(&mut PhysicalPlanRef)) {
    if p.children().is_empty() {
        f(p);
        return;
    }
    crate::map_children(p, |mut child| {
        with_leaf_mut(&mut child, f);
        child
    });
}

impl PhysicalTopN {
    /// For upstream issue `51723`,
    /// This function only supports `CLUSTER_SLOW_QUERY`,
    /// it will change plan from
    /// TopN -> TableReader -> TableFullScan[cop] to
    /// TopN -> TableReader -> Limit[cop] -> TableFullScan[cop] + keepOrder
    ///
    /// Go's `(nil, false)` is `None`. Go replaces `copTsk.tablePlan` by its clone before
    /// any check, so the caller's task holds the clone even when nothing is pushed: `cop_tsk`
    /// is changed in place likewise.
    // go: PhysicalTopN.pushLimitDownToTiDBCop L1 [M-CTX, M-PLAN]
    fn push_limit_down_to_ti_db_cop(
        &self,
        ctx: &PlanCtx<'_>,
        this: &PhysicalPlanRef,
        cop_tsk: &mut CopTask,
    ) -> Result<Option<Task>, Error> {
        if cop_tsk.index_plan.is_some() || cop_tsk.table_plan.is_none() {
            return Ok(None);
        }

        let mut sel_selectivity = 0.0;

        cop_tsk.table_plan = match deref(cop_tsk.table_plan.as_ref()).clone_plan() {
            Ok(p) => Some(p),
            Err(_) => return Ok(None),
        };
        let table_plan = deref(cop_tsk.table_plan.as_ref());
        let (final_tbl_scan_plan, sel_on_tbl_scan) = leaf_of(table_plan);

        if final_tbl_scan_plan.kind() != PhysicalKind::TableScan {
            return Ok(None);
        }
        let tbl_scan = downcast::<PhysicalTableScan>(final_tbl_scan_plan.as_ref());

        // Check the table is `CLUSTER_SLOW_QUERY` or not.
        if tbl_scan.table.name.o != CLUSTER_TABLE_SLOW_LOG {
            return Ok(None);
        }

        let Some(cols_prop) = get_prop_by_order_by_items(&self.by_items) else {
            return Ok(None);
        };
        let handle_col = deref(deref(tbl_scan.handle_cols.as_ref()).get_col(0));
        if cols_prop.sort_items.len() != 1
            || !cols_prop.sort_items[0]
                .col
                .equal(&Expr::Column((**handle_col).clone()))
        {
            return Ok(None);
        }
        let tbl_scan_rows = stats_of(tbl_scan).row_count;
        if let Some(sel) = sel_on_tbl_scan
            && tbl_scan_rows > 0.0
        {
            sel_selectivity = stats_of(sel.as_ref()).row_count / tbl_scan_rows;
        }
        let desc = cols_prop.sort_items[0].desc;
        with_leaf_mut(deref(cop_tsk.table_plan.as_mut()), &mut |leaf| {
            let ts = downcast_mut::<PhysicalTableScan>(leaf);
            ts.desc = desc;
            ts.keep_order = true;
        });

        let child_profile = stats_of(deref(cop_tsk.plan()).as_ref());
        let new_count = self.offset.wrapping_add(self.count);
        let stats = derive_limit_stats(child_profile, new_count as f64);
        let mut pushed_limit = PhysicalLimit {
            count: new_count,
            ..Default::default()
        }
        .init(
            ctx,
            Some(Arc::new(stats)),
            self.query_block_offset(),
            Vec::new(),
        );
        pushed_limit.set_schema(deref(cop_tsk.table_plan.as_ref()).schema().clone_schema());
        // Go's copTsk is the caller's task: it holds the pushed limit from here on.
        let Task::Cop(mut c) =
            attach_plan2_task(Arc::new(pushed_limit), Task::Cop(std::mem::take(cop_tsk)))
        else {
            unreachable!("attachPlan2Task returns the task it is given")
        };
        {
            // child := pushedLimit.Children()[0]
            let limit = make_mut_plan(deref(c.table_plan.as_mut()));
            let child = &mut limit.children_mut()[0];
            let scaled = stats_of(child.as_ref()).scale_by_expect_cnt(new_count as f64);
            make_mut_plan(child).set_stats(Some(scaled));
        }
        if sel_selectivity > 0.0 && sel_selectivity < 1.0 {
            let limit_child = Arc::clone(&deref(c.table_plan.as_ref()).children()[0]);
            let scaled_row_count = stats_of(limit_child.as_ref()).row_count / sel_selectivity;
            with_leaf_mut(deref(c.table_plan.as_mut()), &mut |leaf| {
                let scaled = stats_of(leaf.as_ref()).scale_by_expect_cnt(scaled_row_count);
                make_mut_plan(leaf).set_stats(Some(scaled));
            });
        }
        let root_task = c.convert_to_root_task(ctx)?;
        Ok(Some(attach_plan2_task(Arc::clone(this), root_task)))
    }
}

/// Attach2Task implements the PhysicalPlan interface.
///
/// The index merge reader with advisory sort items (`handleAdvisorySortItemsForIndexMerge`)
/// is outside the first slice: a boundary stub on that branch.
// go: PhysicalTopN.Attach2Task L2 (the index merge branch is blocked-by:D10.1)
fn attach2_task_top_n(
    ctx: &PlanCtx<'_>,
    p: PhysicalPlanRef,
    tasks: &[Task],
) -> Result<Task, Error> {
    let mut t = tasks[0].copy();
    if let Task::Cop(cop_task) = &t
        && cop_task
            .partial_order_match_result
            .as_ref()
            .is_some_and(|m| m.matched)
    {
        let cop_task = cop_task.clone();
        return handle_partial_order_top_n(ctx, p, cop_task);
    }
    let top_n = downcast::<PhysicalTopN>(p.as_ref());
    let mut cols = Vec::with_capacity(top_n.by_items.len());
    for item in &top_n.by_items {
        cols.extend(extract_columns(&item.expr));
    }
    let need_push_down = !cols.is_empty();
    if let Task::Cop(cop_task) = &mut t
        && need_push_down
        && cop_task.get_store_type() == kv::TiDB
        && cop_task.root_task_conds.is_empty()
    {
        let new_task = top_n.push_limit_down_to_ti_db_cop(ctx, &p, cop_task)?;
        if let Some(new_task) = new_task {
            return Ok(new_task);
        }
    }
    let push_to_ti_kv = match &t {
        Task::Cop(cop_task) if need_push_down => {
            top_n.can_push_down_to_ti_kv(ctx, cop_task)? && cop_task.root_task_conds.is_empty()
        }
        _ => false,
    };
    if push_to_ti_kv {
        let Task::Cop(cop_task) = &mut t else {
            unreachable!("checked above")
        };
        // Handle IndexMerge with advisory sort items when some (but not all)
        // partial paths satisfy the sort order. When all paths satisfy, the
        // existing Limit pushdown via attach2Task4PhysicalLimit gives a better plan.
        if cop_task.idx_merge_part_plans_len() > 0
            && !cop_task.index_plan_finished
            && !cop_task.idx_merge_is_intersection
            && cop_task.idx_merge_match_with_advisory_sort_items
        {
            let all_satisfy = cop_task
                .idx_merge_part_plans_match_results
                .iter()
                .all(|r| r.matched());
            if !all_satisfy {
                // glue: blocked-by:D10.1 (handleAdvisorySortItemsForIndexMerge; index merge is outside the first slice)
                return Err(blocked(
                    "D10.1",
                    "PhysicalTopN.Attach2Task on an index merge with advisory sort items",
                ));
            }
        }
        // If all columns in topN are from index plan, we push it to index plan, otherwise we finish the index plan and
        // push it to table plan.
        if !cop_task.index_plan_finished
            && PhysicalTopN::can_push_to_index_plan(cop_task.index_plan.as_ref(), &cols)
        {
            let index_plan = deref(cop_task.index_plan.take());
            let pushed_down_top_n = top_n.get_pushed_down_top_n(ctx, index_plan);
            cop_task.index_plan = Some(Arc::new(pushed_down_top_n));
        } else {
            // It works for both normal index scan and index merge scan.
            cop_task.finish_index_plan()?;
            let table_plan = deref(cop_task.table_plan.take());
            let pushed_down_top_n = top_n.get_pushed_down_top_n(ctx, table_plan);
            cop_task.table_plan = Some(Arc::new(pushed_down_top_n));
        }
    } else if let Task::Mpp(mpp_task) = &mut t
        && need_push_down
        && top_n.can_push_down_to_ti_flash(ctx, mpp_task)?
    {
        let child = deref(mpp_task.p.take());
        let pushed_down_top_n = top_n.get_pushed_down_top_n(ctx, child);
        mpp_task.p = Some(Arc::new(pushed_down_top_n));
    }
    let root_task = t.convert_to_root_task(ctx)?;
    // Skip TopN with partition on the root. This is a derived topN and window function
    // will take care of the filter.
    if !top_n.get_partition_by().is_empty() {
        return Ok(t);
    }
    Ok(attach_plan2_task(p, root_task))
}

/// handlePartialOrderTopN handles the partial order TopN scenario.
///
/// Case 1: two-phase TopN, where TiDB keeps TopN and TiKV applies a partial-order Limit:
///
/// ```text
/// TopN(with partial info)
///   └─IndexLookUp
///      └─Limit(with partial info)
/// ```
///
/// Case 2: one-phase TopN, where the whole TopN can be executed in the coprocessor:
///
/// ```text
/// TopN(with partial info)
///   ├─IndexPlan
///   └─TablePlan
/// ```
// go: handlePartialOrderTopN L1 [M-CTX, M-PLAN]
fn handle_partial_order_top_n(
    ctx: &PlanCtx<'_>,
    mut p: PhysicalPlanRef,
    mut cop_task: CopTask,
) -> Result<Task, Error> {
    let match_result = Arc::clone(deref(cop_task.partial_order_match_result.as_ref()));
    let (partial_ordered_limit, qb_offset) = {
        let top_n = downcast_mut::<PhysicalTopN>(&mut p);
        let partial_ordered_limit = top_n.count.wrapping_add(top_n.offset);
        top_n.prefix_col = match_result.prefix_col.as_ref().map(Arc::clone);
        top_n.prefix_len = match_result.prefix_len;
        (partial_ordered_limit, top_n.query_block_offset())
    };

    let can_push_limit = cop_task.idx_merge_part_plans_len() == 0
        && !cop_task.index_plan_finished
        && cop_task.root_task_conds.is_empty()
        && cop_task.index_plan.is_some();
    if can_push_limit {
        let max_x = estimate_max_x_for_partial_order(ctx, &cop_task);
        let estimated_rows = partial_ordered_limit as f64 + max_x as f64;
        let index_plan = deref(cop_task.index_plan.take());
        let child_profile = stats_of(index_plan.as_ref());
        let limit_stats = derive_limit_stats(child_profile, estimated_rows);

        let mut pushed_down_limit = PhysicalLimit {
            count: partial_ordered_limit,
            prefix_col: match_result.prefix_col.as_ref().map(Arc::clone),
            prefix_len: match_result.prefix_len,
            ..Default::default()
        }
        .init(ctx, Some(Arc::new(limit_stats)), qb_offset, Vec::new());
        pushed_down_limit.set_schema(index_plan.schema().clone_schema());
        pushed_down_limit.set_children(vec![index_plan]);
        cop_task.index_plan = Some(Arc::new(pushed_down_limit));
    }
    let root_task = cop_task.convert_to_root_task(ctx)?;
    Ok(attach_plan2_task(p, root_task))
}

/// estimateMaxXForPartialOrder estimates the extra rows X to read for partial order optimization.
// go: estimateMaxXForPartialOrder L1 [M-CTX]
fn estimate_max_x_for_partial_order(_ctx: &PlanCtx<'_>, _cop_task: &CopTask) -> u64 {
    0
}

/// Attach2Task implements PhysicalPlan interface.
// go: PhysicalProjection.Attach2Task L1 [M-CTX, M-PLAN]
fn attach2_task_projection(
    ctx: &PlanCtx<'_>,
    mut p: PhysicalPlanRef,
    tasks: &[Task],
) -> Result<Task, Error> {
    let mut t = tasks[0].copy();
    let pushed = {
        let exprs = &downcast::<PhysicalProjection>(p.as_ref()).exprs;
        match &t {
            Task::Cop(cop) => {
                (cop.root_task_conds.is_empty() && cop.idx_merge_part_plans_len() == 0)
                    && can_exprs_push_down(&ctx.push_down_ctx()?, exprs, cop.get_store_type())
            }
            Task::Mpp(_) => can_exprs_push_down(&ctx.push_down_ctx()?, exprs, kv::TiFlash),
            Task::Root(_) => false,
        }
    };
    if pushed {
        match t {
            Task::Cop(_) => {
                let cop_task = attach_plan2_task(p, t);
                return Ok(cop_task);
            }
            Task::Mpp(mut mpp) => {
                let child = deref(mpp.p.take());
                make_mut_plan(&mut p).set_children(vec![child]);
                mpp.p = Some(p);
                return Ok(Task::Mpp(mpp));
            }
            Task::Root(_) => unreachable!("a root task is not pushed"),
        }
    }
    t = t.convert_to_root_task(ctx)?;
    t = attach_plan2_task(p, t);
    if let Task::Root(root) = &tasks[0]
        && root.is_empty()
    {
        let Task::Root(t) = &mut t else {
            panic!("interface conversion: base.Task is not *core.RootTask");
        };
        t.set_empty(true);
    }
    Ok(t)
}

/// Attach2Task implements PhysicalPlan interface.
// go: PhysicalSelection.Attach2Task L1 [M-CTX, M-PLAN]
fn attach2_task_selection(
    ctx: &PlanCtx<'_>,
    p: PhysicalPlanRef,
    tasks: &[Task],
) -> Result<Task, Error> {
    if let Task::Mpp(mpp_task) = &tasks[0] {
        // always push to mpp task.
        let sel = downcast::<PhysicalSelection>(p.as_ref());
        if can_exprs_push_down(&ctx.push_down_ctx()?, &sel.conditions, kv::TiFlash) {
            return Ok(attach_plan2_task(p, Task::Mpp(mpp_task.copy())));
        }
    }
    let t = tasks[0].convert_to_root_task(ctx)?;
    Ok(attach_plan2_task(p, t))
}

/// `computePartialCursorOffset` reads `aggregation.NeedCount` / `NeedValue` (block D08.11,
/// the aggregate functions); only the aggregation push down (`RemoveUnnecessaryFirstRow`),
/// outside the first slice, calls it.
// glue: stub of computePartialCursorOffset, blocked-by:D08.11 (aggregation.NeedCount / NeedValue; only the aggregation push down calls it)
pub fn compute_partial_cursor_offset(name: &str) -> Result<isize, Error> {
    let _ = name;
    Err(blocked(
        "D08.11",
        "computePartialCursorOffset (aggregation.NeedCount / NeedValue)",
    ))
}

/// `tryExpandVirtualColumn`: expands the virtual columns of the table scans under `p` (the
/// MPP gather in disaggregated TiFlash).
///
/// Go writes the scans in place, which every task sharing them sees; here the scans on the
/// path are copied when shared (`make_mut_plan`). Only `MppTask.ConvertToRootTaskImpl`, a
/// boundary stub, calls it.
// go: tryExpandVirtualColumn L1 [M-PLAN]
pub fn try_expand_virtual_column(p: &mut PhysicalPlanRef) {
    if p.kind() == PhysicalKind::TableScan {
        let ts = downcast_mut::<PhysicalTableScan>(p);
        let table = Arc::clone(&ts.table);
        let mut schema = ts.schema().clone_schema();
        ts.columns = expand_virtual_column(
            &ts.columns,
            &mut schema,
            table.columns.as_deref().unwrap_or(&[]),
        );
        ts.set_schema(schema);
        return;
    }
    crate::map_children(p, |mut child| {
        try_expand_virtual_column(&mut child);
        child
    });
}
