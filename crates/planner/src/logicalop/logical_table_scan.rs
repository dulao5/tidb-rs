//! go-file: pkg/planner/core/operator/logicalop/logical_table_scan.go
//! go-file: pkg/planner/core/operator/logicalop/logical_index_scan.go
//! go-file: pkg/planner/core/operator/logicalop/logical_tikv_single_gather.go
//!
//! `LogicalTableScan`, `LogicalIndexScan` and `TiKVSingleGather` are built only by the old
//! cascades planner (`DataSource.Convert2Gathers`, `transformation_rules.go`): exception B3
//! (not ported).

// go-exception: EXC.B3 LogicalTableScan.Init (only the old cascades planner builds it, exception B3)
// go-exception: EXC.B3 LogicalTableScan.ExplainInfo (only the old cascades planner builds it, exception B3)
// go-exception: EXC.B3 LogicalTableScan.BuildKeyInfo (only the old cascades planner builds it, exception B3)

// go-exception: EXC.B3 LogicalTableScan.PreparePossibleProperties (only the old cascades planner builds it, exception B3)
// go-exception: EXC.B3 LogicalIndexScan.PreparePossibleProperties (only the old cascades planner builds it, exception B3)
// go-exception: EXC.B3 TiKVSingleGather.PreparePossibleProperties (only the old cascades planner builds it, exception B3)
