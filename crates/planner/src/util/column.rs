//! go-file: pkg/planner/util/column.go
//!
//! `*expression.Column` is `Arc<Column>`; Go's nil entry of `IndexInfo2FullCols` is `None`.
//! `intest.Assert` is a `debug_assert!`.

use std::sync::Arc;

use tidb_expression::Column;
use tidb_model::{ColumnInfo, IndexColumn, IndexInfo};
use tidb_parser::types::UnspecifiedLength;

/// indexCol2Col finds the corresponding column of the IndexColumn in a column slice.
// go: indexCol2Col L1
fn index_col2_col(
    col_infos: &[ColumnInfo],
    cols: &[Arc<Column>],
    col: &IndexColumn,
) -> Option<Arc<Column>> {
    for (i, info) in col_infos.iter().enumerate() {
        if info.name.l == col.name.l {
            if col.length > 0 && info.field_type.get_flen() > col.length {
                let mut c = (*cols[i]).clone();
                c.is_prefix = true;
                return Some(Arc::new(c));
            }
            return Some(Arc::clone(&cols[i]));
        }
    }
    None
}

/// `indexInfo2ColsFlags`.
type IndexInfo2ColsFlags = u32;

const EXTRACT_PREFIX_COLS: IndexInfo2ColsFlags = 1;
const EXTRACT_FULL_COLS: IndexInfo2ColsFlags = 1 << 1;

/// The four results of `indexInfo2ColsImpl`: prefix columns and lengths, full columns and
/// lengths. A result Go leaves nil (its flag is not set) is empty.
pub type IndexCols = (
    Vec<Arc<Column>>,
    Vec<isize>,
    Vec<Option<Arc<Column>>>,
    Vec<isize>,
);

// go: indexInfo2ColsImpl L1
fn index_info2_cols_impl(
    col_infos: &[ColumnInfo],
    cols: &[Arc<Column>],
    index: &IndexInfo,
    flags: IndexInfo2ColsFlags,
) -> IndexCols {
    debug_assert!(
        flags != 0,
        "at least one of indexInfo2ColsFlags must be set"
    );
    let index_columns = index.columns.as_deref().unwrap_or(&[]);
    let (mut prefix_cols, mut prefix_lens, mut full_cols, mut full_lens) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    if flags & EXTRACT_PREFIX_COLS != 0 {
        prefix_cols = Vec::with_capacity(index_columns.len());
        prefix_lens = Vec::with_capacity(index_columns.len());
    }
    if flags & EXTRACT_FULL_COLS != 0 {
        full_cols = Vec::with_capacity(index_columns.len());
        full_lens = Vec::with_capacity(index_columns.len());
    }
    let mut prefix_complete = false;

    for c in index_columns {
        let Some(col) = index_col2_col(col_infos, cols, c) else {
            prefix_complete = true;
            if flags & EXTRACT_FULL_COLS == 0 {
                return (prefix_cols, prefix_lens, full_cols, full_lens);
            }
            full_cols.push(None);
            full_lens.push(UnspecifiedLength);
            continue;
        };

        // We use `types.UnspecifiedLength` to specifically indicate that a column does not
        // have a prefix length (see the `hasPrefix` function in util/ranger, for example).
        // In other words, `length` is only used for indexed columns with a prefix length
        // (as long as it is less than the full length).
        let mut length = c.length;
        if length != UnspecifiedLength && length == col.ret_type.get_flen() {
            length = UnspecifiedLength;
        }
        if flags & EXTRACT_PREFIX_COLS != 0 && !prefix_complete {
            prefix_cols.push(Arc::clone(&col));
            prefix_lens.push(length);
        }
        if flags & EXTRACT_FULL_COLS != 0 {
            full_cols.push(Some(col));
            full_lens.push(length);
        }
    }
    (prefix_cols, prefix_lens, full_cols, full_lens)
}

/// IndexInfo2PrefixCols gets the corresponding []*expression.Column of the indexInfo's []*IndexColumn,
/// together with a []int containing their lengths.
/// If this index has three IndexColumn that the 1st and 3rd IndexColumn has corresponding *expression.Column,
/// the return value will be only the 1st corresponding *expression.Column and its length.
/// TODO: Use a struct to represent {*expression.Column, int}.
// go: IndexInfo2PrefixCols L1
pub fn index_info2_prefix_cols(
    col_infos: &[ColumnInfo],
    cols: &[Arc<Column>],
    index: &IndexInfo,
) -> (Vec<Arc<Column>>, Vec<isize>) {
    let (prefix_cols, prefix_lens, _, _) =
        index_info2_cols_impl(col_infos, cols, index, EXTRACT_PREFIX_COLS);
    (prefix_cols, prefix_lens)
}

/// IndexInfo2FullCols gets the corresponding []*expression.Column of the indexInfo's []*IndexColumn,
/// together with a []int containing their lengths.
/// If this index has three IndexColumn that the 1st and 3rd IndexColumn has corresponding *expression.Column,
/// the return value will be [col1, nil, col2].
// go: IndexInfo2FullCols L1
pub fn index_info2_full_cols(
    col_infos: &[ColumnInfo],
    cols: &[Arc<Column>],
    index: &IndexInfo,
) -> (Vec<Option<Arc<Column>>>, Vec<isize>) {
    let (_, _, full_cols, full_lens) =
        index_info2_cols_impl(col_infos, cols, index, EXTRACT_FULL_COLS);
    (full_cols, full_lens)
}

/// IndexInfo2Cols returns the combined result of IndexInfo2PrefixCols and IndexInfo2FullCols.
// go: IndexInfo2Cols L1
pub fn index_info2_cols(
    col_infos: &[ColumnInfo],
    cols: &[Arc<Column>],
    index: &IndexInfo,
) -> IndexCols {
    index_info2_cols_impl(
        col_infos,
        cols,
        index,
        EXTRACT_PREFIX_COLS | EXTRACT_FULL_COLS,
    )
}
