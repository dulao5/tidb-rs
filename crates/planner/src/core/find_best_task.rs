//! go-file: pkg/planner/core/find_best_task.go
//!
//! FindBestTask (block D10.2, specs/D10.2-find-best-task): the physical plan enumeration of
//! the logical plan with its memo (`findBestTask`, `enumeratePhysicalPlans4Task`,
//! `compareTaskCost`), the `TableDual` and `DataSource` tasks, the skyline pruning of the
//! access paths and the table paths (`convertToTableScan`, `convertToPointGet`).
//!
//! Boundaries (spec §5): an index path left after the skyline pruning (`convertToIndexScan`:
//! IndexReader / IndexLookUp, D10.2 part 2) and the batch point get (D10.8); index merge is
//! D10.7; TiFlash and MPP tasks are D10.1; the other
//! logical operators' tasks (memory tables, CTEs, SHOW) come with those operators.
//!
//! Mapping (spec §4):
//! - the memo (`taskMap`) is [`TaskMap`], a side table that only lives inside
//!   `physicalOptimize` (N1 §3 item 9.3);
//! - the candidates of `exhaustPhysicalPlans` are moved into `Attach2Task` one by one
//!   (N1 §3 item 9.2): no `Arc` of a candidate is cloned;
//! - a `candidatePath` owns a copy of its access path (Go points into the DataSource's
//!   path); `matchProperty`'s writes to the path are written back to the DataSource's path.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering::SeqCst;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_base::gostd::conv::f64_to_u64;
use tidb_base::gostd::time::utc;
use tidb_base::kv;
use tidb_base::mysql::{TypeLonglong, has_binary_flag, has_pri_key_flag, has_unsigned_flag};
use tidb_expr_pushdown::infer_pushdown::push_down_exprs;
use tidb_expression::{Column, Expr, ExprRef, new_schema};
use tidb_kv::key::{Handle, IntHandle};
use tidb_model::{ExtraHandleID, ExtraHandleName, ExtraPhysTblID, ExtraPhysTblIDName, StatePublic};
use tidb_parser::types::UnspecifiedLength;
use tidb_sessionctx::planbase::PlanCounterTp;
use tidb_sessionctx::ranger::Range;
use tidb_statistics::Table;
use tidb_types::datum::Datum;
use tidb_types::field_type::new_field_type;

use crate::arena::PlanId;
use crate::base::{self, LogicalPlan, PhysicalPlan};
use crate::core::cost::{SelectionFactor, ToleranceFactor};
use crate::core::physical_plans::{
    PhysPlanPartInfo, PhysicalIndexScan, PhysicalSelection, PhysicalTableDual, PhysicalTableScan,
};
use crate::core::plan::enforce_property;
use crate::core::point_get_plan::PointGetPlan;
use crate::core::task::attach2_task;
use crate::core::task_base::{CopTask, RootTask, Task};
use crate::logicalop::base_logical_plan::{
    TaskMap, get_logical_ts4_task_map, get_task, roll_back_task_map, store_task,
};
use crate::logicalop::{DataSource, LogicalTableDual};
use crate::physicalop::PhysicalPlanRef;
use crate::planctx::PlanCtx;
use crate::property::{
    AnyType, CopMultiReadTaskType, CopSingleReadTaskType, MppTaskType, PartialOrderInfo,
    PartialOrderMatchResult, PhysicalPropMatchResult, PhysicalProperty, PropMatched,
    PropMatchedNeedMergeSort, PropNotMatched, RootTaskType, SortItem, StatsInfo, TaskType,
};
use crate::util::fixcontrol;
use crate::util::misc::clone_col_infos;
use crate::util::optimizetrace::{PhysicalOptimizeOp, new_default_plan_cost_option};
use crate::util::path::{Col2Len, compare_col2_len, extract_col2_len};
use crate::util::{AccessPath, ByItems};

/// GetPropByOrderByItems will check if this sort property can be pushed or not. In order to simplify the problem, we only
/// consider the case that all expression are columns.
///
/// Go's `(nil, false)` is `None`.
// go: GetPropByOrderByItems L1
pub fn get_prop_by_order_by_items(items: &[ByItems]) -> Option<PhysicalProperty> {
    let mut prop_items = Vec::with_capacity(items.len());
    for item in items {
        let Expr::Column(col) = &*item.expr else {
            return None;
        };
        prop_items.push(SortItem {
            col: Arc::new(col.clone()),
            desc: item.desc,
        });
    }
    Some(PhysicalProperty {
        sort_items: prop_items,
        ..Default::default()
    })
}

/// GetPropByOrderByItemsContainScalarFunc will check if this sort property can be pushed or not. In order to simplify the
/// problem, we only consider the case that all expression are columns or some special scalar functions.
///
/// Go's `(nil, false, false)` is `None`; otherwise the property and `onlyColumn`.
// go: GetPropByOrderByItemsContainScalarFunc L1
pub fn get_prop_by_order_by_items_contain_scalar_func(
    items: &[ByItems],
) -> Option<(PhysicalProperty, bool)> {
    let mut prop_items = Vec::with_capacity(items.len());
    let mut only_column = true;
    for item in items {
        match &*item.expr {
            Expr::Column(expr) => prop_items.push(SortItem {
                col: Arc::new(expr.clone()),
                desc: item.desc,
            }),
            Expr::ScalarFunction(expr) => {
                let (col, desc) = expr.get_single_column(item.desc);
                let col = col?;
                prop_items.push(SortItem {
                    col: Arc::new(col.clone()),
                    desc,
                });
                only_column = false;
            }
            _ => return None,
        }
    }
    Some((
        PhysicalProperty {
            sort_items: prop_items,
            ..Default::default()
        },
        only_column,
    ))
}

/// The `PlanCounterDisabled` of the children's searches (`base.PlanCounterTp(-1)`).
// glue: PlanCounterDisabled (core_init.go)
const PLAN_COUNTER_DISABLED: PlanCounterTp = PlanCounterTp(-1);

// go: findBestTask4LogicalTableDual L1 [M-CTX]
pub fn find_best_task4_logical_table_dual(
    p: &LogicalTableDual,
    ctx: &mut PlanCtx<'_>,
    prop: &PhysicalProperty,
    plan_counter: &mut PlanCounterTp,
    opt: &PhysicalOptimizeOp,
) -> Result<(Task, i64), Error> {
    // If the required property is not empty and the row count > 1,
    // we cannot ensure this required property.
    // But if the row count is 0 or 1, we don't need to care about the property.
    if (!prop.is_sort_item_empty() && p.row_count > 1) || plan_counter.empty() {
        return Ok((Task::invalid_task(), 0));
    }
    let mut dual = PhysicalTableDual {
        row_count: p.row_count,
        ..Default::default()
    }
    .init(
        ctx,
        p.base.plan.stats_info().cloned(),
        p.query_block_offset(),
    );
    dual.set_schema(p.schema(&ctx.arena).clone_schema());
    plan_counter.dec(1);
    let dual: PhysicalPlanRef = Arc::new(dual);
    append_candidate4_physical_optimize_op(opt, Some(&dual), prop);
    let mut rt = RootTask::default();
    rt.set_plan(dual);
    rt.set_empty(p.row_count == 0);
    Ok((Task::Root(rt), 1))
}

/// rebuildChildTasks rebuilds the childTasks to make the clock_th combination.
// go: rebuildChildTasks L1 [M-PLAN]
#[allow(clippy::too_many_arguments)]
fn rebuild_child_tasks(
    children: &[PlanId],
    ctx: &mut PlanCtx<'_>,
    memo: &mut TaskMap,
    child_tasks: &mut Vec<Task>,
    pp: &PhysicalPlanRef,
    child_cnts: &[i64],
    mut plan_counter: i64,
    ts: u64,
    opt: &PhysicalOptimizeOp,
) -> Result<(), Error> {
    // The taskMap of children nodes should be rolled back first.
    for &child in children {
        roll_back_task_map(memo, ctx, child, ts);
    }

    let mut mult_all: i64 = 1;
    for &x in child_cnts {
        mult_all = mult_all.wrapping_mul(x);
    }
    child_tasks.clear();
    for (j, &child) in children.iter().enumerate() {
        mult_all /= child_cnts[j];
        let mut cur_clock = PlanCounterTp((plan_counter - 1) / mult_all + 1);
        let child_prop = child_req_prop(pp, j)?;
        let r = base::find_best_task(ctx, child, memo, &child_prop, &mut cur_clock, opt);
        plan_counter = (plan_counter - 1) % mult_all + 1;
        let (child_task, _) = r?;
        if cur_clock.0 != 0 {
            return Err(Error::plain("PlanCounterTp planCounter is not handled"));
        }
        if child_task.invalid() {
            return Err(Error::plain(
                "The current plan is invalid, please skip this plan",
            ));
        }
        child_tasks.push(child_task);
    }
    Ok(())
}

/// `pp.GetChildReqProps(j)`. Go's nil property is the inner side of an index join, which
/// builds its inner plan itself (`findBestTask` returns a nil task for it): index joins are
/// not ported, so a nil property is a boundary.
// glue: GetChildReqProps of the candidates (a nil property is the index join's inner side)
fn child_req_prop(pp: &PhysicalPlanRef, j: usize) -> Result<Arc<PhysicalProperty>, Error> {
    match pp.get_child_req_props(j as isize) {
        Some(p) => Ok(Arc::clone(p)),
        // glue: blocked-by:D10.3 (a nil child property: the inner side of an index join)
        None => Err(blocked(
            "D10.3",
            "a nil child property (the inner side of an index join)",
        )),
    }
}

/// `enumeratePhysicalPlans4Task`. The candidates are moved into `Attach2Task` (N1 §3
/// item 9.2). `LogicalSequence` (CTEs, D09.5) is not ported: the iteration is always
/// `iteratePhysicalPlan4BaseLogical`.
// go: enumeratePhysicalPlans4Task L2 (iterateChildPlan4LogicalSequence comes with LogicalSequence, D09.5)
#[allow(clippy::too_many_arguments)]
fn enumerate_physical_plans4_task<P: LogicalPlan + ?Sized>(
    p: &P,
    ctx: &mut PlanCtx<'_>,
    memo: &mut TaskMap,
    physical_plans: Vec<PhysicalPlanRef>,
    prop: &PhysicalProperty,
    add_enforcer: bool,
    plan_counter: &mut PlanCounterTp,
    opt: &PhysicalOptimizeOp,
) -> Result<(Task, i64), Error> {
    let mut best_task = Task::invalid_task();
    let mut cnt_plan: i64 = 0;
    let children = p.children().to_vec();
    let mut child_cnts = vec![0i64; children.len()];

    for pp in physical_plans {
        let time_stamp_now = get_logical_ts4_task_map(ctx);
        let saved_plan_id = ctx.vars.state.plan_id.0.load(SeqCst);

        let (mut child_tasks, mut cur_cnt_plan) =
            iterate_physical_plan4_base_logical(&children, ctx, memo, &pp, &mut child_cnts, opt)?;

        // This check makes sure that there is no invalid child task.
        if child_tasks.len() != children.len() {
            continue;
        }

        // If the target plan can be found in this physicalPlan(pp), rebuild childTasks to build the corresponding combination.
        if plan_counter.is_force() && plan_counter.0 <= cur_cnt_plan {
            ctx.vars.state.plan_id.0.store(saved_plan_id, SeqCst);
            cur_cnt_plan = plan_counter.0;
            rebuild_child_tasks(
                &children,
                ctx,
                memo,
                &mut child_tasks,
                &pp,
                &child_cnts,
                plan_counter.0,
                time_stamp_now,
                opt,
            )?;
        }

        // Combine the best child tasks with parent physical plan.
        let mut cur_task = attach2_task(ctx, pp, &child_tasks)?;
        if cur_task.invalid() {
            continue;
        }

        // An optimal task could not satisfy the property, so it should be converted here.
        if !matches!(cur_task, Task::Root(_)) && prop.task_tp == RootTaskType {
            cur_task = cur_task.convert_to_root_task(ctx)?;
        }

        // Enforce curTask property
        if add_enforcer {
            cur_task = enforce_property(prop, cur_task, ctx)?;
        }

        // Optimize by shuffle executor to running in parallel manner.
        if !matches!(cur_task, Task::Mpp(_)) && prop.is_sort_item_empty() {
            // Currently, we do not regard shuffled plan as a new plan.
            cur_task = crate::core::plan::optimize_by_shuffle(cur_task, ctx);
        }

        cnt_plan += cur_cnt_plan;
        plan_counter.dec(cur_cnt_plan);

        if plan_counter.empty() {
            best_task = cur_task;
            break;
        }
        append_candidate4_physical_optimize_op(opt, cur_task.plan(), prop);
        // Get the most efficient one.
        if compare_task_cost(ctx, &cur_task, &best_task, opt)? {
            best_task = cur_task;
        }
    }
    Ok((best_task, cnt_plan))
}

/// iteratePhysicalPlan4BaseLogical is used to iterate the physical plan and get all child
/// tasks. Go's `(nil, 0, childCnts, nil)` for an invalid child is an empty task list.
// go: iteratePhysicalPlan4BaseLogical L1 [M-PLAN]
fn iterate_physical_plan4_base_logical(
    children: &[PlanId],
    ctx: &mut PlanCtx<'_>,
    memo: &mut TaskMap,
    self_physical_plan: &PhysicalPlanRef,
    child_cnts: &mut [i64],
    opt: &PhysicalOptimizeOp,
) -> Result<(Vec<Task>, i64), Error> {
    // Find best child tasks firstly.
    let mut child_tasks = Vec::with_capacity(children.len());
    // The curCntPlan records the number of possible plans for pp
    let mut cur_cnt_plan: i64 = 1;
    for (j, &child) in children.iter().enumerate() {
        let child_prop = child_req_prop(self_physical_plan, j)?;
        let mut counter = PLAN_COUNTER_DISABLED;
        let (child_task, cnt) =
            base::find_best_task(ctx, child, memo, &child_prop, &mut counter, opt)?;
        child_cnts[j] = cnt;
        cur_cnt_plan = cur_cnt_plan.wrapping_mul(cnt);
        if child_task.invalid() {
            return Ok((Vec::new(), 0));
        }
        child_tasks.push(child_task);
    }

    // This check makes sure that there is no invalid child task.
    if child_tasks.len() != children.len() {
        return Ok((Vec::new(), 0));
    }
    Ok((child_tasks, cur_cnt_plan))
}

/// compareTaskCost compares cost of curTask and bestTask and returns whether curTask's cost
/// is smaller than bestTask's.
// go: compareTaskCost L1 [M-CTX]
pub fn compare_task_cost(
    ctx: &PlanCtx<'_>,
    cur_task: &Task,
    best_task: &Task,
    op: &PhysicalOptimizeOp,
) -> Result<bool, Error> {
    let (cur_cost, cur_invalid) = get_task_plan_cost(ctx, cur_task, op)?;
    let (best_cost, best_invalid) = get_task_plan_cost(ctx, best_task, op)?;
    if cur_invalid {
        return Ok(false);
    }
    if best_invalid {
        return Ok(true);
    }
    Ok(cur_cost < best_cost)
}

