//! go-file: pkg/planner/core/explain.go
//!
//! The EXPLAIN text of the physical operators (`ExplainInfo`, `ExplainNormalizedInfo`,
//! `OperatorInfo`) is block D10.9 (the EXPLAIN output task of the first vertical slice).
//! Until then the operators' methods are boundary stubs that panic with the `blocked` error,
//! like `PointGetPlan.ExplainInfo` (the trait methods return a `String`). The table scan's
//! `TP` and `ExplainID` are ported: the executor ids of `ToPB` and the optimizer trace use
//! them.

use tidb_base::blocked::blocked;

/// The EXPLAIN text of an operator, a boundary stub.
// glue: stub of PhysicalTableReader.ExplainInfo, blocked-by:D10.9 (the EXPLAIN text and access objects of the operators)
// glue: stub of PhysicalTableScan.ExplainInfo, blocked-by:D10.9 (the EXPLAIN text and access objects of the operators)
// glue: stub of PhysicalTableScan.ExplainNormalizedInfo, blocked-by:D10.9 (the EXPLAIN text and access objects of the operators)
// glue: stub of PhysicalIndexReader.ExplainInfo, blocked-by:D10.9 (the EXPLAIN text and access objects of the operators)
// glue: stub of PhysicalIndexReader.ExplainNormalizedInfo, blocked-by:D10.9 (the EXPLAIN text and access objects of the operators)
// glue: stub of PhysicalIndexLookUpReader.ExplainInfo, blocked-by:D10.9 (the EXPLAIN text and access objects of the operators)
// glue: stub of PhysicalIndexScan.ExplainInfo, blocked-by:D10.9 (the EXPLAIN text and access objects of the operators)
// glue: stub of PhysicalIndexScan.ExplainNormalizedInfo, blocked-by:D10.9 (the EXPLAIN text and access objects of the operators)
// glue: stub of PhysicalIndexScan.OperatorInfo, blocked-by:D10.9 (the EXPLAIN text and access objects of the operators)
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

/// ExplainID overrides the ExplainID in order to match different range.
///
/// `sc` is the statement context of Go's stored `ctx` (`M-CTX`); `None` is Go's nil context.
// go: PhysicalIndexScan.ExplainID L1 [M-CTX]
pub fn index_scan_explain_id(
    p: &crate::core::physical_plans::PhysicalIndexScan,
    sc: Option<&tidb_sessionctx::stmtctx::StatementContext>,
) -> String {
    if let Some(sc) = sc
        && sc.ignore_explain_id_suffix
    {
        return index_scan_tp(p).to_owned();
    }
    format!("{}_{}", index_scan_tp(p), crate::PhysicalPlan::id(p))
}

/// TP overrides the TP in order to match different range.
// go: PhysicalIndexScan.TP L1
pub fn index_scan_tp(p: &crate::core::physical_plans::PhysicalIndexScan) -> &'static str {
    if index_scan_is_full_scan(p) {
        return "IndexFullScan";
    }
    "IndexRangeScan"
}

// go: PhysicalIndexScan.haveCorCol L1
pub(crate) fn index_scan_have_cor_col(p: &crate::core::physical_plans::PhysicalIndexScan) -> bool {
    for cond in &p.access_condition {
        if !tidb_expression::util::extract_cor_columns(cond).is_empty() {
            return true;
        }
    }
    false
}

// go: PhysicalIndexScan.isFullScan L1
pub(crate) fn index_scan_is_full_scan(p: &crate::core::physical_plans::PhysicalIndexScan) -> bool {
    if !p.range_info.is_empty() || index_scan_have_cor_col(p) {
        return false;
    }
    for ran in &p.ranges {
        if !ran.is_full_range(false) {
            return false;
        }
    }
    true
}

/// ExplainID overrides the ExplainID in order to match different range.
///
/// `sc` is the statement context of Go's stored `ctx` (`M-CTX`); `None` is Go's nil context.
// go: PhysicalTableScan.ExplainID L1 [M-CTX]
pub fn table_scan_explain_id(
    p: &crate::core::physical_plans::PhysicalTableScan,
    sc: Option<&tidb_sessionctx::stmtctx::StatementContext>,
) -> String {
    if let Some(sc) = sc
        && sc.ignore_explain_id_suffix
    {
        return table_scan_tp(p).to_owned();
    }
    format!("{}_{}", table_scan_tp(p), crate::PhysicalPlan::id(p))
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
