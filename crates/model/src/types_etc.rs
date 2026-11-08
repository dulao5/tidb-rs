//! go-file: pkg/types/etc.go
//!
//! `ColumnNeedRestoredData` takes the model structs, so it lives here (block D06.1) rather
//! than in tidb-types: Go's `pkg/types` imports `pkg/meta/model`, the Rust crates are
//! layered the other way (N0: tidb-types rank 2, tidb-model rank 3).

use tidb_types::etc::need_restored_data;

use crate::{ColumnInfo, IndexColumn, get_idx_changing_field_type};

/// ColumnNeedRestoredData checks whether a single index column needs restored data.
/// Panics when the column offset is out of range (Go's index panic).
// go: ColumnNeedRestoredData L1 [M-PANIC]
pub fn column_need_restored_data(idx_col: &IndexColumn, col_infos: &[ColumnInfo]) -> bool {
    let col = &col_infos[idx_col.offset as usize];
    need_restored_data(get_idx_changing_field_type(idx_col, col))
}