/// getTaskPlanCost returns the cost of this task.
/// The new cost interface will be used if EnableNewCostInterface is true.
/// The second returned value indicates whether this task is valid.
///
/// Go's `WithOptimizeTracer(pop)`: the optimizer trace is a boundary before the physical
/// optimization (TRACE PLAN), so the option never carries a tracer.
// go: getTaskPlanCost L1 [M-CTX]
fn get_task_plan_cost(
    ctx: &PlanCtx<'_>,
    t: &Task,
    _pop: &PhysicalOptimizeOp,
) -> Result<(f64, bool), Error> {
    if t.invalid() {
        return Ok((f64::MAX, true));
    }

    // use the new cost interface
    let mut index_partial_cost = 0.0;
    let option = new_default_plan_cost_option();
    let cost = |p: &PhysicalPlanRef, tt: TaskType| {
        crate::core::plan_cost_ver2::get_plan_cost_impl(ctx, &**p, tt, &option)
    };
    let task_type: TaskType = match t {
        Task::Root(_) => RootTaskType,
        // no need to know whether the task is single-read or double-read, so both CopSingleReadTaskType and CopDoubleReadTaskType are OK
        Task::Cop(cop) => {
            if let (Some(index_plan), Some(table_plan)) = (&cop.index_plan, &cop.table_plan) {
                // handle IndexLookup specially
                let tt = CopMultiReadTaskType;
                // keep compatible with the old cost interface, for CopMultiReadTask, the cost is idxCost + tblCost.
                if !cop.index_plan_finished {
                    // only consider index cost in this case
                    let idx_cost = cost(index_plan, tt)?;
                    return Ok((idx_cost, false));
                }
                // consider both sides
                let idx_cost = cost(index_plan, tt)?;
                let tbl_cost = cost(table_plan, tt)?;
                return Ok((idx_cost + tbl_cost, false));
            }

            let mut tt = CopSingleReadTaskType;

            // TiFlash can run cop task as well, check whether this cop task will run on TiKV or TiFlash.
            if let Some(table_plan) = &cop.table_plan {
                let mut leaf_node = table_plan;
                while !leaf_node.children().is_empty() {
                    leaf_node = &leaf_node.children()[0];
                }
                if let Some(tbl_scan) = leaf_node.as_any().downcast_ref::<PhysicalTableScan>()
                    && tbl_scan.store_type == kv::TiFlash
                {
                    tt = MppTaskType;
                }
            }

            // Detail reason ref about comment in function `convertToIndexMergeScan`
            // for cop task with {indexPlan=nil, tablePlan=xxx, idxMergePartPlans=[x,x,x], indexPlanFinished=true} we should
            // plus the partial index plan cost into the final cost. Because t.plan() the below code used only calculate the
            // cost about table plan.
            if cop.index_plan_finished
                && let Some(parts) = &cop.idx_merge_part_plans
                && !parts.is_empty()
            {
                for partial_scan in parts {
                    index_partial_cost += cost(partial_scan, tt)?;
                }
            }
            tt
        }
        Task::Mpp(_) => MppTaskType,
    };
    let Some(plan) = t.plan() else {
        // It's a very special case for index merge case.
        // t.plan() == nil in index merge COP case, it means indexPlanFinished is false in other words.
        let mut c = 0.0;
        if let Task::Cop(cop) = t {
            for partial_scan in cop.idx_merge_part_plans.iter().flatten() {
                c += cost(partial_scan, task_type)?;
            }
        }
        return Ok((c, false));
    };
    let c = cost(plan, task_type)?;
    Ok((c + index_partial_cost, false))
}

/// `appendCandidate4PhysicalOptimizeOp`: records a candidate in the optimizer trace. The
/// trace is a boundary before the physical optimization (TRACE PLAN), so there is never a
/// tracer and Go returns at the first check.
// go: appendCandidate4PhysicalOptimizeOp L2 (the optimizer trace stops before physicalOptimize, D02.2)
fn append_candidate4_physical_optimize_op(
    pop: &PhysicalOptimizeOp,
    pp: Option<&PhysicalPlanRef>,
    prop: &PhysicalProperty,
) {
    let _ = prop;
    if pop.get_tracer().is_none() || pp.is_none() {
        return;
    }
    unreachable!("the optimizer trace stops before physicalOptimize (D02.2)");
}

/// `appendPlanCostDetail4PhysicalOptimizeOp`: the plan cost detail of the optimizer trace,
/// never set (see [`append_candidate4_physical_optimize_op`]).
// go: appendPlanCostDetail4PhysicalOptimizeOp L2 (the optimizer trace stops before physicalOptimize, D02.2)
pub fn append_plan_cost_detail4_physical_optimize_op(pop: &PhysicalOptimizeOp) {
    if pop.get_tracer().is_none() {
        return;
    }
    unreachable!("the optimizer trace stops before physicalOptimize (D02.2)");
}

/// findBestTask is key workflow that drive logic plan tree to generate optimal physical
/// ones. The logic inside it is mainly about physical plan numeration and task
/// encapsulation, it should be defined in core pkg, and be called by logic plan in their
/// logic interface implementation.
///
/// Go's nil `prop` (the inner side of an index join) is not representable: the caller
/// returns that boundary ([`child_req_prop`]). The candidates are moved into the
/// enumeration (N1 §3 item 9.2).
// go: findBestTask L1 [M-SELF, M-PLAN]
pub fn find_best_task<P: LogicalPlan + ?Sized>(
    p: &mut P,
    ctx: &mut PlanCtx<'_>,
    this: PlanId,
    memo: &mut TaskMap,
    prop: &PhysicalProperty,
    plan_counter: &mut PlanCounterTp,
    opt: &PhysicalOptimizeOp,
) -> Result<(Task, i64), Error> {
    // Look up the task with this prop in the task map.
    // It's used to reduce double counting.
    if let Some(best_task) = get_task(memo, this, prop) {
        plan_counter.dec(1);
        return Ok((best_task, 1));
    }

    let mut can_add_enforcer = prop.can_add_enforcer;

    if prop.task_tp != RootTaskType && !prop.is_flash_prop() {
        // Currently all plan cannot totally push down to TiKV.
        store_task(memo, ctx, this, prop, Task::invalid_task());
        return Ok((Task::invalid_task(), 0));
    }

    let mut cnt_plan: i64 = 0;
    // prop should be read only because its cached hashcode might be not consistent
    // when it is changed. So we clone a new one for the temporary changes.
    let mut new_prop = prop.clone_essential_fields();
    // Maybe the plan can satisfy the required property,
    // so we try to get the task without the enforced sort first.
    let (mut plans_fits_prop, hint_works_with_prop) =
        p.exhaust_physical_plans(ctx, this, &new_prop)?;
    let mut plans_need_enforce = Vec::new();
    if !hint_works_with_prop && !new_prop.is_sort_item_empty() {
        // If there is a hint in the plan and the hint cannot satisfy the property,
        // we enforce this property and try to generate the PhysicalPlan again to
        // make sure the hint can work.
        can_add_enforcer = true;
    }

    let enum_prop: &PhysicalProperty = if can_add_enforcer {
        // Then, we use the empty property to get physicalPlans and
        // try to get the task with an enforced sort.
        new_prop.sort_items = Vec::new();
        new_prop.sort_items_for_partition = Vec::new();
        new_prop.expected_cnt = f64::MAX;
        new_prop.mpp_partition_cols = Vec::new();
        new_prop.mpp_partition_tp = AnyType;
        let (need_enforce, hint_can_work) = p.exhaust_physical_plans(ctx, this, &new_prop)?;
        plans_need_enforce = need_enforce;
        if hint_can_work && !hint_works_with_prop {
            // If the hint can work with the empty property, but cannot work with
            // the required property, we give up `plansFitProp` to make sure the hint
            // can work.
            plans_fits_prop = Vec::new();
        }
        if !hint_can_work && !hint_works_with_prop && !prop.can_add_enforcer {
            // If the original property is not enforced and hint cannot
            // work anyway, we give up `plansNeedEnforce` for efficiency,
            plans_need_enforce = Vec::new();
        }
        prop
    } else {
        &new_prop
    };

    let (mut best_task, cnt) = enumerate_physical_plans4_task(
        p,
        ctx,
        memo,
        plans_fits_prop,
        enum_prop,
        false,
        plan_counter,
        opt,
    )?;
    cnt_plan += cnt;
    if !plan_counter.empty() {
        let (cur_task, cnt) = enumerate_physical_plans4_task(
            p,
            ctx,
            memo,
            plans_need_enforce,
            enum_prop,
            true,
            plan_counter,
            opt,
        )?;
        cnt_plan += cnt;
        if plan_counter.empty() {
            best_task = cur_task;
        } else {
            append_candidate4_physical_optimize_op(opt, cur_task.plan(), prop);
            if compare_task_cost(ctx, &cur_task, &best_task, opt)? {
                best_task = cur_task;
            }
        }
    }

    // END:
    store_task(memo, ctx, this, prop, best_task.clone());
    Ok((best_task, cnt_plan))
}

/// tryToGetDualTask will check if the push down predicate has false constant. If so, it
/// will return table dual.
// go: tryToGetDualTask L1 [M-CTX]
fn try_to_get_dual_task(ds: &DataSource, ctx: &PlanCtx<'_>) -> Result<Option<Task>, Error> {
    for cond in &ds.pushed_down_conds {
        if let Expr::Constant(con) = &**cond
            && con.deferred_expr.is_none()
            && con.param_marker.is_none()
        {
            let (result, _) = tidb_expression::expression::eval_bool(
                &*ctx.eval_ctx(),
                &tidb_expression::EvalState::new(),
                std::slice::from_ref(cond),
                tidb_chunk::Row::default(),
            )?;
            if !result {
                let mut dual = PhysicalTableDual::default().init(
                    ctx,
                    ds.base.plan.stats_info().cloned(),
                    ds.query_block_offset(),
                );
                dual.set_schema(ds.schema(&ctx.arena).clone_schema());
                let mut rt = RootTask::default();
                rt.set_plan(Arc::new(dual));
                return Ok(Some(Task::Root(rt)));
            }
        }
    }
    Ok(None)
}

/// candidatePath is used to maintain required info for skyline pruning.
///
/// Go points `path` into the DataSource's access path; here the candidate owns a copy
/// (module doc).
#[derive(Clone, Debug, Default)]
pub struct CandidatePath {
    /// path.
    pub path: AccessPath,
    /// accessCondsColMap maps Column.UniqueID to column length for the columns in AccessConds.
    pub access_conds_col_map: Col2Len,
    /// indexCondsColMap maps Column.UniqueID to column length for the columns in AccessConds and indexFilters.
    pub index_conds_col_map: Col2Len,
    /// matchPropResult.
    pub match_prop_result: PhysicalPropMatchResult,
    /// partialOrderMatchResult records whether this path can provide partial order using prefix index.
    pub partial_order_match_result: PartialOrderMatchResult,
    /// how many index columns are used in access conditions in this IndexJoin.
    pub index_join_cols: isize,
    /// cached result of whether this path covers the full scan range.
    pub is_full_range: bool,
    /// cached result of equalPredicateCount().
    pub eq_or_in_count: isize,
}

// go: compareBool L1
pub(crate) fn compare_bool(l: bool, r: bool) -> isize {
    if l == r {
        return 0;
    }
    if !l {
        return -1;
    }
    1
}

// go: compareIndexBack L1
fn compare_index_back(lhs: &CandidatePath, rhs: &CandidatePath) -> (isize, bool) {
    let result = compare_bool(lhs.path.is_single_scan, rhs.path.is_single_scan);
    if result == 0 && !lhs.path.is_single_scan {
        // if both lhs and rhs need to access table after IndexScan, we utilize the set of columns that occurred in AccessConds and IndexFilters
        // to compare how many table rows will be accessed.
        return compare_col2_len(&lhs.index_conds_col_map, &rhs.index_conds_col_map);
    }
    (result, true)
}

// go: compareGlobalIndex L1
fn compare_global_index(lhs: &CandidatePath, rhs: &CandidatePath) -> isize {
    if lhs.path.is_table_path()
        || rhs.path.is_table_path()
        || !lhs.path.partial_index_paths.is_empty()
        || !rhs.path.partial_index_paths.is_empty()
    {
        return 0;
    }
    compare_bool(
        lhs.path.index.as_ref().expect("index path").global,
        rhs.path.index.as_ref().expect("index path").global,
    )
}

/// `isMVIndexPath` (`indexmerge_path.go`): whether the path is a multi-valued index path.
// glue: isMVIndexPath of indexmerge_path.go (D10.7), the one check skyline pruning needs
fn is_mv_index_path(path: &AccessPath) -> bool {
    !path.is_table_path() && path.index.as_ref().is_some_and(|i| i.mv_index)
}

