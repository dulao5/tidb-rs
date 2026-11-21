//! go-file: pkg/planner/core/rule/util/build_key_info_misc.go
//!
//! The pure helpers of the key-info rule (block D09.13). `BuildKeyInfoPortal`, a function
//! variable other packages set to break an import cycle, has no Rust counterpart: callers
//! use the key-info functions of the crate directly.

use std::collections::HashSet;
use std::sync::Arc;

use tidb_base::mysql::has_not_null_flag;
use tidb_expression::{KeyInfo, Schema};
use tidb_model::{ColumnInfo, IndexInfo};

/// CheckMaxOneRowCond check if a condition is the form of (uniqueKey = constant) or (uniqueKey =
/// Correlated column), it returns at most one row.
///
/// Go's `map[int64]struct{}` is a `HashSet` (a nil map reads as empty).
// go: CheckMaxOneRowCond L1
pub fn check_max_one_row_cond(eq_col_ids: &HashSet<i64>, child_schema: &Schema) -> bool {
    if eq_col_ids.is_empty() {
        return false;
    }
    // We check `UniqueKeys` as well since the condition is `col = con | corr`, not `col <=> con | corr`.
    let keys = child_schema
        .keys
        .iter()
        .chain(child_schema.unique_keys.iter());
    let mut max_one_row;
    for cols in keys {
        max_one_row = true;
        for c in cols {
            if !eq_col_ids.contains(&c.unique_id) {
                max_one_row = false;
                break;
            }
        }
        if max_one_row {
            return true;
        }
    }
    false
}

/// CheckIndexCanBeKey checks whether an Index can be a Key in schema.
///
/// Returns `(uniqueKey, newKey)`; Go's nil `KeyInfo` is `None`.
// go: CheckIndexCanBeKey L1
pub fn check_index_can_be_key(
    idx: &IndexInfo,
    columns: &[Arc<ColumnInfo>],
    schema: &Schema,
) -> (Option<KeyInfo>, Option<KeyInfo>) {
    if !idx.unique {
        return (None, None);
    }
    let mut unique_key: Option<KeyInfo> = None;
    let mut new_key: Option<KeyInfo> = None;
    let mut new_key_ok = true;
    let mut unique_key_ok = true;
    for idx_col in idx.columns.iter().flatten() {
        // The columns of this index should all occur in column schema.
        // Since null value could be duplicate in unique key. So we check NotNull flag of every column.
        let mut find_unique_key = false;
        for (i, col) in columns.iter().enumerate() {
            if idx_col.name.l == col.name.l {
                unique_key
                    .get_or_insert_with(Vec::new)
                    .push(Arc::clone(&schema.columns[i]));
                find_unique_key = true;
                if new_key_ok {
                    if !has_not_null_flag(col.get_flag()) {
                        new_key_ok = false;
                        break;
                    }
                    new_key
                        .get_or_insert_with(Vec::new)
                        .push(Arc::clone(&schema.columns[i]));
                    break;
                }
            }
        }
        if !find_unique_key {
            new_key_ok = false;
            unique_key_ok = false;
            break;
        }
    }
    if new_key_ok {
        return (None, new_key);
    } else if unique_key_ok {
        return (unique_key, None);
    }
    (None, None)
}
