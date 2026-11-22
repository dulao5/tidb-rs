//! go-file: pkg/planner/core/plan.go
//!
//! `AsSctx` (a boundary stub), `enforceProperty` and the probe-count helpers `BasePhysicalPlan` calls (Go reaches them through
//! `utilfuncp` function pointers to break an import cycle; here they are called directly).
//!
//! Only `PhysicalApply`, `PhysicalIndexJoin`, `PhysicalIndexHashJoin` and
//! `PhysicalIndexMergeJoin` count as probe parents. Those operators are the join part of
//! D10.1 and have no [`crate::PhysicalKind`] yet, so every probe parent falls into Go's
//! default branch (no factor), as Go does for any other operator. The join arms (outer child
//! `pp.Children()[1-getInnerChildIdx()]`) are added with those operators; until then nothing
//! sets probe parents (`propagateProbeParents`, VS1-24).

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_execdetails::execdetails::RuntimeStatsColl;
use tidb_expression::Expr;

use crate::core::physical_plans::PhysicalSort;
use crate::core::task::attach2_task;
use crate::core::task_base::Task;
use crate::physicalop::PhysicalPlanRef;
use crate::planctx::PlanCtx;
use crate::property::{MppTaskType, PhysicalProperty, RootTaskType};
use crate::util::ByItems;

/// AsSctx converts PlanContext to sessionctx.Context.
///
/// The planner gets `PlanCtx` (N2), never the session; the callers that need the session
/// (plan replayer, `EXPLAIN ANALYZE`, `IMPORT INTO`) come with D05.3 / D12.12.
// glue: stub of AsSctx, blocked-by:D05.3 (sessionctx.Context)
pub fn as_sctx() -> Result<(), Error> {
    Err(blocked("D05.3", "AsSctx (sessionctx.Context)"))
}

/// `enforceProperty` adds a `PhysicalSort` (and MPP exchangers) on top of a task to give it
/// the property `p`.
///
/// The MPP exchanger (`MppTask.enforceExchanger`) is outside the first vertical slice: a
/// boundary stub on that branch.
// go: enforceProperty L2 (the MPP exchanger is blocked-by:D10.1)
pub fn enforce_property(
    p: &PhysicalProperty,
    mut tsk: Task,
    ctx: &PlanCtx<'_>,
) -> Result<Task, Error> {
    if p.task_tp == MppTaskType {
        let Task::Mpp(mpp) = &tsk else {
            return Ok(Task::invalid_task());
        };
        if mpp.invalid() {
            return Ok(Task::invalid_task());
        }
        if !p.is_sort_item_all_for_partition() {
            ctx.vars.raise_warning_when_mpp_enforced(
                "MPP mode may be blocked because operator `Sort` is not supported now.",
            );
            return Ok(Task::invalid_task());
        }
        // glue: blocked-by:D10.1 (MppTask.enforceExchanger; MPP is outside the first slice)
        return Err(blocked(
            "D10.1",
            "enforceProperty of an MPP task (enforceExchanger)",
        ));
    }
    // when task is double cop task warping a index merge reader, tsk.plan() may be nil when indexPlanFinished is marked
    // as false, while the real plan is in idxMergePartPlans. tsk.plan()==nil is not right here.
    if p.is_sort_item_empty() || tsk.invalid() {
        return Ok(tsk);
    }
    if p.task_tp != MppTaskType {
        tsk = tsk.convert_to_root_task(ctx)?;
    }
    let sort_req_prop = PhysicalProperty {
        task_tp: RootTaskType,
        sort_items: p.sort_items.clone(),
        expected_cnt: f64::MAX,
        ..Default::default()
    };
    let plan = tsk
        .plan()
        .expect("runtime error: invalid memory address or nil pointer dereference");
    let mut sort = PhysicalSort {
        by_items: Vec::with_capacity(p.sort_items.len()),
        is_partial_sort: p.is_sort_item_all_for_partition(),
        ..Default::default()
    }
    .init(
        ctx,
        plan.stats_info().map(Arc::clone),
        plan.query_block_offset(),
        vec![Some(Arc::new(sort_req_prop))],
    );
    for col in &p.sort_items {
        sort.by_items.push(ByItems {
            expr: Arc::new(Expr::Column((*col.col).clone())),
            desc: col.desc,
        });
    }
    attach2_task(ctx, Arc::new(sort), &[tsk])
}

// go: getEstimatedProbeCntFromProbeParents L1 [M-IFACE]
pub fn get_estimated_probe_cnt_from_probe_parents(probe_parents: &[PhysicalPlanRef]) -> f64 {
    let res = 1.0f64;
    for pp in probe_parents {
        // `case *PhysicalApply, *PhysicalIndexHashJoin, *PhysicalIndexMergeJoin,
        // *PhysicalIndexJoin`: res *= outer.StatsInfo().RowCount (see the module doc).
        // glue: blocked-by:D10.1 (the join arms come with the join operators)
        let _ = pp.kind();
    }
    res
}

// go: getActualProbeCntFromProbeParents L1 [M-IFACE]
pub fn get_actual_probe_cnt_from_probe_parents(
    pps: &[PhysicalPlanRef],
    stats_coll: &RuntimeStatsColl,
) -> i64 {
    let res = 1i64;
    for pp in pps {
        // `case *PhysicalApply, ...`: res *= the actual rows of the outer child from
        // stats_coll (see the module doc).
        // glue: blocked-by:D10.1 (the join arms come with the join operators)
        let _ = (pp.kind(), stats_coll);
    }
    res
}

/// `optimizeByShuffle` inserts a `PhysicalShuffle` to optimize performance by running in a
/// parallel manner. Only `PhysicalWindow`, `PhysicalMergeJoin` and `PhysicalStreamAgg` get a
/// shuffle; none of them is ported (windows D09.4, joins D09.3, aggregation D09.4), so the
/// task is returned as is.
// go: optimizeByShuffle L2 (the window / merge join / stream aggregation arms come with those operators)
pub fn optimize_by_shuffle(tsk: Task, ctx: &PlanCtx<'_>) -> Task {
    let _ = ctx;
    // switch p := tsk.Plan().(type): no ported operator gets a shuffle.
    tsk
}