/// compareCandidates is the core of skyline pruning, which is used to decide which
/// candidate path is better. The return value is 1 if lhs is better, -1 if rhs is better,
/// 0 if they are equivalent or not comparable.
// go: compareCandidates L1 [M-CTX]
fn compare_candidates(
    sctx: &PlanCtx<'_>,
    stats_tbl: Option<&Table>,
    prop: &PhysicalProperty,
    lhs: &CandidatePath,
    rhs: &CandidatePath,
    prefer_range: bool,
) -> (isize, bool) {
    // Due to #50125, full scan on MVIndex has been disabled, so MVIndex path might lead to 'can't find a proper plan' error at the end.
    // Avoid MVIndex path to exclude all other paths and leading to 'can't find a proper plan' error, see #49438 for an example.
    if is_mv_index_path(&lhs.path) || is_mv_index_path(&rhs.path) {
        return (0, false);
    }
    // lhsPseudo == lhs has pseudo (no) stats for the table or index for the lhs path.
    // rhsPseudo == rhs has pseudo (no) stats for the table or index for the rhs path.
    //
    // For the return value - if lhs wins (1), we return lhsPseudo. If rhs wins (-1), we return rhsPseudo.
    // If there is no winner (0), we return false.
    //
    // This return value is used later in SkyLinePruning to determine whether we should preference an index scan
    // over a table scan. Allowing indexes without statistics to survive means they can win via heuristics where
    // they otherwise would have lost on cost.
    let (mut lhs_pseudo, mut rhs_pseudo, mut table_pseudo) = (false, false, false);
    if let Some(stats_tbl) = stats_tbl {
        table_pseudo = stats_tbl.hist_coll.pseudo;
        (lhs_pseudo, rhs_pseudo) = is_candidates_pseudo(lhs, rhs, stats_tbl);
    }
    // matchResult: comparison result of whether LHS vs RHS matches the required properties (1=LHS better, -1=RHS better, 0=equal)
    // globalResult: comparison result of global index vs local index preference (1=LHS better, -1=RHS better, 0=equal)
    let match_result = compare_bool(
        lhs.match_prop_result.matched(),
        rhs.match_prop_result.matched(),
    );
    let global_result = compare_global_index(lhs, rhs);
    // accessResult: comparison result of access condition coverage (1=LHS better, -1=RHS better, 0=equal)
    // comparable1: whether the access conditions are comparable between LHS and RHS
    let (access_result, comparable1) =
        compare_col2_len(&lhs.access_conds_col_map, &rhs.access_conds_col_map);
    // scanResult: comparison result of index back scan efficiency (1=LHS better, -1=RHS better, 0=equal)
    //             scanResult will always be true for a table scan (because it is a single scan).
    //             This has the effect of allowing the table scan plan to not be pruned.
    // comparable2: whether the index back scan characteristics are comparable between LHS and RHS
    let (scan_result, comparable2) = compare_index_back(lhs, rhs);
    // eqOrInResult: comparison result of equal/IN predicate coverage (1=LHS better, -1=RHS better, 0=equal)
    let eq_or_in_result = compare_eq_or_in(lhs, rhs);

    // totalSum is the aggregate score of all comparison metrics
    // riskResult is excluded because more work is required.
    // TODO: - extend riskResult such that risk factors can be integrated into the aggregate score. Risk should
    // consider what "type" of risk is being evaluated (eg. out of range, implied independence, data skew, whether a
    // bound was applied, etc.)
    let total_sum = access_result + scan_result + match_result + global_result + eq_or_in_result;

    // Determine winner if one index doesn't have statistics and another has statistics
    if (lhs_pseudo || rhs_pseudo) && !table_pseudo // At least one index doesn't have statistics
        && (lhs.eq_or_in_count > 0 || rhs.eq_or_in_count > 0)
    // At least one index has equal/IN predicates
    {
        let lhs_full_match = is_full_index_match(lhs);
        let rhs_full_match = is_full_index_match(rhs);
        let pseudo_result = compare_pseudo(
            lhs_pseudo,
            rhs_pseudo,
            lhs_full_match,
            rhs_full_match,
            eq_or_in_result,
            lhs.eq_or_in_count,
            rhs.eq_or_in_count,
            prefer_range,
        );
        if pseudo_result > 0 && total_sum >= 0 {
            return (pseudo_result, lhs_pseudo);
        }
        if pseudo_result < 0 && total_sum <= 0 {
            return (pseudo_result, rhs_pseudo);
        }
    }

    // This rule is empirical but not always correct.
    // If x's range row count is significantly lower than y's, for example, 1000 times, we think x is better.
    if lhs.path.count_after_access > 100.0 && rhs.path.count_after_access > 100.0 // to prevent some extreme cases, e.g. 0.01 : 10
        && lhs.path.partial_index_paths.is_empty() && rhs.path.partial_index_paths.is_empty() // not IndexMerge since its row count estimation is not accurate enough
        && prop.expected_cnt == f64::MAX
    // Limit may affect access row count
    {
        let threshold = fixcontrol::get_int_with_default(
            sctx.vars.get_optimizer_fix_control_map(),
            fixcontrol::Fix45132,
            1000,
        ) as f64;
        if threshold > 0.0 {
            // set it to 0 to disable this rule
            if lhs.path.count_after_access / rhs.path.count_after_access > threshold {
                return (-1, rhs_pseudo); // right wins - also return whether it has statistics (pseudo) or not
            }
            if rhs.path.count_after_access / lhs.path.count_after_access > threshold {
                return (1, lhs_pseudo); // left wins - also return whether it has statistics (pseudo) or not
            }
        }
    }

    if !comparable1 || !comparable2 {
        return (0, false); // No winner (0). Do not return the pseudo result
    }
    if access_result >= 0
        && scan_result >= 0
        && match_result >= 0
        && global_result >= 0
        && eq_or_in_result >= 0
        && total_sum > 0
    {
        return (1, lhs_pseudo); // left wins - also return whether it has statistics (pseudo) or not
    }
    if access_result <= 0
        && scan_result <= 0
        && match_result <= 0
        && global_result <= 0
        && eq_or_in_result <= 0
        && total_sum < 0
    {
        return (-1, rhs_pseudo); // right wins - also return whether it has statistics (pseudo) or not
    }
    (0, false) // No winner (0). Do not return the pseudo result
}

// go: isCandidatesPseudo L1
fn is_candidates_pseudo(
    lhs: &CandidatePath,
    rhs: &CandidatePath,
    stats_tbl: &Table,
) -> (bool, bool) {
    let (mut lhs_pseudo, mut rhs_pseudo) = (stats_tbl.hist_coll.pseudo, stats_tbl.hist_coll.pseudo);
    let analyzed = |id: i64| {
        stats_tbl
            .col_and_idx_existence_map
            .as_ref()
            .is_some_and(|m| m.has_analyzed(id, true))
    };
    if lhs.path.partial_index_paths.is_empty() && rhs.path.partial_index_paths.is_empty() {
        if !lhs.path.is_table_path()
            && let Some(index) = &lhs.path.index
        {
            // We have statistics for the lhs index, or not.
            lhs_pseudo = !analyzed(index.id);
        }
        if !rhs.path.is_table_path()
            && let Some(index) = &rhs.path.index
        {
            // We have statistics on the rhs index, or not.
            rhs_pseudo = !analyzed(index.id);
        }
    }
    (lhs_pseudo, rhs_pseudo)
}

// go: comparePseudo L1
#[allow(clippy::too_many_arguments)]
fn compare_pseudo(
    lhs_pseudo: bool,
    rhs_pseudo: bool,
    lhs_full_match: bool,
    rhs_full_match: bool,
    eq_or_in_result: isize,
    lhs_eq_or_in_count: isize,
    rhs_eq_or_in_count: isize,
    prefer_range: bool,
) -> isize {
    // TO-DO: Consider a separate set of rules for global indexes.
    // If one index has statistics and the other does not, choose the index with statistics if it
    // has the same or higher number of equal/IN predicates.
    if !lhs_pseudo && lhs_eq_or_in_count > 0 && eq_or_in_result >= 0 {
        return 1; // left wins
    }
    if !rhs_pseudo && rhs_eq_or_in_count > 0 && eq_or_in_result <= 0 {
        return -1; // right wins
    }
    if prefer_range {
        // keep an index without statistics if that index has more equal/IN predicates, AND:
        // 1) there are at least 2 equal/INs
        // 2) OR - it's a full index match for all index predicates
        if lhs_pseudo && eq_or_in_result > 0 && (lhs_eq_or_in_count > 1 || lhs_full_match) {
            return 1; // left wins
        }
        if rhs_pseudo && eq_or_in_result < 0 && (rhs_eq_or_in_count > 1 || rhs_full_match) {
            return -1; // right wins
        }
    }
    0
}

/// Return the index with the higher EqOrInCondCount as winner (1 for lhs, -1 for rhs, 0 for tie).
// go: compareEqOrIn L1
fn compare_eq_or_in(lhs: &CandidatePath, rhs: &CandidatePath) -> isize {
    if !lhs.path.partial_index_paths.is_empty() || !rhs.path.partial_index_paths.is_empty() {
        // If either path has partial index paths, we cannot reliably compare EqOrIn conditions.
        return 0;
    }
    if lhs.eq_or_in_count > rhs.eq_or_in_count {
        return 1;
    }
    if lhs.eq_or_in_count < rhs.eq_or_in_count {
        return -1;
    }
    // We didn't find a winner
    0
}

// go: isFullIndexMatch L1
fn is_full_index_match(candidate: &CandidatePath) -> bool {
    let index_cols = candidate
        .path
        .index
        .as_ref()
        .and_then(|i| i.columns.as_deref())
        .map_or(0, <[_]>::len) as isize;
    // Check if the DNF condition is a full match
    if candidate.path.is_dnf_cond && candidate.has_only_equal_predicates_in_dnf() {
        return candidate.path.min_access_conds_for_dnf_cond >= index_cols;
    }
    // Check if the index covers all access conditions for non-DNF conditions
    candidate.path.eq_or_in_cond_count > 0
        && candidate.index_conds_col_map.len() as isize >= index_cols
}

/// `matchProperty`. The vector property (vector indexes) is not ported (TiFlash ANN
/// indexes, outside the first slice): a property with a vector helper is a boundary.
// go: matchProperty L2 (the vector index property is a boundary of D10.1)
pub(crate) fn match_property(
    ds: &DataSource,
    ctx: &PlanCtx<'_>,
    path: &mut AccessPath,
    prop: &PhysicalProperty,
) -> Result<PhysicalPropMatchResult, Error> {
    // This function may set the two fields below for the PropMatchedNeedMergeSort case, so we reset them here to
    // avoid leaving the AccessPath with an inconsistent state when there are multiple calls to matchProperty with
    // different properties.
    path.grouped_ranges = Vec::new();
    path.group_by_col_idxs = Vec::new();

    if ds.is_cluster_table && !prop.is_sort_item_empty() {
        // TableScan with cluster table can't keep order.
        return Ok(PropNotMatched);
    }
    if prop.vector_prop.vector_helper.is_some()
        && path.index.as_ref().is_some_and(|i| i.vector_info.is_some())
    {
        // glue: blocked-by:D10.1 (vector index properties: TiFlash ANN indexes)
        return Err(blocked(
            "D10.1",
            "a vector index property (TiFlash ANN index)",
        ));
    }
    if path.is_int_handle_path {
        let pk_col = ds.get_pk_is_handle_col(ctx);
        if prop.sort_items.len() != 1 || pk_col.is_none() {
            return Ok(PropNotMatched);
        }
        let item = &prop.sort_items[0];
        let pk_col = pk_col.expect("checked");
        if !item.col.equal_column(&Expr::Column(Column::clone(&pk_col)))
            || path.store_type == kv::TiFlash && item.desc
        {
            return Ok(PropNotMatched);
        }
        return Ok(PropMatched);
    }
    let (all, _) = prop.all_same_order();
    // When the prop is empty or `all` is false, `matchProperty` is better to be `PropNotMatched` because
    // it needs not to keep order for index scan.
    if prop.is_sort_item_empty() || !all || path.idx_cols.len() < prop.sort_items.len() {
        return Ok(PropNotMatched);
    }

    // Basically, if `prop.SortItems` is the prefix of `path.IdxCols`, then the property is matched.
    // However, we need to consider the situations when some columns of `path.IdxCols` are evaluated as constant.
    // For example:
    // ```text
    // create table t(a int, b int, c int, d int, index idx_a_b_c(a, b, c), index idx_d_c_b_a(d, c, b, a));
    // select * from t where a = 1 order by b, c;
    // select * from t where b = 1 order by a, c;
    // select * from t where d = 1 and b = 2 order by c, a;
    // select * from t where d = 1 and b = 2 order by c, b, a;
    // ```
    // In the first two `SELECT` statements, `idx_a_b_c` matches the sort order.
    // In the last two `SELECT` statements, `idx_d_c_b_a` matches the sort order.
    // Hence, we use `path.ConstCols` to deal with the above situations. This corresponds to Case 2 in the code below.
    //
    // Moreover, we also need to consider the situation when some columns of `path.IdxCols` are a list of constant
    // values. For example, for query `SELECT * FROM t WHERE a IN (1,2,3) ORDER BY b, c`, we can use `idx_a_b_c` to
    // access 3 ranges and use a merge sort to satisfy `ORDER BY b, c`. This corresponds to Case 3 in the code below.
    let mut match_result = PropMatched;
    let mut group_by_col_idxs: Vec<isize> = Vec::new();
    let mut col_idx = 0usize;
    let tc = ctx.vars.stmt_ctx().type_ctx();
    for sort_item in &prop.sort_items {
        let mut found = false;
        while col_idx < path.idx_cols.len() {
            // Case 1: this sort item is satisfied by the index column, go to match the next sort item.
            if path.idx_col_lens[col_idx] == UnspecifiedLength
                && sort_item
                    .col
                    .equal_column(&Expr::Column(Column::clone(&path.idx_cols[col_idx])))
            {
                found = true;
                col_idx += 1;
                break;
            }
            // Below: this sort item is not satisfied by the index column

            // Case 2: the accessed range on this index column is a single constant value, so we can skip this
            // column and try to use the next index column to satisfy this sort item.
            if !path.const_cols.is_empty()
                && col_idx < path.const_cols.len()
                && path.const_cols[col_idx]
            {
                col_idx += 1;
                continue;
            }

            // Check if the accessed range on this index column is a single constant value **for each range**.
            let mut all_ranges_point = true;
            for ran in &path.ranges {
                if ran.low_val.len() <= col_idx || ran.high_val.len() <= col_idx {
                    all_ranges_point = false;
                    break;
                }
                let (cmp_result, err) = ran.low_val[col_idx].compare(
                    &tc,
                    &ran.high_val[col_idx],
                    ran.collators[col_idx],
                );
                if err.is_err() || cmp_result != 0 {
                    all_ranges_point = false;
                    break;
                }
            }
            // Case 3: the accessed range on this index column is a constant value in each range respectively,
            // we can also skip this column and try the next index column. But we need to record this column
            // as a "group by" column, because we need to do a merge sort on these ranges later.
            // So `len(groupByColIdxs) > 0` means we need to do a merge sort to satisfy the required property.
            if all_ranges_point {
                group_by_col_idxs.push(col_idx as isize);
                col_idx += 1;
                continue;
            }
            // Case 4: cannot satisfy this sort item, the path cannot match the required property.
            break;
        }
        if !found {
            match_result = PropNotMatched;
            break;
        }
    }
    if !group_by_col_idxs.is_empty() && match_result == PropMatched {
        let Ok(groups) = group_ranges_by_cols(&path.ranges, &group_by_col_idxs) else {
            return Ok(PropNotMatched);
        };
        const MAX_GROUP_COUNT: usize = 2048;
        if groups.len() > MAX_GROUP_COUNT {
            return Ok(PropNotMatched);
        }
        if !groups.is_empty() {
            path.grouped_ranges = groups;
            path.group_by_col_idxs = group_by_col_idxs;
            return Ok(PropMatchedNeedMergeSort);
        }
    }
    Ok(match_result)
}

/// GroupRangesByCols groups the ranges by the values of the columns specified by groupByColIdxs.
///
/// Go collects values from a `map[string][]*ranger.Range` in random order; Rust groups in
/// deterministic first-appearance order (`M-MAPORDER`), which is always one of Go's valid outputs.
// go: GroupRangesByCols L2 (M-MAPORDER: groups in deterministic first-appearance order instead of Go map iteration order)
pub fn group_ranges_by_cols(
    ranges: &[Range],
    group_by_col_idxs: &[isize],
) -> Result<Vec<Vec<Range>>, Error> {
    let mut group_idx_by_key: HashMap<Vec<u8>, usize> = HashMap::new();
    let mut result: Vec<Vec<Range>> = Vec::new();
    let utc_loc = utc();
    for ran in ranges {
        let mut datums: Vec<Datum> = Vec::with_capacity(group_by_col_idxs.len());
        for &idx in group_by_col_idxs {
            datums.push(ran.low_val[idx as usize].clone());
        }
        // We just use it to group the values, so any time zone is ok.
        let mut key_bytes = Vec::new();
        tidb_codec::encode_value(&utc_loc, &mut key_bytes, &datums)?;
        if let Some(&pos) = group_idx_by_key.get(&key_bytes) {
            result[pos].push(ran.clone());
        } else {
            let pos = result.len();
            group_idx_by_key.insert(key_bytes, pos);
            result.push(vec![ran.clone()]);
        }
    }
    Ok(result)
}

