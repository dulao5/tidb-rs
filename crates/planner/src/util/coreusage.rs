//! go-file: pkg/planner/util/coreusage/correlated_misc.go
//!
//! The physical-plan helper of the file (the table reader's correlated columns). The logical
//! ones and the schema filters are not used by the first vertical slice yet.

use std::sync::Arc;

use tidb_expression::CorrelatedColumn;

use crate::base::PhysicalPlan;

/// ExtractCorrelatedCols4PhysicalPlan recursively extracts all of the correlated columns
/// from a plan tree by calling PhysicalPlan.ExtractCorrelatedCols.
// go: ExtractCorrelatedCols4PhysicalPlan L1
pub fn extract_correlated_cols4_physical_plan(p: &dyn PhysicalPlan) -> Vec<Arc<CorrelatedColumn>> {
    let mut cor_cols = p.extract_correlated_cols();
    for child in p.children() {
        cor_cols.extend(extract_correlated_cols4_physical_plan(child.as_ref()));
    }
    cor_cols
}
