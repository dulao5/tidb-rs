//! go-file: pkg/planner/core/operator/logicalop/base_logical_plan.go
//! go-file: pkg/planner/core/operator/logicalop/logical_datasource.go
//! go-file: pkg/planner/core/operator/logicalop/logical_limit.go
//! go-file: pkg/planner/core/operator/logicalop/logical_projection.go
//! go-file: pkg/planner/core/operator/logicalop/logical_selection.go
//! go-file: pkg/planner/core/operator/logicalop/logical_table_dual.go
//! go-file: pkg/planner/core/operator/logicalop/logical_top_n.go
//!
//! The statistics derivation of the logical operators (block D10.6, listed under VS1-29).
//! It needs the operators and their `StatsInfo` slot, which come with D09.14 (VS1-21 /
//! VS1-23); until then these are boundary stubs, and the functions move to the operators'
//! files when they are ported (specs/D10.6-cardinality §1.2).
// glue: stub of BaseLogicalPlan.DeriveStats, blocked-by:D09.14 (VS1-23: logical operators carry their StatsInfo)
// glue: stub of BaseLogicalPlan.ExtractColGroups, blocked-by:D09.14 (VS1-23: logical operators)
// glue: stub of DataSource.DeriveStats, blocked-by:D09.14 (VS1-21 / VS1-23: DataSource)
// glue: stub of LogicalLimit.DeriveStats, blocked-by:D09.14 (VS1-23: LogicalLimit)
// glue: stub of LogicalProjection.DeriveStats, blocked-by:D09.14 (VS1-23: LogicalProjection)
// glue: stub of LogicalProjection.ExtractColGroups, blocked-by:D09.14 (VS1-23: LogicalProjection)
// glue: stub of LogicalSelection.DeriveStats, blocked-by:D09.14 (VS1-23: LogicalSelection)
// glue: stub of LogicalTableDual.DeriveStats, blocked-by:D09.14 (VS1-23: LogicalTableDual)
// glue: stub of LogicalTopN.DeriveStats, blocked-by:D09.14 (VS1-23: LogicalTopN)

/// `RecursiveDeriveStats`: the statistics derivation of the logical plan, the first step
/// of the physical optimization (`physicalOptimize`). The logical operators are ported
/// (D09.14); the derivation itself is the cardinality block (D10.6, the pseudo statistics
/// task of the first vertical slice).
// glue: stub of BaseLogicalPlan.RecursiveDeriveStats, blocked-by:D10.6 (the statistics derivation of the logical operators, called first by physicalOptimize)
pub fn recursive_derive_stats(
    ctx: &mut crate::PlanCtx<'_>,
    p: crate::PlanId,
) -> Result<(), tidb_base::Error> {
    let _ = (ctx, p);
    Err(tidb_base::blocked::blocked(
        "D10.6",
        "RecursiveDeriveStats (the statistics derivation)",
    ))
}