/// matchPartialOrderProperty checks if the index can provide partial order for TopN
/// optimization. Unlike matchProperty, this function allows a prefix index to match ORDER BY
/// columns.
// go: matchPartialOrderProperty L1
fn match_partial_order_property(
    path: &AccessPath,
    partial_order_info: Option<&PartialOrderInfo>,
) -> PartialOrderMatchResult {
    let empty_result = PartialOrderMatchResult::default();
    let Some(partial_order_info) = partial_order_info else {
        return empty_result;
    };
    let Some(index) = &path.index else {
        return empty_result;
    };
    if path.idx_cols.is_empty() {
        return empty_result;
    }

    let sort_items = &partial_order_info.sort_items;
    if sort_items.is_empty() {
        return empty_result;
    }

    let (all_same_order, _) = partial_order_info.all_same_order();
    if !all_same_order {
        return empty_result;
    }

    let index_columns = index.columns.as_deref().unwrap_or_default();
    let index_col_count = index_columns.len();
    if path.full_idx_cols.len() < index_col_count || path.full_idx_col_lens.len() < index_col_count
    {
        return empty_result;
    }
    if index_col_count > sort_items.len() {
        return empty_result;
    }
    if index_columns[index_col_count - 1].length == UnspecifiedLength {
        return empty_result;
    }

    for (i, sort_item) in sort_items.iter().enumerate().take(index_col_count) {
        let Some(idx_col) = &path.full_idx_cols[i] else {
            return empty_result;
        };
        if !sort_item
            .col
            .equal_column(&Expr::Column(Column::clone(idx_col)))
        {
            return empty_result;
        }
        if path.full_idx_col_lens[i] != UnspecifiedLength {
            if i != index_col_count - 1 {
                return empty_result;
            }
            return PartialOrderMatchResult {
                matched: true,
                prefix_col: Some(Arc::clone(idx_col)),
                prefix_len: path.full_idx_col_lens[i],
            };
        }
    }
    empty_result
}

// go: getTableCandidate L1 [M-CTX]
fn get_table_candidate(
    ds: &DataSource,
    ctx: &PlanCtx<'_>,
    mut path: AccessPath,
    prop: &PhysicalProperty,
) -> Result<CandidatePath, Error> {
    let match_prop_result = match_property(ds, ctx, &mut path, prop)?;
    let mut candidate = CandidatePath {
        match_prop_result,
        access_conds_col_map: extract_col2_len(&*ctx.eval_ctx(), &path.access_conds, None, &[]),
        is_full_range: path.is_full_scan_range(&ds.table_info),
        path,
        ..Default::default()
    };
    candidate.eq_or_in_count = candidate.equal_predicate_count();
    Ok(candidate)
}

// go: getIndexCandidate L1 [M-CTX]
fn get_index_candidate(
    ds: &DataSource,
    ctx: &PlanCtx<'_>,
    mut path: AccessPath,
    prop: &PhysicalProperty,
) -> Result<CandidatePath, Error> {
    let match_prop_result = match_property(ds, ctx, &mut path, prop)?;
    let mut partial_order_match_result = PartialOrderMatchResult::default();
    if ctx.vars.is_partial_ordered_index_for_top_n_enabled() && prop.partial_order_info.is_some() {
        partial_order_match_result =
            match_partial_order_property(&path, prop.partial_order_info.as_deref());
    }
    let ectx = ctx.eval_ctx();
    let access_conds_col_map = extract_col2_len(
        &*ectx,
        &path.access_conds,
        Some(&path.idx_cols),
        &path.idx_col_lens,
    );
    let mut conds = path.access_conds.clone();
    conds.extend(path.index_filters.iter().map(Arc::clone));
    let full_cols: Vec<Arc<Column>> = path
        .full_idx_cols
        .iter()
        .flatten()
        .map(Arc::clone)
        .collect();
    // Go passes FullIdxCols with nil entries; extractCol2Len skips them with their lengths.
    let full_lens: Vec<isize> = path
        .full_idx_cols
        .iter()
        .zip(&path.full_idx_col_lens)
        .filter(|(c, _)| c.is_some())
        .map(|(_, l)| *l)
        .collect();
    let index_conds_col_map = extract_col2_len(&*ectx, &conds, Some(&full_cols), &full_lens);
    let mut candidate = CandidatePath {
        match_prop_result,
        partial_order_match_result,
        access_conds_col_map,
        index_conds_col_map,
        is_full_range: path.is_full_scan_range(&ds.table_info),
        path,
        ..Default::default()
    };
    candidate.eq_or_in_count = candidate.equal_predicate_count();
    Ok(candidate)
}

// glue: stub of expressionContainsHash, blocked-by:D10.7 (the filters of index merge paths)
// glue: stub of checkColinSchema, blocked-by:D10.7 (the partial table scans of index merge)
// glue: stub of PhysicalTableScan.addPushedDownSelectionToMppTask, blocked-by:D10.1 (the MPP task of a TiFlash table scan)
// glue: stub of isMatchPropForIndexMerge, blocked-by:D10.7 (index merge paths; no such path reaches skyline pruning)
/// The functions of index merge and MPP that FindBestTask does not reach in the first slice:
/// skyline pruning and the table path return their boundaries (index merge paths, TiFlash scans)
/// before them.
pub fn find_best_task_boundary(what: &str, block: &str) -> Error {
    blocked(block, what)
}

impl PhysicalTableScan {
    // go: PhysicalTableScan.appendExtraHandleCol L1 [M-CTX]
    fn append_extra_handle_col(
        &mut self,
        ds: &DataSource,
        ctx: &PlanCtx<'_>,
    ) -> (Arc<Column>, bool) {
        if let Some(hc) = &ds.handle_cols {
            return (
                Arc::clone(
                    hc.get_col(0)
                        .expect("runtime error: index out of range [0] with length 0"),
                ),
                false,
            );
        }
        let handle_col = Arc::new(ds.new_extra_handle_schema_col(ctx));
        self.sp.schema_mut().append([Arc::clone(&handle_col)]);
        self.columns
            .push(Arc::new(tidb_model::new_extra_handle_col_info()));
        (handle_col, true)
    }
}

/// skylinePruning prunes access paths according to different factors. An access path can be
/// pruned only if there exists a path that is not worse than it at all factors and there is
/// at least one better factor.
// go: skylinePruning L1 [M-CTX]
pub fn skyline_pruning(
    ds: &mut DataSource,
    ctx: &PlanCtx<'_>,
    prop: &PhysicalProperty,
) -> Result<Vec<CandidatePath>, Error> {
    let mut candidates: Vec<CandidatePath> = Vec::with_capacity(4);
    let mut idx_missing_stats = false;
    // tidb_opt_prefer_range_scan is the master switch to control index preferencing
    let mut prefer_range = ctx.vars.get_allow_prefer_range_scan();
    for i in 0..ds.possible_access_paths.len() {
        let path = &ds.possible_access_paths[i];
        // We should check whether the possible access path is valid first.
        if path.store_type != kv::TiFlash && prop.is_flash_prop() {
            continue;
        }
        if !path.partial_alternative_index_paths.is_empty() {
            // OR normal index merge path, try to determine every index partial path for this property.
            // glue: blocked-by:D10.7 (convergeIndexMergeCandidate: index merge)
            return Err(blocked(
                "D10.7",
                "an index merge path (convergeIndexMergeCandidate)",
            ));
        }
        if !path.partial_index_paths.is_empty() {
            // glue: blocked-by:D10.7 (getIndexMergeCandidate: index merge)
            return Err(blocked(
                "D10.7",
                "an index merge path (getIndexMergeCandidate)",
            ));
        }
        // if we already know the range of the scan is empty, just return a TableDual
        if path.ranges.is_empty() {
            return Ok(vec![CandidatePath {
                path: path.clone_path(),
                ..Default::default()
            }]);
        }
        let current_candidate = if path.is_table_path() {
            if prop.partial_order_info.is_some() {
                continue;
            }
            let c = get_table_candidate(ds, ctx, path.clone_path(), prop)?;
            // matchProperty writes the path (Go's pointer): keep the DataSource's path in step.
            ds.possible_access_paths[i].grouped_ranges = c.path.grouped_ranges.clone();
            ds.possible_access_paths[i].group_by_col_idxs = c.path.group_by_col_idxs.clone();
            c
        } else {
            let mut match_partial_order_index = false;
            if ctx.vars.is_partial_ordered_index_for_top_n_enabled()
                && prop.partial_order_info.is_some()
            {
                if !match_partial_order_property(path, prop.partial_order_info.as_deref()).matched {
                    continue;
                }
                match_partial_order_index = true;
                if path.forced && !path.force_no_keep_order {
                    ds.possible_access_paths[i].force_partial_order = true;
                }
            }
            let path = &ds.possible_access_paths[i];
            let keep_index = !path.access_conds.is_empty()
                || !prop.is_sort_item_empty()
                || path.forced
                || path.is_single_scan
                || match_partial_order_index;
            if !keep_index {
                continue;
            }
            // We will use index to generate physical plan if any of the following conditions is satisfied:
            // 1. This path's access cond is not nil.
            // 2. We have a non-empty prop to match.
            // 3. This index is forced to choose.
            // 4. The needed columns are all covered by index columns(and handleCol).
            // 5. It matches PartialOrderInfo physical property for partial order optimization.
            let c = get_index_candidate(ds, ctx, path.clone_path(), prop)?;
            ds.possible_access_paths[i].grouped_ranges = c.path.grouped_ranges.clone();
            ds.possible_access_paths[i].group_by_col_idxs = c.path.group_by_col_idxs.clone();
            c
        };
        let mut pruned = false;
        let mut j = candidates.len();
        while j > 0 {
            j -= 1;
            if candidates[j].path.store_type == kv::TiFlash {
                continue;
            }
            let (result, missing_stats) = compare_candidates(
                ctx,
                ds.statistic_table.as_deref(),
                prop,
                &candidates[j],
                &current_candidate,
                prefer_range,
            );
            if missing_stats {
                idx_missing_stats = true; // Ensure that we track idxMissingStats across all iterations
            }
            if result == 1 {
                pruned = true;
                // We can break here because the current candidate lost to another plan.
                // This means that we won't add it to the candidates below.
                break;
            } else if result == -1 {
                // The current candidate is better - so remove the old one from "candidates"
                candidates.remove(j);
            }
        }
        if !pruned {
            candidates.push(current_candidate);
        }
    }

    // If we've forced an index merge - we want to keep these plans
    let prefer_merge = crate::core::rule::rule_prune_indexes::should_prefer_index_merge(ds, ctx);
    if prefer_range {
        // Override preferRange with the following limitations to scope
        let table_stats = ds.table_stats.as_ref().expect("initStats");
        prefer_range = prefer_merge
            || idx_missing_stats
            || table_stats.hist_coll.as_ref().is_some_and(|h| h.pseudo)
            || table_stats.row_count < 1.0;
    }
    if prefer_range && candidates.len() > 1 {
        // If a candidate path is TiFlash-path or forced-path or MV index, we just keep them. For other candidate paths, if there exists
        // any range scan path, we remove full scan paths and keep range scan paths.
        let mut preferred_paths = Vec::with_capacity(candidates.len());
        let mut has_range_scan_path = false;
        for c in &candidates {
            if c.path.forced
                || c.path.store_type == kv::TiFlash
                || c.path
                    .index
                    .as_ref()
                    .is_some_and(|i| i.global || i.mv_index)
            {
                preferred_paths.push(c.clone());
                continue;
            }
            // Preference plans with equals/IN predicates or where there is more filtering in the index than against the table
            let index_filters =
                c.eq_or_in_count > 0 || c.path.table_filters.len() < c.path.index_filters.len();
            if (prefer_merge
                || ((c.path.is_single_scan || index_filters)
                    && (prop.is_sort_item_empty() || c.match_prop_result.matched())))
                && !c.is_full_range
            {
                preferred_paths.push(c.clone());
                has_range_scan_path = true;
            }
        }
        if has_range_scan_path {
            return Ok(preferred_paths);
        }
    }

    Ok(candidates)
}

impl CandidatePath {
    /// hasOnlyEqualPredicatesInDNF checks if all access conditions in DNF form contain at
    /// least one equal predicate
    // go: candidatePath.hasOnlyEqualPredicatesInDNF L1
    fn has_only_equal_predicates_in_dnf(&self) -> bool {
        // Helper function to check if a condition is an equal/IN predicate or a LogicOr of equal/IN predicates
        // go-part: candidatePath.hasOnlyEqualPredicatesInDNF
        fn is_equal_predicate_or_or(expr: &Expr) -> bool {
            let Expr::ScalarFunction(sf) = expr else {
                return false;
            };

            // Reject NOT operators - they can make predicates non-equal
            if sf.func_name.l == tidb_expression::scalar_function::ast_func::UnaryNot {
                return false;
            }

            if sf.func_name.l == tidb_expression::scalar_function::ast_func::LogicOr
                || sf.func_name.l == tidb_expression::scalar_function::ast_func::LogicAnd
            {
                for arg in sf.get_args() {
                    if !is_equal_predicate_or_or(arg) {
                        return false;
                    }
                }
                return true;
            }

            // Check if it's an equal predicate (eq) or IN predicate (in)
            // Also reject any other comparison operators that are not equal/IN
            if sf.func_name.l == tidb_expression::scalar_function::ast_func::EQ
                || sf.func_name.l == tidb_expression::scalar_function::ast_func::In
            {
                return true;
            }

            // Reject all other comparison operators (LT, GT, LE, GE, NE, etc.)
            // and any other functions that are not equal/IN predicates
            false
        }

        // Check all access conditions
        for cond in &self.path.access_conds {
            if !is_equal_predicate_or_or(cond) {
                return false;
            }
        }
        true
    }

