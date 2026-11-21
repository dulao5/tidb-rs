//! go-file: pkg/planner/core/operator/logicalop/logical_table_scan.go
//!
//! `LogicalTableScan` is built only by the old cascades planner
//! (`DataSource.Convert2Gathers`, `transformation_rules.go`): exception B3 (not ported).

// go-exception: EXC.B3 LogicalTableScan.Init (only the old cascades planner builds it, exception B3)
// go-exception: EXC.B3 LogicalTableScan.ExplainInfo (only the old cascades planner builds it, exception B3)
// go-exception: EXC.B3 LogicalTableScan.BuildKeyInfo (only the old cascades planner builds it, exception B3)
