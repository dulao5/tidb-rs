//! go-file: pkg/planner/core/plan.go
//!
//! `AsSctx`, `enforceProperty` (boundary stubs) and the probe-count helpers `BasePhysicalPlan` calls (Go reaches them through
//! `utilfuncp` function pointers to break an import cycle; here they are called directly).
//!
//! Only `PhysicalApply`, `PhysicalIndexJoin`, `PhysicalIndexHashJoin` and
//! `PhysicalIndexMergeJoin` count as probe parents. Those operators are the join part of
//! D10.1 and have no [`crate::PhysicalKind`] yet, so every probe parent falls into Go's
//! default branch (no factor), as Go does for any other operator. The join arms (outer child
//! `pp.Children()[1-getInnerChildIdx()]`) are added with those operators; until then nothing
//! sets probe parents (`propagateProbeParents`, VS1-24).

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_execdetails::execdetails::RuntimeStatsColl;

use crate::physicalop::PhysicalPlanRef;

/// AsSctx converts PlanContext to sessionctx.Context.
///
/// The planner gets `PlanCtx` (N2), never the session; the callers that need the session
/// (plan replayer, `EXPLAIN ANALYZE`, `IMPORT INTO`) come with D05.3 / D12.12.
// glue: stub of AsSctx, blocked-by:D05.3 (sessionctx.Context)
pub fn as_sctx() -> Result<(), Error> {
    Err(blocked("D05.3", "AsSctx (sessionctx.Context)"))
}

/// `enforceProperty` adds a `PhysicalSort` (and MPP exchangers) on top of a task.
// glue: stub of enforceProperty, blocked-by:D10.1 (VS1-27: base.Task, PhysicalSort)
pub fn enforce_property() -> Result<(), Error> {
    Err(blocked(
        "D10.1",
        "enforceProperty (base.Task, PhysicalSort)",
    ))
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