    // go: candidatePath.equalPredicateCount L1
    fn equal_predicate_count(&self) -> isize {
        if self.index_join_cols > 0 {
            // this candidate path is for Index Join
            // Specially handle indexes under Index Join, since these indexes can't see the join key themselves, we can't
            // use path.EqOrInCondCount here.
            // For example, for "where t1.a=t2.a and t2.b=1" and index "idx(t2, a, b)", since the DataSource of t2 can't
            // see the join key "t1.a=t2.a", it only considers "t2.b=1" as access condition, so its EqOrInCondCount is 0,
            // but actually it should be 2.
            return self.index_join_cols;
        }
        // Exit if this isn't a DNF condition or has no access conditions
        if !self.path.is_dnf_cond || self.path.access_conds.is_empty() {
            return self.path.eq_or_in_cond_count;
        }
        if self.has_only_equal_predicates_in_dnf() {
            return self.path.min_access_conds_for_dnf_cond;
        }
        0.max(self.path.min_access_conds_for_dnf_cond - 1)
    }
}

/// `getPruningInfo`. Go's `item.String()` of a sort item and `TaskTp`'s `%s`.
// go: getPruningInfo L1
fn get_pruning_info(
    ds: &DataSource,
    candidates: &[CandidatePath],
    prop: &PhysicalProperty,
) -> String {
    if candidates.len() == ds.possible_access_paths.len() {
        return String::new();
    }
    if candidates.len() == 1 && candidates[0].path.ranges.is_empty() {
        // For TableDual, we don't need to output pruning info.
        return String::new();
    }
    let mut names = Vec::with_capacity(candidates.len());
    let table_name = match &ds.table_as_name {
        Some(n) if !n.o.is_empty() => n.o.clone(),
        _ => ds.table_info.name.o.clone(),
    };
    let get_simple_path_name = |path: &AccessPath| -> String {
        if path.is_table_path() {
            if path.store_type == kv::TiFlash {
                return format!("{table_name}(tiflash)");
            }
            return table_name.clone();
        }
        path.index.as_ref().expect("index path").name.o.clone()
    };
    for cand in candidates {
        if !cand.path.partial_index_paths.is_empty() {
            let partial_names: Vec<String> = cand
                .path
                .partial_index_paths
                .iter()
                .map(get_simple_path_name)
                .collect();
            names.push(format!("IndexMerge{{{}}}", partial_names.join(",")));
        } else {
            names.push(get_simple_path_name(&cand.path));
        }
    }
    let items: Vec<String> = prop
        .sort_items
        .iter()
        .map(|i| String::from_utf8_lossy(&i.to_string()).into_owned())
        .collect();
    format!(
        "[{}] remain after pruning paths for {} given Prop{{SortItems: [{}], TaskTp: {}}}",
        names.join(","),
        table_name,
        items.join(" "),
        prop.task_tp.to_string()
    )
}

// go: isPointGetConvertableSchema L1
fn is_point_get_convertable_schema(ds: &DataSource) -> bool {
    for col in &ds.columns {
        if col.name.l == ExtraHandleName.l {
            continue;
        }

        // Only handle tables that all columns are public.
        if col.state != StatePublic {
            return false;
        }
    }
    true
}

/// exploreEnforcedPlan determines whether to explore enforced plans for this DataSource if
/// it has already found an unenforced plan. See #46177 for more information.
// go: exploreEnforcedPlan L1 [M-CTX]
fn explore_enforced_plan(ctx: &PlanCtx<'_>) -> bool {
    // default value is true which is different than original implementation.
    fixcontrol::get_bool_with_default(
        ctx.vars.get_optimizer_fix_control_map(),
        fixcontrol::Fix46177,
        true,
    )
}

/// `findBestTask4LogicalDataSource`. The table sample (`ds.SampleInfo`) stops in the plan
/// builder (D02.2), so `convertToSampleTable` and `validateTableSamplePlan` are never
/// reached. `prop` is Go's pointer argument: Go clears its sort items while exploring the
/// enforced plans and restores it afterwards; here the explored property is a copy that
/// keeps the cached hash code (Go's cached `hashcode` field survives the temporary changes,
/// so the memo key is the original one).
// go: findBestTask4LogicalDataSource L1 [M-PLAN, M-CTX]
pub fn find_best_task4_logical_data_source(
    ds: &mut DataSource,
    ctx: &mut PlanCtx<'_>,
    this: PlanId,
    memo: &mut TaskMap,
    prop: &PhysicalProperty,
    plan_counter: &mut PlanCounterTp,
    opt: &PhysicalOptimizeOp,
) -> Result<(Task, i64), Error> {
    // If ds is an inner plan in an IndexJoin, the IndexJoin will generate an inner plan by itself,
    // and set inner child prop nil, so here we do nothing: a nil prop is not representable
    // (child_req_prop returns its boundary).
    let isolation_read_engines_has_tikv = ctx.vars.get_isolation_read_engines().contains(&kv::TiKV);
    if ds.is_for_update_read
        && ctx.vars.state.txn_ctx.is_explicit
        && isolation_read_engines_has_tikv
    {
        let mut has_point_get_path = false;
        for path in &ds.possible_access_paths {
            if is_point_get_path(ds, ctx, path) {
                has_point_get_path = true;
                break;
            }
        }
        let tbl_name = ds.table_info.name.clone();
        let paths = std::mem::take(&mut ds.possible_access_paths);
        ds.possible_access_paths = crate::core::planbuilder::filter_path_by_isolation_read(
            ctx.vars,
            paths,
            &tbl_name,
            &ds.db_name,
        )?;
        if has_point_get_path {
            // if the path is the point get range path with for update lock, we should forbid tiflash as it's store path (#39543)
            ds.possible_access_paths
                .retain(|p| p.store_type != kv::TiFlash);
        }
    }
    if let Some(t) = get_task(memo, this, prop) {
        plan_counter.dec(1);
        return Ok((t, 1));
    }
    let _ = prop.hash_code();
    let mut unenforced_task: Option<Task> = None;
    // If prop.CanAddEnforcer is true, the prop.SortItems need to be set nil for ds.findBestTask.
    // Before function return, reset it for enforcing task prop and storing map<prop,task>.
    let explored: PhysicalProperty = if prop.can_add_enforcer {
        // First, get the bestTask without enforced prop
        let mut unenforced_prop = prop.clone();
        unenforced_prop.can_add_enforcer = false;
        let (t, _cnt) = find_best_task4_logical_data_source(
            ds,
            ctx,
            this,
            memo,
            &unenforced_prop,
            plan_counter,
            opt,
        )?;
        if !t.invalid() && !explore_enforced_plan(ctx) {
            store_task(memo, ctx, this, prop, t.clone());
            return Ok((t, _cnt));
        }
        unenforced_task = Some(t);

        // Then, explore the bestTask with enforced prop
        let mut p = prop.clone();
        p.sort_items = Vec::new();
        p.mpp_partition_tp = AnyType;
        p
    } else if prop.mpp_partition_tp != AnyType {
        return Ok((Task::invalid_task(), 0));
    } else {
        prop.clone()
    };

    let (mut t, cnt_plan) =
        find_best_task4_data_source_paths(ds, ctx, &explored, plan_counter, opt)?;

    // The deferred function of Go:
    if prop.can_add_enforcer {
        // *prop = *oldProp
        t = enforce_property(prop, t, ctx)?;
    }
    if let Some(unenforced_task) = unenforced_task
        && !unenforced_task.invalid()
        && compare_task_cost(ctx, &unenforced_task, &t, opt)?
    {
        t = unenforced_task;
    }
    store_task(memo, ctx, this, prop, t.clone());
    // err = validateTableSamplePlan(ds, t, err): no table sample (doc).
    Ok((t, cnt_plan))
}

/// The body of `findBestTask4LogicalDataSource` after its deferred function is registered:
/// the dual task and the candidates of the skyline pruning. Returns Go's `(t, cntPlan)`.
// go-part: findBestTask4LogicalDataSource
fn find_best_task4_data_source_paths(
    ds: &mut DataSource,
    ctx: &mut PlanCtx<'_>,
    prop: &PhysicalProperty,
    plan_counter: &mut PlanCounterTp,
    opt: &PhysicalOptimizeOp,
) -> Result<(Task, i64), Error> {
    if let Some(t) = try_to_get_dual_task(ds, ctx)? {
        plan_counter.dec(1);
        append_candidate(Some(&t), prop, opt);
        return Ok((t, 1));
    }

    let mut t = Task::invalid_task();
    let candidates = skyline_pruning(ds, ctx, prop)?;
    let pruning_info = get_pruning_info(ds, &candidates, prop);
    let r = convert_candidates(ds, ctx, prop, candidates, &mut t, plan_counter, opt);
    // The deferred note of the pruning information.
    if let Ok(_) = &r
        && !t.invalid()
        && !pruning_info.is_empty()
    {
        let warn_err = Error::plain(pruning_info.clone());
        let sc = ctx.vars.stmt_ctx();
        if sc.in_verbose_explain {
            sc.append_note(warn_err);
        } else {
            sc.append_extra_note(warn_err);
        }
    }
    let cnt_plan = r?;
    Ok((t, cnt_plan))
}

/// The loop of `findBestTask4LogicalDataSource` over the candidates; `t` is Go's named
/// result, the count is returned.
// go-part: findBestTask4LogicalDataSource
fn convert_candidates(
    ds: &mut DataSource,
    ctx: &mut PlanCtx<'_>,
    prop: &PhysicalProperty,
    candidates: Vec<CandidatePath>,
    t: &mut Task,
    plan_counter: &mut PlanCounterTp,
    opt: &PhysicalOptimizeOp,
) -> Result<i64, Error> {
    let mut cnt_plan: i64 = 0;
    for candidate in candidates {
        let path = &candidate.path;
        if !path.partial_index_paths.is_empty() {
            // glue: blocked-by:D10.7 (convertToIndexMergeScan: index merge)
            return Err(blocked(
                "D10.7",
                "an index merge path (convertToIndexMergeScan)",
            ));
        }
        // if we already know the range of the scan is empty, just return a TableDual
        if path.ranges.is_empty() {
            // We should uncache the tableDual plan.
            if tidb_expression::constant_fold::maybe_over_optimized4_plan_cache(
                &*ctx.expr_ctx,
                &path.access_conds,
            ) {
                ctx.expr_ctx.set_skip_plan_cache("get a TableDual plan");
            }
            let mut dual = PhysicalTableDual::default().init(
                ctx,
                ds.base.plan.stats_info().cloned(),
                ds.query_block_offset(),
            );
            dual.set_schema(ds.schema(&ctx.arena).clone_schema());
            cnt_plan += 1;
            plan_counter.dec(1);
            let mut rt = RootTask::default();
            rt.set_plan(Arc::new(dual));
            let task = Task::Root(rt);
            append_candidate(Some(&task), prop, opt);
            *t = task;
            return Ok(cnt_plan);
        }

        let mut can_convert_point_get = !path.ranges.is_empty()
            && path.store_type == kv::TiKV
            && is_point_get_convertable_schema(ds);
        if fixcontrol::get_bool_with_default(
            ctx.vars.get_optimizer_fix_control_map(),
            fixcontrol::Fix52592,
            false,
        ) {
            can_convert_point_get = false;
        }

        if can_convert_point_get && path.index.as_ref().is_some_and(|i| i.mv_index) {
            can_convert_point_get = false; // cannot use PointGet upon MVIndex
        }

        if can_convert_point_get && !path.is_int_handle_path {
            let index = path.index.as_ref().expect("an index path");
            // We simply do not build [batch] point get for prefix indexes. This can be optimized.
            can_convert_point_get = index.unique && !index.has_prefix_index();
            // If any range cannot cover all columns of the index, we cannot build [batch] point get.
            let idx_cols_len = index.columns.as_deref().map_or(0, <[_]>::len);
            for ran in &path.ranges {
                if ran.low_val.len() != idx_cols_len {
                    can_convert_point_get = false;
                    break;
                }
            }
        }
        if can_convert_point_get && ds.table_info.get_partition_info().is_some() {
            // glue: blocked-by:D09.12 (partitioned tables stop in the plan builder; the point get of a partition comes with them)
            return Err(blocked("D09.12", "the point get of a partitioned table"));
        }
        if can_convert_point_get {
            let mut all_range_is_point = true;
            let tc = ctx.vars.stmt_ctx().type_ctx();
            for ran in &path.ranges {
                if !ran.is_point_non_nullable(&tc) {
                    // unique indexes can have duplicated NULL rows so we cannot use PointGet if there is NULL
                    all_range_is_point = false;
                    break;
                }
            }
            if all_range_is_point {
                let point_get_task = if path.ranges.len() == 1 {
                    convert_to_point_get(ds, ctx, prop, &candidate)?
                } else {
                    // glue: blocked-by:D10.8 (convertToBatchPointGet: BatchPointGet)
                    return Err(blocked("D10.8", "BatchPointGet (convertToBatchPointGet)"));
                };

                // Batch/PointGet plans may be over-optimized, like `a>=1(?) and a<=1(?)` --> `a=1` --> PointGet(a=1).
                // For safety, prevent these plans from the plan cache here.
                if !point_get_task.invalid()
                    && tidb_expression::constant_fold::maybe_over_optimized4_plan_cache(
                        &*ctx.expr_ctx,
                        &candidate.path.access_conds,
                    )
                    && !crate::core::plan_cache_utils::is_safe_point_get_path4_plan_cache(
                        ctx,
                        &candidate.path,
                    )
                {
                    ctx.expr_ctx
                        .set_skip_plan_cache("Batch/PointGet plans may be over-optimized");
                }

                append_candidate(Some(&point_get_task), prop, opt);
                if !point_get_task.invalid() {
                    cnt_plan += 1;
                    plan_counter.dec(1);
                }
                let cur_is_better = compare_task_cost(ctx, &point_get_task, t, opt)?;
                if cur_is_better || plan_counter.empty() {
                    *t = point_get_task;
                    if plan_counter.empty() {
                        return Ok(cnt_plan);
                    }
                    continue;
                }
            }
        }
        if path.is_table_path() {
            // prefer tiflash, while current table path is tikv, skip it.
            if ds.prefer_store_type & crate::hint::hint::PreferTiFlash != 0
                && path.store_type == kv::TiKV
            {
                continue;
            }
            // prefer tikv, while current table path is tiflash, skip it.
            if ds.prefer_store_type & crate::hint::hint::PreferTiKV != 0
                && path.store_type == kv::TiFlash
            {
                continue;
            }
            // ds.SampleInfo != nil: the table sample stops in the plan builder (D02.2).
            let tbl_task = convert_to_table_scan(ds, ctx, prop, &candidate, opt)?;
            if !tbl_task.invalid() {
                cnt_plan += 1;
                plan_counter.dec(1);
            }
            append_candidate(Some(&tbl_task), prop, opt);
            let cur_is_better = compare_task_cost(ctx, &tbl_task, t, opt)?;
            if cur_is_better || plan_counter.empty() {
                *t = tbl_task;
            }
            if plan_counter.empty() {
                return Ok(cnt_plan);
            }
            continue;
        }
        // TiFlash storage do not support index scan.
        if ds.prefer_store_type & crate::hint::hint::PreferTiFlash != 0 {
            continue;
        }
        // TableSample do not support index scan: no table sample (doc).
        let idx_task = convert_to_index_scan(ds, ctx, prop, &candidate, opt)?;
        if !idx_task.invalid() {
            cnt_plan += 1;
            plan_counter.dec(1);
        }
        append_candidate(Some(&idx_task), prop, opt);
        let cur_is_better = compare_task_cost(ctx, &idx_task, t, opt)?;
        if cur_is_better || plan_counter.empty() {
            *t = idx_task;
        }
        if plan_counter.empty() {
            return Ok(cnt_plan);
        }
    }
    Ok(cnt_plan)
}

