//! go-file: pkg/planner/core/plan.go
//!
//! So far the probe-count helpers `BasePhysicalPlan` calls (Go reaches them through
//! `utilfuncp` function pointers to break an import cycle; here they are called directly).
//!
//! Only `PhysicalApply`, `PhysicalIndexJoin`, `PhysicalIndexHashJoin` and
//! `PhysicalIndexMergeJoin` count as probe parents. Those operators are the join part of
//! D10.1 and have no [`crate::PhysicalKind`] yet, so every probe parent falls into Go's
//! default branch (no factor), as Go does for any other operator. The join arms (outer child
//! `pp.Children()[1-getInnerChildIdx()]`) are added with those operators; until then nothing
//! sets probe parents (`propagateProbeParents`, VS1-24).

use tidb_execdetails::execdetails::RuntimeStatsColl;

use crate::physicalop::PhysicalPlanRef;

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
