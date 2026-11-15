//! go-file: pkg/executor/table_reader.go
//!
//! The table reader executor (block D12.1) comes with VS1-34. VS1-12 needs only
//! `buildVirtualColumnIndex`, shared with the point get.

use std::sync::Arc;

use tidb_expression::Schema;
use tidb_model::{ColumnInfo, find_column_info_by_id};

/// The schema positions of the virtual generated columns, in column definition order (the
/// order they are computed in).
///
/// Go calls `plannercore.FindColumnInfoByID` (D10.9, VS1-31); the model's function of the
/// same name has the same body (the first column with the ID).
// go: buildVirtualColumnIndex L1
pub fn build_virtual_column_index(schema: &Schema, columns: &[Arc<ColumnInfo>]) -> Vec<usize> {
    let mut virtual_column_index: Vec<usize> = Vec::with_capacity(columns.len());
    for (i, col) in schema.columns.iter().enumerate() {
        if col.virtual_expr.is_some() {
            virtual_column_index.push(i);
        }
    }
    let offset = |i: usize| {
        find_column_info_by_id(columns, schema.columns[i].id)
            .expect("a virtual column of the plan's columns")
            .offset
    };
    // slices.SortFunc: not stable; the offsets of distinct columns differ.
    virtual_column_index.sort_by_key(|&i| offset(i));
    virtual_column_index
}