/// convertToIndexScan converts the DataSource to index scan with idx.
// go: convertToIndexScan L1 [M-CTX]
fn convert_to_index_scan(
    ds: &mut DataSource,
    ctx: &PlanCtx<'_>,
    prop: &PhysicalProperty,
    candidate: &CandidatePath,
    _opt: &PhysicalOptimizeOp,
) -> Result<Task, Error> {
    let path = &candidate.path;
    if path.index.as_ref().is_some_and(|i| i.mv_index) {
        // MVIndex is special since different index rows may return the same _row_id and this can break some assumptions of IndexReader.
        // Currently only support using IndexMerge to access MVIndex instead of IndexReader.
        // TODO: make IndexReader support accessing MVIndex directly.
        return Ok(Task::invalid_task());
    }
    if !path.is_single_scan {
        // If it's parent requires single read task, return max cost.
        if prop.task_tp == CopSingleReadTaskType {
            return Ok(Task::invalid_task());
        }
    } else if prop.task_tp == CopMultiReadTaskType {
        // If it's parent requires double read task, return max cost.
        return Ok(Task::invalid_task());
    }
    if !prop.is_sort_item_empty() && !candidate.match_prop_result.matched() {
        return Ok(Task::invalid_task());
    }
    // If we need to keep order for the index scan, we should forbid the non-keep-order index scan when we try to generate the path.
    if !prop.need_keep_order() && path.force_keep_order {
        return Ok(Task::invalid_task());
    }
    // If we don't need to keep order for the index scan, we should forbid the non-keep-order index scan when we try to generate the path.
    if prop.need_keep_order() && path.force_no_keep_order {
        return Ok(Task::invalid_task());
    }
    // If partial order is forced, reject normal full-order or no-order candidates.
    if path.force_partial_order && prop.partial_order_info.is_none() {
        return Ok(Task::invalid_task());
    }
    let mut is = get_original_physical_index_scan(
        ds,
        ctx,
        prop,
        path,
        candidate.match_prop_result.matched(),
        path.is_single_scan,
    )?;
    let mut cop = CopTask {
        tbl_col_hists: ds.tbl_col_hists.clone(),
        tbl_cols: ds.tbl_cols.clone(),
        expect_cnt: f64_to_u64(prop.expected_cnt), // [M-F2I]
        ..Default::default()
    };
    if candidate.partial_order_match_result.matched {
        cop.partial_order_match_result =
            Some(Arc::new(candidate.partial_order_match_result.clone()));
    }
    let pruning_conds = push_down_not_conds(ctx, &mut ds.all_conds);
    cop.phys_plan_part_info = Some(Arc::new(PhysPlanPartInfo {
        pruning_conds,
        partition_names: ds.partition_names.clone(),
        columns: ds.tbl_cols.clone(),
        column_names: ds.output_names(&ctx.arena).clone(),
    }));
    let mut ts: Option<PhysicalTableScan> = if !path.is_single_scan {
        // On this way, it's double read case.
        let mut ts = PhysicalTableScan {
            columns: clone_col_infos(&ds.columns),
            table: Arc::clone(&is.table),
            table_as_name: ds.table_as_name.clone(),
            db_name: ds.db_name.clone(),
            is_partition: ds.partition_def_idx.is_some(),
            physical_table_id: ds.physical_table_id,
            tbl_cols: ds.tbl_cols.clone(),
            tbl_col_hists: ds.tbl_col_hists.clone(),
            ..Default::default()
        }
        .init(ctx, is.query_block_offset());
        ts.set_schema(ds.schema(&ctx.arena).clone_schema());
        // We set `StatsVersion` here and fill other fields in `(*copTask).finishIndexPlan`. Since `copTask.indexPlan` may
        // change before calling `(*copTask).finishIndexPlan`, we don't know the stats information of `ts` currently and on
        // the other hand, it may be hard to identify `StatsVersion` of `ts` in `(*copTask).finishIndexPlan`.
        ts.set_stats(Some(Arc::new(StatsInfo {
            stats_version: ds.table_stats.as_ref().expect("initStats").stats_version,
            ..Default::default()
        })));
        cop.index_look_up_push_down_by = path.index_look_up_push_down_by;
        Some(ts)
    } else {
        None
    };
    if let Some(ts_mut) = &mut ts
        && ds.table_info.is_common_handle
    {
        cop.common_handle_cols = ds.common_handle_cols.clone();
        let Some(crate::util::HandleCols::Common(common_handle)) = &ds.handle_cols else {
            panic!("interface conversion: util.HandleCols is not *util.CommonHandleCols");
        };
        let ds_schema = ds.schema(&ctx.arena);
        for col in common_handle.get_columns() {
            if ds_schema.column_index(col) == -1 {
                ts_mut.sp.schema_mut().append([Arc::clone(col)]);
                ts_mut.columns.push(Arc::new(col.to_info()));
                cop.need_extra_proj = true;
            }
        }
    }
    if prop.need_keep_order() {
        cop.keep_order = true;
        if let Some(ts_mut) = &mut ts
            && !ds.table_info.is_common_handle
        {
            let (col, is_new) = ts_mut.append_extra_handle_col(ds, ctx);
            cop.extra_handle_col = Some(col);
            cop.need_extra_proj = cop.need_extra_proj || is_new;
        }

        // Add sort items for index scan for the merge-sort operation
        // Case 1: keep order between partitions (only required for local index)
        // Case 2: keep order between range groups (see comments of PropMatchedNeedMergeSort and
        // AccessPath.GroupedRanges for details)
        // Case 3: both
        if (ds.table_info.get_partition_info().is_some() && !is.index.global)
            || candidate.match_prop_result == PropMatchedNeedMergeSort
        {
            is.by_items = prop
                .get_sort_items_for_keep_order()
                .into_iter()
                .map(|si| ByItems {
                    expr: Arc::new(Expr::Column(Column::clone(&si.col))),
                    desc: si.desc,
                })
                .collect();
        }
        if candidate.match_prop_result == PropMatchedNeedMergeSort {
            is.grouped_ranges = path.grouped_ranges.clone();
            is.group_by_col_idxs = path.group_by_col_idxs.clone();
        }
        if ds.table_info.get_partition_info().is_some()
            && let Some(ts_mut) = &mut ts
            && ctx.vars.stmt_ctx().use_dynamic_partition_prune()
        {
            if !is.index.global {
                let is_cols = std::mem::take(&mut is.columns);
                let is_schema = is.sp.schema().clone_schema();
                let (new_cols, new_schema, _) =
                    crate::core::physical_plans::add_extra_phys_tbl_id_column(
                        ctx, is_cols, is_schema,
                    );
                is.columns = new_cols;
                is.sp.set_schema(Some(new_schema));
            }
            // global index for tableScan with keepOrder also need PhysicalTblID
            let ts_cols = std::mem::take(&mut ts_mut.columns);
            let ts_schema = ts_mut.sp.schema().clone_schema();
            let (new_cols, new_schema, succ) =
                crate::core::physical_plans::add_extra_phys_tbl_id_column(ctx, ts_cols, ts_schema);
            ts_mut.columns = new_cols;
            ts_mut.sp.set_schema(Some(new_schema));
            cop.need_extra_proj = cop.need_extra_proj || succ;
        }
    }
    if let Some(ts_owned) = ts {
        cop.table_plan = Some(Arc::new(ts_owned));
    }
    if cop.need_extra_proj {
        cop.origin_schema = Some(Arc::new(ds.schema(&ctx.arena).clone_schema()));
    }
    // prop.IsSortItemEmpty() would always return true when coming to here,
    // so we can just use prop.ExpectedCnt as parameter of addPushedDownSelection.
    let final_stats = ds_stats_of(ds).scale_by_expect_cnt(prop.expected_cnt);
    is.add_pushed_down_selection(ctx, &mut cop, ds, path, final_stats)?;
    let mut task = Task::Cop(cop);
    if prop.task_tp == RootTaskType {
        task = task.convert_to_root_task(ctx)?;
    } else if matches!(task, Task::Root(_)) {
        return Ok(Task::invalid_task());
    }
    Ok(task)
}

impl PhysicalIndexScan {
    /// NeedExtraOutputCol indicates whether this index scan needs an extra physical table ID column.
    // go: PhysicalIndexScan.NeedExtraOutputCol L1 [M-CTX]
    pub fn need_extra_output_col(&self, ctx: &PlanCtx<'_>) -> bool {
        if self.table.partition.is_none() {
            return false;
        }
        if self.index.global {
            return true;
        }
        if !self.by_items.is_empty() && ctx.vars.stmt_ctx().use_dynamic_partition_prune() {
            return true;
        }
        false
    }

    // go: PhysicalIndexScan.getScanRowSize L1 [M-CTX]
    pub fn get_scan_row_size(&self, ctx: &PlanCtx<'_>) -> f64 {
        let idx = &self.index;
        let idx_cols_len = idx.columns.as_deref().map_or(0, <[_]>::len);
        let mut scan_cols: Vec<Arc<Column>> = Vec::with_capacity(idx_cols_len + 1);
        // If `initSchema` has already appended the handle column in schema, just use schema columns, otherwise, add extra handle column.
        if idx_cols_len == self.schema().columns.len() {
            scan_cols.extend(self.schema().columns.iter().map(Arc::clone));
            if let Some(handle_col) = &self.pk_is_handle_col {
                scan_cols.push(Arc::clone(handle_col));
            }
        } else {
            scan_cols.extend(self.schema().columns.iter().map(Arc::clone));
        }
        let default_coll = tidb_statistics::HistColl::default();
        let coll = self.tbl_col_hists.as_deref().unwrap_or(&default_coll);
        crate::cardinality::row_size::get_index_avg_row_size(ctx, coll, &scan_cols, idx.unique)
    }

    /// initSchema is used to set the schema of PhysicalIndexScan.
    // go: PhysicalIndexScan.initSchema L1 [M-CTX]
    pub fn init_schema(
        &mut self,
        ctx: &PlanCtx<'_>,
        idx_expr_cols: &[Option<Arc<Column>>],
        is_double_read: bool,
    ) {
        let idx_meta_cols = self.index.columns.as_deref().unwrap_or(&[]);
        let idx_cols_len = idx_meta_cols.len();
        let table_cols = self.table.columns.as_deref().unwrap_or(&[]);
        let mut index_cols: Vec<Arc<Column>> = Vec::with_capacity(idx_cols_len + 1);
        index_cols.extend(self.idx_cols.iter().map(Arc::clone));

        for i in self.idx_cols.len()..idx_cols_len {
            if let Some(col) = idx_expr_cols.get(i).and_then(|c| c.as_ref()) {
                index_cols.push(Arc::clone(col));
            } else {
                let tbl_col = &table_cols[idx_meta_cols[i].offset as usize];
                index_cols.push(Arc::new(Column {
                    id: tbl_col.id,
                    ret_type: Arc::new(tbl_col.field_type.clone()),
                    unique_id: ctx.vars.alloc_plan_column_id(),
                    ..Default::default()
                }));
            }
        }
        self.need_common_handle = self.table.is_common_handle;

        if self.need_common_handle {
            for c in idx_expr_cols.iter().skip(idx_cols_len).flatten() {
                index_cols.push(Arc::clone(c));
            }
        }
        let mut set_handle = index_cols.len() > idx_cols_len;
        let ds_schema = self
            .data_source_schema
            .as_ref()
            .expect("runtime error: invalid memory address or nil pointer dereference");
        if !set_handle {
            for (i, col) in self.columns.iter().enumerate() {
                if (has_pri_key_flag(col.get_flag()) && self.table.pk_is_handle)
                    || col.id == ExtraHandleID as i64
                {
                    index_cols.push(Arc::clone(&ds_schema.columns[i]));
                    set_handle = true;
                    break;
                }
            }
        }

        let mut extra_phys_tbl_col: Option<Arc<Column>> = None;
        // If `dataSourceSchema` contains `model.ExtraPhysTblID`, we should add it into `indexScan.schema`
        for col in &ds_schema.columns {
            if col.id == ExtraPhysTblID as i64 {
                extra_phys_tbl_col = Some(Arc::new(Column::clone(col)));
                break;
            }
        }

        if is_double_read || self.index.global {
            // If it's double read case, the first index must return handle. So we should add extra handle column
            // if there isn't a handle column.
            if !set_handle && !self.table.is_common_handle {
                index_cols.push(Arc::new(Column {
                    ret_type: Arc::new(new_field_type(TypeLonglong)),
                    id: ExtraHandleID as i64,
                    unique_id: ctx.vars.alloc_plan_column_id(),
                    orig_name: ExtraHandleName.o.to_owned(),
                    ..Default::default()
                }));
            }
            // If it's global index, handle and PhysTblID columns has to be added, so that needed pids can be filtered.
            if self.index.global && extra_phys_tbl_col.is_none() {
                index_cols.push(Arc::new(Column {
                    ret_type: Arc::new(new_field_type(TypeLonglong)),
                    id: ExtraPhysTblID as i64,
                    unique_id: ctx.vars.alloc_plan_column_id(),
                    orig_name: ExtraPhysTblIDName.o.to_owned(),
                    ..Default::default()
                }));
            }
        }

        if let Some(col) = extra_phys_tbl_col {
            index_cols.push(col);
        }

        self.set_schema(new_schema(index_cols));
    }

    // go: PhysicalIndexScan.addSelectionConditionForGlobalIndex L2 (global index stops at D09.12 PartitionPruning)
    pub fn add_selection_condition_for_global_index(
        &self,
        _ds: &DataSource,
        _phys_plan_part_info: Option<&PhysPlanPartInfo>,
        conditions: Vec<ExprRef>,
    ) -> Result<Vec<ExprRef>, Error> {
        if !self.index.global {
            return Ok(conditions);
        }
        // glue: blocked-by:D09.12 (PartitionPruning for global index)
        Err(blocked(
            "D09.12",
            "PhysicalIndexScan.addSelectionConditionForGlobalIndex (PartitionPruning)",
        ))
    }

