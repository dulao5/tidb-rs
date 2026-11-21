//! go-file: pkg/planner/core/explain.go
//!
//! The EXPLAIN text of the physical operators (`ExplainInfo`, `ExplainNormalizedInfo`,
//! `OperatorInfo`, the table scan's `TP` / `ExplainID`) is block D10.9 (the `ResolveIndices`
//! / `ToPB` / access object task of the first vertical slice). Until then the operators'
//! methods are boundary stubs that panic with the `blocked` error, like
//! `PointGetPlan.ExplainInfo` (the trait methods return a `String`).

use tidb_base::blocked::blocked;

/// The EXPLAIN text of an operator, a boundary stub.
// glue: stub of PhysicalTableReader.ExplainInfo, blocked-by:D10.9 (the EXPLAIN text and access objects of the operators)
// glue: stub of PhysicalTableScan.ExplainInfo, blocked-by:D10.9 (the EXPLAIN text and access objects of the operators)
// glue: stub of PhysicalTableScan.ExplainNormalizedInfo, blocked-by:D10.9 (the EXPLAIN text and access objects of the operators)
// glue: stub of PhysicalProjection.ExplainInfo, blocked-by:D10.9 (the EXPLAIN text and access objects of the operators)
// glue: stub of PhysicalProjection.ExplainNormalizedInfo, blocked-by:D10.9 (the EXPLAIN text and access objects of the operators)
// glue: stub of PhysicalTopN.ExplainInfo, blocked-by:D10.9 (the EXPLAIN text and access objects of the operators)
// glue: stub of PhysicalTopN.ExplainNormalizedInfo, blocked-by:D10.9 (the EXPLAIN text and access objects of the operators)
// glue: stub of PhysicalLimit.ExplainInfo, blocked-by:D10.9 (the EXPLAIN text and access objects of the operators)
// glue: stub of PhysicalSort.ExplainInfo, blocked-by:D10.9 (the EXPLAIN text and access objects of the operators)
// glue: stub of PhysicalSelection.ExplainInfo, blocked-by:D10.9 (the EXPLAIN text and access objects of the operators)
// glue: stub of PhysicalSelection.ExplainNormalizedInfo, blocked-by:D10.9 (the EXPLAIN text and access objects of the operators)
// glue: stub of PhysicalTableDual.ExplainInfo, blocked-by:D10.9 (the EXPLAIN text and access objects of the operators)
pub fn explain_stub(what: &str) -> ! {
    panic!("{}", blocked("D10.9", what));
}
