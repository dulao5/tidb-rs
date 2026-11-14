//! go-file: pkg/planner/core/initialize.go
//!
//! The `Init` functions of the VS1 physical operators and the plan flattening helpers. The
//! operator types are D10.1 (VS1-27: `PhysicalTableReader`, `PhysicalTableScan`,
//! `PhysicalSelection`, `PhysicalProjection`, `PhysicalLimit`, `PhysicalTopN`,
//! `PhysicalSort`, `PhysicalTableDual`) and D10.8 (VS1-11: `PointGetPlan`,
//! `BatchPointGetPlan`); each `Init` is ported with its type, until then a boundary stub.

use tidb_base::Error;
use tidb_base::blocked::blocked;

/// The `Init` of a D10.1 operator.
// glue: stub of PhysicalTableReader.Init, blocked-by:D10.1 (VS1-27: PhysicalTableReader)
// glue: stub of PhysicalTableReader.adjustReadReqType, blocked-by:D10.1 (VS1-27: PhysicalTableReader)
// glue: stub of PhysicalTableScan.Init, blocked-by:D10.1 (VS1-27: PhysicalTableScan)
// glue: stub of PhysicalSelection.Init, blocked-by:D10.1 (VS1-27: PhysicalSelection)
// glue: stub of PhysicalProjection.Init, blocked-by:D10.1 (VS1-27: PhysicalProjection)
// glue: stub of PhysicalLimit.Init, blocked-by:D10.1 (VS1-27: PhysicalLimit)
// glue: stub of PhysicalTopN.Init, blocked-by:D10.1 (VS1-27: PhysicalTopN)
// glue: stub of PhysicalSort.Init, blocked-by:D10.1 (VS1-27: PhysicalSort)
// glue: stub of PhysicalTableDual.Init, blocked-by:D10.1 (VS1-27: PhysicalTableDual)
// glue: stub of flattenTreePushDownPlan, blocked-by:D10.1 (VS1-27: PhysicalTableReader)
// glue: stub of flattenListPushDownPlan, blocked-by:D10.1 (VS1-27: PhysicalTableReader)
// glue: stub of flattenPlanWithPreorderTraversal, blocked-by:D10.1 (VS1-27: PhysicalTableReader)
// glue: stub of flattenPlanWithPostorderTraversal, blocked-by:D10.1 (VS1-27: PhysicalTableReader)
pub fn init_physical_operator() -> Result<(), Error> {
    Err(blocked("D10.1", "the Init of a physical operator"))
}

/// The `Init` of the point-get plans.
// glue: stub of PointGetPlan.Init, blocked-by:D10.8 (VS1-11: PointGetPlan)
// glue: stub of BatchPointGetPlan.Init, blocked-by:D10.8 (BatchPointGetPlan)
pub fn init_point_get_plan() -> Result<(), Error> {
    Err(blocked("D10.8", "the Init of a point-get plan"))
}