    // go: PhysicalIndexScan.addPushedDownSelection L1 [M-PLAN, M-CTX]
    pub fn add_pushed_down_selection(
        self,
        ctx: &PlanCtx<'_>,
        cop_task: &mut CopTask,
        ds: &DataSource,
        path: &AccessPath,
        final_stats: Arc<StatsInfo>,
    ) -> Result<(), Error> {
        // Add filter condition to table plan now.
        let (mut index_conds, mut table_conds) =
            (path.index_filters.clone(), path.table_filters.clone());
        (table_conds, cop_task.root_task_conds) = split_sel_conds_with_virtual_column(&table_conds);

        let pctx = ctx.push_down_ctx()?;
        let new_root_conds;
        (index_conds, new_root_conds) = push_down_exprs(&pctx, &index_conds, kv::TiKV);
        cop_task.root_task_conds.extend(new_root_conds);

        let new_root_conds;
        (table_conds, new_root_conds) = push_down_exprs(&pctx, &table_conds, kv::TiKV);
        cop_task.root_task_conds.extend(new_root_conds);

        // Add a `Selection` for `IndexScan` with global index.
        // It should pushdown to TiKV, DataSource schema doesn't contain partition id column.
        index_conds = self.add_selection_condition_for_global_index(
            ds,
            cop_task.phys_plan_part_info.as_deref(),
            index_conds,
        )?;

        let is_qb_offset = self.query_block_offset();
        let is_row_count = self.stats_info().expect("the scan's stats").row_count;
        let is_arc: PhysicalPlanRef = Arc::new(self);
        if !index_conds.is_empty() {
            let selectivity = if path.count_after_access > 0.0 {
                path.count_after_index / path.count_after_access
            } else {
                0.0
            };
            let count = is_row_count * selectivity;
            let stats = ds
                .table_stats
                .as_ref()
                .expect("initStats")
                .scale_by_expect_cnt(count);
            let mut index_sel = PhysicalSelection {
                conditions: index_conds,
                ..Default::default()
            }
            .init(ctx, Some(stats), is_qb_offset, Vec::new());
            index_sel.set_children(vec![is_arc]);
            cop_task.index_plan = Some(Arc::new(index_sel));
        } else {
            cop_task.index_plan = Some(is_arc);
        }
        if !table_conds.is_empty() {
            cop_task.finish_index_plan()?;
            let mut table_sel = PhysicalSelection {
                conditions: table_conds.clone(),
                ..Default::default()
            }
            .init(ctx, Some(final_stats), is_qb_offset, Vec::new());
            if !cop_task.root_task_conds.is_empty() {
                let coll = cop_task
                    .tbl_col_hists
                    .as_deref()
                    .cloned()
                    .unwrap_or_default();
                let selectivity = match crate::cardinality::selectivity::selectivity(
                    ctx,
                    &coll,
                    &table_conds,
                    &[],
                ) {
                    Ok((s, _)) => s,
                    Err(e) if tidb_base::blocked::blocked_by(&e).is_some() => return Err(e),
                    Err(_) => SelectionFactor,
                };
                let plan_stats = cop_task
                    .plan()
                    .expect("cop task plan")
                    .stats_info()
                    .expect("plan stats");
                table_sel.set_stats(Some(Arc::new(plan_stats.scale(selectivity))));
            }
            let table_plan = cop_task.table_plan.take().expect("tablePlan");
            table_sel.set_children(vec![table_plan]);
            cop_task.table_plan = Some(Arc::new(table_sel));
        }
        Ok(())
    }
}

/// `getOriginalPhysicalIndexScan`. The used statistics record (`usedStatsInfo`) is not carried
/// (stats.rs module doc). Returns a `Result` for the boundary of the row count under a limit.
// go: getOriginalPhysicalIndexScan L2 (Result for the boundary of AdjustRowCountForIndexScanByLimit)
fn get_original_physical_index_scan(
    ds: &DataSource,
    ctx: &PlanCtx<'_>,
    prop: &PhysicalProperty,
    path: &AccessPath,
    is_match_prop: bool,
    is_single_scan: bool,
) -> Result<PhysicalIndexScan, Error> {
    let idx = Arc::clone(
        path.index
            .as_ref()
            .expect("runtime error: invalid memory address or nil pointer dereference"),
    );
    let mut is = PhysicalIndexScan {
        table: Arc::clone(&ds.table_info),
        table_as_name: ds.table_as_name.clone(),
        db_name: ds.db_name.clone(),
        columns: clone_col_infos(&ds.columns),
        index: Arc::clone(&idx),
        idx_cols: path.idx_cols.clone(),
        idx_col_lens: path.idx_col_lens.clone(),
        access_condition: path.access_conds.clone(),
        ranges: path.ranges.clone(),
        data_source_schema: Some(ds.schema(&ctx.arena).clone_schema()),
        is_partition: ds.partition_def_idx.is_some(),
        physical_table_id: ds.physical_table_id,
        tbl_col_hists: ds.tbl_col_hists.clone(),
        pk_is_handle_col: ds.get_pk_is_handle_col(ctx),
        const_cols_by_cond: path.const_cols.clone(),
        prop: Some(Arc::new(prop.clone())),
        not_always_valid: path.part_idx_cond_not_always_valid,
        ..Default::default()
    }
    .init(ctx, ds.query_block_offset());
    let mut row_count = path.count_after_access;
    let mut idx_expr_cols = path.full_idx_cols.clone();
    idx_expr_cols.extend(ds.common_handle_cols.iter().map(|c| Some(Arc::clone(c))));
    is.init_schema(ctx, &idx_expr_cols, !is_single_scan);

    // If (1) tidb_opt_ordering_index_selectivity_threshold is enabled (not 0)
    // and (2) there exists an index whose selectivity is smaller than or equal to the threshold,
    // and (3) there is Selection on the IndexScan, we don't use the ExpectedCnt to
    // adjust the estimated row count of the IndexScan.
    let ignore_expected_cnt = ctx.vars.opt_ordering_idx_sel_thresh != 0.0
        && ds.access_path_min_selectivity <= ctx.vars.opt_ordering_idx_sel_thresh
        && (!path.index_filters.is_empty() || !path.table_filters.is_empty());

    let ds_stats = ds_stats_of(ds);
    let table_stats = ds.table_stats.as_ref().expect("initStats");
    if (is_match_prop || prop.is_sort_item_empty())
        && prop.expected_cnt < ds_stats.row_count
        && !ignore_expected_cnt
    {
        row_count = crate::cardinality::cross_estimation::adjust_row_count_for_index_scan_by_limit(
            ctx,
            ds_stats,
            table_stats,
            ds.statistic_table.as_deref().expect("initStats"),
            path,
            prop.expected_cnt,
            is_match_prop && prop.sort_items[0].desc,
        )?;
    }
    // ScaleByExpectCnt only allows to scale the row count smaller than the table total row count.
    // But for MV index, it's possible that the IndexRangeScan row count is larger than the table total row count.
    if idx.mv_index && row_count > table_stats.row_count {
        is.set_stats(Some(Arc::new(
            table_stats.scale(row_count / table_stats.row_count),
        )));
    } else {
        is.set_stats(Some(table_stats.scale_by_expect_cnt(row_count)));
    }
    // usedStats := ...GetUsedStatsInfo(false): not carried.
    if prop.need_keep_order() {
        is.desc = prop.get_sort_desc_for_keep_order();
        is.keep_order = true;
    }
    Ok(is)
}

// go: isIndexColsCoveringCol L1
fn is_index_cols_covering_col(
    sctx: &dyn tidb_sessionctx::exprctx::EvalContext,
    col: &Column,
    index_cols: &[Option<Arc<Column>>],
    idx_col_lens: &[isize],
    ignore_len: bool,
) -> bool {
    for (i, index_col) in index_cols.iter().enumerate() {
        let Some(index_col) = index_col else { continue };
        if !col.equal_by_expr_and_id(sctx, index_col) {
            continue;
        }
        if ignore_len
            || idx_col_lens[i] == UnspecifiedLength
            || idx_col_lens[i] == col.ret_type.get_flen()
        {
            return true;
        }
    }
    false
}

// go: indexCoveringColumn L1 [M-CTX]
fn index_covering_column(
    ds: &DataSource,
    ctx: &PlanCtx<'_>,
    column: &Column,
    index_columns: &[Option<Arc<Column>>],
    idx_col_lens: &[isize],
    ignore_len: bool,
) -> bool {
    if ds.table_info.pk_is_handle && has_pri_key_flag(column.ret_type.get_flag()) {
        return true;
    }
    if column.id == ExtraHandleID as i64 || column.id == ExtraPhysTblID as i64 {
        return true;
    }
    let eval_ctx = ctx.eval_ctx();
    let covered_by_plain_index =
        is_index_cols_covering_col(&*eval_ctx, column, index_columns, idx_col_lens, ignore_len);
    let common: Vec<Option<Arc<Column>>> = ds
        .common_handle_cols
        .iter()
        .map(|c| Some(Arc::clone(c)))
        .collect();
    let covered_by_clustered_index = is_index_cols_covering_col(
        &*eval_ctx,
        column,
        &common,
        &ds.common_handle_lens,
        ignore_len,
    );
    if !covered_by_plain_index && !covered_by_clustered_index {
        return false;
    }
    let tp = &column.ret_type;
    let is_clustered_new_collation_idx = tidb_types::collate::new_collation_enabled()
        && tp.eval_type() == tidb_parser::types::ETString
        && !has_binary_flag(tp.get_flag());
    if !covered_by_plain_index
        && covered_by_clustered_index
        && is_clustered_new_collation_idx
        && ds.table_info.common_handle_version == 0
    {
        return false;
    }
    true
}

// go: isIndexCoveringColumns L1
pub(crate) fn is_index_covering_columns(
    ds: &DataSource,
    ctx: &PlanCtx<'_>,
    columns: &[Arc<Column>],
    index_columns: &[Option<Arc<Column>>],
    idx_col_lens: &[isize],
) -> bool {
    for col in columns {
        if !index_covering_column(ds, ctx, col, index_columns, idx_col_lens, false) {
            return false;
        }
    }
    true
}

// go: isIndexCoveringCondition L1
pub(crate) fn is_index_covering_condition(
    ds: &DataSource,
    ctx: &PlanCtx<'_>,
    condition: &Expr,
    index_columns: &[Option<Arc<Column>>],
    idx_col_lens: &[isize],
) -> bool {
    match condition {
        Expr::Column(v) => index_covering_column(ds, ctx, v, index_columns, idx_col_lens, false),
        Expr::ScalarFunction(v) => {
            // Even if the index only contains prefix `col`, the index can cover `col is null`.
            if v.func_name.l == tidb_expression::scalar_function::ast_func::IsNull
                && let Expr::Column(col) = &*v.get_args()[0]
            {
                return index_covering_column(ds, ctx, col, index_columns, idx_col_lens, true);
            }
            for arg in v.get_args() {
                if !is_index_covering_condition(ds, ctx, arg, index_columns, idx_col_lens) {
                    return false;
                }
            }
            true
        }
        _ => true,
    }
}

// go: splitIndexFilterConditions L1
pub(crate) fn split_index_filter_conditions(
    ds: &DataSource,
    ctx: &PlanCtx<'_>,
    conditions: &[ExprRef],
    index_columns: &[Option<Arc<Column>>],
    idx_col_lens: &[isize],
) -> (Vec<ExprRef>, Vec<ExprRef>) {
    let mut index_conditions = Vec::new();
    let mut table_conditions = Vec::new();
    for cond in conditions {
        let covered = if ctx.vars.opt_prefix_index_single_scan {
            is_index_covering_condition(ds, ctx, cond, index_columns, idx_col_lens)
        } else {
            is_index_covering_columns(
                ds,
                ctx,
                &tidb_expression::util::extract_columns(cond),
                index_columns,
                idx_col_lens,
            )
        };
        if covered {
            index_conditions.push(Arc::clone(cond));
        } else {
            table_conditions.push(Arc::clone(cond));
        }
    }
    (index_conditions, table_conditions)
}

/// `isSingleScan` (`DataSource.IsSingleScan`).
// go: isSingleScan L1 [M-CTX]
pub fn is_single_scan(
    ds: &DataSource,
    ctx: &PlanCtx<'_>,
    index_columns: &[Option<Arc<Column>>],
    idx_col_lens: &[isize],
) -> bool {
    // Go's `ds.ColsRequiringFullLen == nil` (not set by column pruning) is the flag
    // `cols_requiring_full_len_set`.
    if !ctx.vars.opt_prefix_index_single_scan || !ds.cols_requiring_full_len_set {
        // ds.ColsRequiringFullLen is set at (*DataSource).PruneColumns. In some cases we don't reach (*DataSource).PruneColumns
        // and ds.ColsRequiringFullLen is nil, so we fall back to ds.isIndexCoveringColumns(ds.schema.Columns, indexColumns, idxColLens).
        return is_index_covering_columns(
            ds,
            ctx,
            &ds.schema(&ctx.arena).columns,
            index_columns,
            idx_col_lens,
        );
    }
    if !is_index_covering_columns(
        ds,
        ctx,
        &ds.cols_requiring_full_len,
        index_columns,
        idx_col_lens,
    ) {
        return false;
    }
    for cond in &ds.all_conds {
        if !is_index_covering_condition(ds, ctx, cond, index_columns, idx_col_lens) {
            return false;
        }
    }
    true
}

/// SplitSelCondsWithVirtualColumn filter the select conditions which contain virtual column
// go: SplitSelCondsWithVirtualColumn L1
pub fn split_sel_conds_with_virtual_column(conds: &[ExprRef]) -> (Vec<ExprRef>, Vec<ExprRef>) {
    let mut without_virt = Vec::new();
    let mut with_virt = Vec::new();
    for i in 0..conds.len() {
        if tidb_expression::util::contain_virtual_column(&conds[i..i + 1]) {
            with_virt.push(Arc::clone(&conds[i]));
        } else {
            without_virt.push(Arc::clone(&conds[i]));
        }
    }
    (without_virt, with_virt)
}

// go-exception: EXC.B3 GetPhysicalScan4LogicalTableScan (LogicalTableScan is built only by the old cascades planner, exception B3)

// go: isPointGetPath L1 [M-CTX]
fn is_point_get_path(ds: &DataSource, ctx: &PlanCtx<'_>, path: &AccessPath) -> bool {
    let _ = ds;
    if path.ranges.is_empty() {
        return false;
    }
    if !path.is_int_handle_path {
        let Some(index) = &path.index else {
            return false;
        };
        if !index.unique || index.has_prefix_index() {
            return false;
        }
        let idx_cols_len = index.columns.as_deref().map_or(0, <[_]>::len);
        for ran in &path.ranges {
            if ran.low_val.len() != idx_cols_len {
                return false;
            }
        }
    }
    let tc = ctx.vars.stmt_ctx().type_ctx();
    for ran in &path.ranges {
        if !ran.is_point_non_nullable(&tc) {
            return false;
        }
    }
    true
}

