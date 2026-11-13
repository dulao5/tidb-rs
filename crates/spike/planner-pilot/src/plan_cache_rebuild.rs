//! go-file: pkg/planner/core/plan_cache_rebuild.go (spike: the shape of `rebuildRange`)
//!
//! With the instance-level plan cache, Go clones the whole cached plan for every hit
//! (`PlanCacheValue.CloneForInstancePlanCache`, a deep copy including expressions) and then
//! rewrites the ranges of the clone in place. Here the cached plan is shared immutably; the
//! rebuild copies only the nodes on the paths from the root to the rewritten scans
//! ([`make_mut_plan`]), everything else stays shared with the cache (N1 §3.5, pilot Q4).
//! The range computation itself (ranger, D10.4) is a parameter.

use std::sync::Arc;

use tidb_base::Error;

use crate::physicalop::{
    PhysicalKind, PhysicalPlanRef, PhysicalTableReader, PhysicalTableScan, Range, make_mut_plan,
    map_children,
};

/// `buildRangeForTableScan` stand-in: computes the ranges of `ts` for the current parameters.
pub type BuildTableRange<'a> = dyn Fn(&PhysicalTableScan) -> Result<Vec<Range>, Error> + 'a;

/// rebuildRange rebuilds the ranges of the cached plan under the current parameters. `p`
/// is a slot: when something below changes, the node in the slot is replaced by a copy
/// (or changed in place if this statement owns it alone).
// go: pkg/planner/core/plan_cache_rebuild.go rebuildRange L2 (path copying instead of rewriting a deep clone; ranger is a parameter, blocked-by:D10.4)
pub fn rebuild_range(p: &mut PhysicalPlanRef, build: &BuildTableRange<'_>) -> Result<(), Error> {
    match p.kind() {
        PhysicalKind::TableScan => {
            let ts = p
                .as_any()
                .downcast_ref::<PhysicalTableScan>()
                .expect("table scan");
            let ranges = build(ts)?;
            let x = make_mut_plan(p)
                .as_any_mut()
                .downcast_mut::<PhysicalTableScan>()
                .expect("table scan");
            x.ranges = ranges;
        }
        PhysicalKind::TableReader => {
            let x = p
                .as_any()
                .downcast_ref::<PhysicalTableReader>()
                .expect("table reader");
            let mut table_plan = x.table_plans[0].clone();
            rebuild_range(&mut table_plan, build)?;
            if !Arc::ptr_eq(&table_plan, &x.table_plans[0]) {
                let x = make_mut_plan(p)
                    .as_any_mut()
                    .downcast_mut::<PhysicalTableReader>()
                    .expect("table reader");
                x.table_plans[0] = table_plan;
            }
        }
        // case base.PhysicalPlan: for _, child := range x.Children() { rebuildRange(child) }
        _ => {
            let mut err = None;
            map_children(p, |mut child| {
                if err.is_none()
                    && let Err(e) = rebuild_range(&mut child, build)
                {
                    err = Some(e);
                }
                child
            });
            if let Some(e) = err {
                return Err(e);
            }
        }
    }
    Ok(())
}
