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

/// TP overrides the TP in order to match different range. The memory and cluster tables of
/// `information_schema` / `performance_schema` (`TypeMemTableScan`) stop in the plan builder
/// (D10.11), so a scan never has their names.
// go: PhysicalTableScan.TP L2 (memory table scans stop in the plan builder, D10.11)
pub fn table_scan_tp(p: &crate::core::physical_plans::PhysicalTableScan) -> &'static str {
    // infoschema.IsClusterTableByName(p.DBName.O, p.Table.Name.O): never (doc).
    if p.is_child_of_index_look_up {
        return "TableRowIDScan";
    } else if is_full_scan(p) {
        return "TableFullScan";
    }
    "TableRangeScan"
}

// go: PhysicalTableScan.haveCorCol L1
fn have_cor_col(p: &crate::core::physical_plans::PhysicalTableScan) -> bool {
    for cond in &p.access_condition {
        if !tidb_expression::util::extract_cor_columns(cond).is_empty() {
            return true;
        }
    }
    false
}

// go: PhysicalTableScan.isFullScan L1
fn is_full_scan(p: &crate::core::physical_plans::PhysicalTableScan) -> bool {
    if !p.range_info.is_empty() || have_cor_col(p) {
        return false;
    }
    let mut unsigned_int_handle = false;
    if p.table.pk_is_handle
        && let Some(pk_col_info) = p.table.get_pk_col_info()
    {
        unsigned_int_handle = tidb_base::mysql::has_unsigned_flag(pk_col_info.get_flag());
    }
    for ran in &p.ranges {
        if !ran.is_full_range(unsigned_int_handle) {
            return false;
        }
    }
    true
}