/// convertToTableScan converts the DataSource to table scan. TiFlash table scans (the MPP
/// task, the batch cop of TiFlash) are outside the first slice: a boundary.
// go: convertToTableScan L2 (TiFlash and MPP table scans are a boundary of D10.1)
fn convert_to_table_scan(
    ds: &mut DataSource,
    ctx: &PlanCtx<'_>,
    prop: &PhysicalProperty,
    candidate: &CandidatePath,
    _opt: &PhysicalOptimizeOp,
) -> Result<Task, Error> {
    // It will be handled in convertToIndexScan.
    if prop.task_tp == CopMultiReadTaskType {
        return Ok(Task::invalid_task());
    }
    if !prop.is_sort_item_empty() && !candidate.match_prop_result.matched() {
        return Ok(Task::invalid_task());
    }
    // If we need to keep order for the index scan, we should forbid the non-keep-order index scan when we try to generate the path.
    if prop.is_sort_item_empty() && candidate.path.force_keep_order {
        return Ok(Task::invalid_task());
    }
    // If we don't need to keep order for the index scan, we should forbid the non-keep-order index scan when we try to generate the path.
    if !prop.is_sort_item_empty() && candidate.path.force_no_keep_order {
        return Ok(Task::invalid_task());
    }
    let (mut ts, _) = get_original_physical_table_scan(
        ds,
        ctx,
        prop,
        &candidate.path,
        candidate.match_prop_result.matched(),
    )?;
    if ts.keep_order && ts.store_type == kv::TiFlash && (ts.desc || ctx.vars.tiflash_fast_scan) {
        // TiFlash fast mode(https://github.com/pingcap/tidb/pull/35851) does not keep order in TableScan
        return Ok(Task::invalid_task());
    }

    // In disaggregated tiflash mode, only MPP is allowed, cop and batchCop is deprecated.
    // So if prop.TaskTp is RootTaskType, have to use mppTask then convert to rootTask.
    let is_tiflash_path = ts.store_type == kv::TiFlash;
    if prop.task_tp == MppTaskType || is_tiflash_path {
        // glue: blocked-by:D10.1 (TiFlash table scans: the MPP task and the TiFlash cop / batch cop reads, outside the first slice)
        return Err(blocked("D10.1", "a TiFlash table scan (MPP / batch cop)"));
    }
    let mut cop_task = CopTask {
        index_plan_finished: true,
        tbl_col_hists: ds.tbl_col_hists.clone(),
        ..Default::default()
    };
    let pruning_conds = push_down_not_conds(ctx, &mut ds.all_conds);
    let part_info = Arc::new(PhysPlanPartInfo {
        pruning_conds,
        partition_names: ds.partition_names.clone(),
        columns: ds.tbl_cols.clone(),
        column_names: ds.output_names(&ctx.arena).clone(),
    });
    cop_task.phys_plan_part_info = Some(Arc::clone(&part_info));
    ts.plan_part_info = Some(part_info);
    if candidate.match_prop_result.matched() {
        cop_task.keep_order = true;
        // ds.TableInfo.GetPartitionInfo() != nil: partitioned tables stop in the plan builder (D09.12).
        if candidate.match_prop_result == PropMatchedNeedMergeSort {
            // Add sort items for table scan for merge-sort operation between partitions.
            ts.by_items = prop
                .sort_items
                .iter()
                .map(|si| ByItems {
                    expr: Arc::new(Expr::Column(Column::clone(&si.col))),
                    desc: si.desc,
                })
                .collect();
            ts.grouped_ranges = candidate.path.grouped_ranges.clone();
            ts.group_by_col_idxs = candidate.path.group_by_col_idxs.clone();
        }
    }
    let stats = ds_stats_of(ds).scale_by_expect_cnt(prop.expected_cnt);
    add_pushed_down_selection(ts, ctx, &mut cop_task, stats)?;
    if prop.is_flash_prop() && !cop_task.root_task_conds.is_empty() {
        return Ok(Task::invalid_task());
    }
    let task = Task::Cop(cop_task);
    if prop.task_tp == RootTaskType {
        return task.convert_to_root_task(ctx);
    }
    Ok(task)
}

/// `ds.StatsInfo()`.
// glue: Go reads ds.StatsInfo() directly (set by DeriveStats)
fn ds_stats_of(ds: &DataSource) -> &Arc<StatsInfo> {
    ds.base
        .plan
        .stats_info()
        .expect("DeriveStats sets the DataSource's stats")
}

/// `convertToPointGet`.
// go: convertToPointGet L1 [M-CTX]
fn convert_to_point_get(
    ds: &DataSource,
    ctx: &PlanCtx<'_>,
    prop: &PhysicalProperty,
    candidate: &CandidatePath,
) -> Result<Task, Error> {
    if !prop.is_sort_item_empty() && !candidate.match_prop_result.matched() {
        return Ok(Task::invalid_task());
    }
    if prop.task_tp == CopMultiReadTaskType && candidate.path.is_single_scan
        || prop.task_tp == CopSingleReadTaskType && !candidate.path.is_single_scan
    {
        return Ok(Task::invalid_task());
    }

    if tidb_infoschema::metadef::is_mem_db(&ds.db_name.l) {
        return Ok(Task::invalid_task());
    }

    let access_cnt = candidate.path.count_after_access.min(1.0);
    let table_stats = ds.table_stats.as_ref().expect("initStats");
    let mut point_get_plan = PointGetPlan {
        access_conditions: candidate.path.access_conds.clone(),
        schema: ds.schema(&ctx.arena).clone_schema(),
        db_name: ds.db_name.l.clone(),
        tbl_info: Arc::clone(&ds.table_info),
        output_names: ds.output_names(&ctx.arena).clone(),
        lock_wait_time: ctx.vars.lock_wait_timeout,
        columns: ds.columns.clone(),
        ..Default::default()
    }
    .init(
        ctx,
        Some(table_stats.scale_by_expect_cnt(access_cnt)),
        ds.query_block_offset(),
    );
    if let Some(idx) = ds.partition_def_idx {
        point_get_plan.partition_idx = Some(idx as isize);
    }
    point_get_plan.partition_names = ds.partition_names.clone();
    let mut sel_conds: Option<Vec<ExprRef>> = None;
    if candidate.path.is_int_handle_path {
        point_get_plan.handle = Some(Handle::Int(IntHandle(
            candidate.path.ranges[0].low_val[0].get_int64(),
        )));
        let handle_col = Arc::clone(
            ds.handle_cols
                .as_ref()
                .and_then(|h| h.get_col(0))
                .expect("the int handle column"),
        );
        point_get_plan.unsigned_handle = has_unsigned_flag(handle_col.ret_type.get_flag());
        point_get_plan.access_cols = ds.tbl_cols.clone();
        let mut found = false;
        for c in &ds.columns {
            if c.id == handle_col.id {
                point_get_plan.handle_col_offset = c.offset as usize;
                found = true;
                break;
            }
        }
        if !found {
            return Ok(Task::invalid_task());
        }
        // Add filter condition to table plan now.
        if !candidate.path.table_filters.is_empty() {
            sel_conds = Some(candidate.path.table_filters.clone());
        }
    } else {
        point_get_plan.index_info = candidate.path.index.clone();
        point_get_plan.idx_cols = candidate.path.idx_cols.clone();
        point_get_plan.idx_col_lens = candidate.path.idx_col_lens.clone();
        point_get_plan.index_values = candidate.path.ranges[0].low_val.clone();
        if candidate.path.is_single_scan {
            point_get_plan.access_cols = candidate.path.idx_cols.clone();
        } else {
            point_get_plan.access_cols = ds.tbl_cols.clone();
        }
        // Add index condition to table plan now.
        if candidate.path.index_filters.len() + candidate.path.table_filters.len() > 0 {
            let mut conds = candidate.path.index_filters.clone();
            conds.extend(candidate.path.table_filters.iter().map(Arc::clone));
            sel_conds = Some(conds);
        }
    }
    let point_get: PhysicalPlanRef = Arc::new(point_get_plan);
    let mut r_tsk = RootTask::default();
    match sel_conds {
        None => r_tsk.set_plan(point_get),
        Some(conditions) => {
            let mut sel = PhysicalSelection {
                conditions,
                ..Default::default()
            }
            .init(
                ctx,
                Some(ds_stats_of(ds).scale_by_expect_cnt(prop.expected_cnt)),
                ds.query_block_offset(),
                Vec::new(),
            );
            sel.set_children(vec![point_get]);
            r_tsk.set_plan(Arc::new(sel));
        }
    }
    Ok(Task::Root(r_tsk))
}

impl PhysicalTableScan {
    // go: PhysicalTableScan.getScanRowSize L1 [M-CTX]
    pub fn get_scan_row_size(&self, ctx: &PlanCtx<'_>) -> f64 {
        let coll = self.tbl_col_hists.as_deref().cloned().unwrap_or_default();
        if self.store_type == kv::TiKV {
            return crate::cardinality::row_size::get_table_avg_row_size(
                ctx,
                &coll,
                &self.tbl_cols,
                self.store_type,
                true,
            );
        }
        // If `ts.handleCol` is nil, then the schema of tableScan doesn't have handle column.
        // This logic can be ensured in column pruning.
        crate::cardinality::row_size::get_table_avg_row_size(
            ctx,
            &coll,
            &self.schema().columns,
            self.store_type,
            self.handle_cols.is_some(),
        )
    }
}

/// `addPushedDownSelection`: the table scan (Go's receiver) goes into the cop task, under a
/// selection of the conditions that can be pushed down.
// go: PhysicalTableScan.addPushedDownSelection L1 [M-PLAN, M-CTX]
fn add_pushed_down_selection(
    mut ts: PhysicalTableScan,
    ctx: &PlanCtx<'_>,
    cop_task: &mut CopTask,
    stats: Arc<StatsInfo>,
) -> Result<(), Error> {
    (ts.filter_condition, cop_task.root_task_conds) =
        split_sel_conds_with_virtual_column(&ts.filter_condition);
    let new_root_conds;
    (ts.filter_condition, new_root_conds) =
        push_down_exprs(&ctx.push_down_ctx()?, &ts.filter_condition, ts.store_type);
    cop_task.root_task_conds.extend(new_root_conds);

    // Add filter condition to table plan now.
    if !ts.filter_condition.is_empty() {
        let conditions = ts.filter_condition.clone();
        let mut sel = PhysicalSelection {
            conditions: conditions.clone(),
            ..Default::default()
        }
        .init(ctx, Some(stats), ts.query_block_offset(), Vec::new());
        if !cop_task.root_task_conds.is_empty() {
            let coll = cop_task
                .tbl_col_hists
                .as_deref()
                .cloned()
                .unwrap_or_default();
            let selectivity =
                match crate::cardinality::selectivity::selectivity(ctx, &coll, &conditions, &[]) {
                    Ok((s, _)) => s,
                    Err(e) if tidb_base::blocked::blocked_by(&e).is_some() => return Err(e),
                    // logutil.BgLogger().Debug("calculate selectivity failed, use selection factor")
                    Err(_) => SelectionFactor,
                };
            let ts_stats = ts.stats_info().expect("the scan's stats");
            sel.set_stats(Some(Arc::new(ts_stats.scale(selectivity))));
        }
        sel.set_children(vec![Arc::new(ts)]);
        cop_task.table_plan = Some(Arc::new(sel));
    } else {
        cop_task.table_plan = Some(Arc::new(ts));
    }
    Ok(())
}

/// `getOriginalPhysicalTableScan`. The used statistics record (`usedStatsInfo`) is not
/// carried (stats.rs module doc); the vector property stops in `matchProperty`. Returns a
/// `Result` for the boundary of the row count under a limit.
// go: getOriginalPhysicalTableScan L2 (Result for the boundary of AdjustRowCountForTableScanByLimit)
fn get_original_physical_table_scan(
    ds: &DataSource,
    ctx: &PlanCtx<'_>,
    prop: &PhysicalProperty,
    path: &AccessPath,
    is_match_prop: bool,
) -> Result<(PhysicalTableScan, f64), Error> {
    let mut ts = PhysicalTableScan {
        table: Arc::clone(&ds.table_info),
        columns: ds.columns.clone(),
        table_as_name: ds.table_as_name.clone(),
        db_name: ds.db_name.clone(),
        is_partition: ds.partition_def_idx.is_some(),
        physical_table_id: ds.physical_table_id,
        ranges: path.ranges.clone(),
        access_condition: path.access_conds.clone(),
        store_type: path.store_type,
        handle_cols: ds.handle_cols.clone(),
        tbl_cols: ds.tbl_cols.clone(),
        tbl_col_hists: ds.tbl_col_hists.clone(),
        const_cols_by_cond: path.const_cols.clone(),
        prop: Some(Arc::new(prop.clone())),
        filter_condition: path.table_filters.clone(),
        ..Default::default()
    }
    .init(ctx, ds.query_block_offset());
    ts.set_schema(ds.schema(&ctx.arena).clone_schema());
    let mut row_count = path.count_after_access;
    let ds_stats = ds_stats_of(ds);
    let table_stats = ds.table_stats.as_ref().expect("initStats");
    // Add an arbitrary tolerance factor to account for comparison with floating point
    if (prop.expected_cnt + ToleranceFactor) < ds_stats.row_count {
        row_count = crate::cardinality::cross_estimation::adjust_row_count_for_table_scan_by_limit(
            ctx,
            ds_stats,
            table_stats,
            ds.statistic_table.as_deref().expect("initStats"),
            path,
            prop.expected_cnt,
            is_match_prop && prop.sort_items[0].desc,
        )?;
    }
    // We need NDV of columns since it may be used in cost estimation of join. Precisely speaking,
    // we should track NDV of each histogram bucket, and sum up the NDV of buckets we actually need
    // to scan, but this would only help improve accuracy of NDV for one column, for other columns,
    // we still need to assume values are uniformly distributed. For simplicity, we use uniform-assumption
    // for all columns now, as we do in `deriveStatsByFilter`.
    ts.set_stats(Some(table_stats.scale_by_expect_cnt(row_count)));
    // usedStats := ...GetUsedStatsInfo(false): not carried.
    if is_match_prop && prop.vector_prop.vector_helper.is_none() {
        ts.desc = prop.sort_items[0].desc;
        ts.keep_order = true;
    }
    Ok((ts, row_count))
}

// go: appendCandidate L1
fn append_candidate(task: Option<&Task>, prop: &PhysicalProperty, opt: &PhysicalOptimizeOp) {
    let Some(task) = task else { return };
    if task.invalid() {
        return;
    }
    append_candidate4_physical_optimize_op(opt, task.plan(), prop);
}

/// PushDownNot here can convert condition 'not (a != 1)' to 'a = 1'. When we build range from
/// conds, the condition like 'not (a != 1)' would not be handled so we need to convert it to
/// 'a = 1', which can be handled when building range. Go writes the result into the given
/// slice (here `ds.AllConds`, through its shared backing array) and returns it.
// go: pushDownNot L1 [M-CTX]
fn push_down_not_conds(ctx: &PlanCtx<'_>, conds: &mut [ExprRef]) -> Vec<ExprRef> {
    for cond in conds.iter_mut() {
        *cond = tidb_expression::util::push_down_not(&*ctx.expr_ctx, cond)
            .expect("runtime error: invalid memory address or nil pointer dereference");
    }
    conds.to_vec()
}
