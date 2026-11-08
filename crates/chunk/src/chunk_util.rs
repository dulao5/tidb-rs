//! go-file: pkg/util/chunk/chunk_util.go
//!
//! Row copying helpers and `ColumnSwapHelper` (N5a 3.1: L1). `diskFileReaderWriter` (the
//! spill file of `chunk_in_disk.go` / `row_in_disk.go`, with checksum and encryption layers)
//! belongs with block D01.10 and waits for it.

// Go index loops over `selected` (workspace style-lint list).
#![allow(clippy::needless_range_loop)]

use std::collections::HashMap;
use std::sync::OnceLock;

use tidb_base::Error;
use tidb_base::errors;

use crate::chunk::Chunk;
use crate::chunk::col_mut;
use crate::chunk::msgErrSelNotNil;
use crate::column::Column;

/// CopySelectedJoinRowsDirect directly copies the selected joined rows from the source Chunk
/// to the destination Chunk.
/// Return true if at least one joined row was selected.
// go: CopySelectedJoinRowsDirect L1
pub fn copy_selected_join_rows_direct(
    src: &Chunk,
    selected: &[bool],
    dst: &mut Chunk,
) -> Result<bool, Error> {
    if src.num_rows() == 0 {
        return Ok(false);
    }
    if src.sel.is_some() || dst.sel.is_some() {
        return Err(errors::new(msgErrSelNotNil));
    }
    if src.cols().is_empty() {
        let num_selected = selected.iter().filter(|&&s| s).count();
        dst.num_virtual_rows += num_selected;
        return Ok(num_selected > 0);
    }

    let old_len = dst.cols()[0].length;
    for (j, src_col) in src.cols().iter().enumerate() {
        let dst_col = dst.column_mut(j);
        if src_col.is_fixed() {
            for (i, &sel) in selected.iter().enumerate() {
                if !sel {
                    continue;
                }
                dst_col.append_null_bitmap_inner(!src_col.is_null(i));
                dst_col.length += 1;

                let elem_len = src_col.elem_len();
                let offset = i * elem_len;
                dst_col
                    .data
                    .append(&src_col.data.capb()[offset..offset + elem_len]);
            }
        } else {
            for (i, &sel) in selected.iter().enumerate() {
                if !sel {
                    continue;
                }
                dst_col.append_null_bitmap_inner(!src_col.is_null(i));
                dst_col.length += 1;

                let (start, end) = (src_col.offsets.i64_at(i), src_col.offsets.i64_at(i + 1));
                dst_col
                    .data
                    .append(&src_col.data.capb()[start as usize..end as usize]);
                dst_col.offsets.push_i64(dst_col.data.len() as i64);
            }
        }
    }
    let num_selected = dst.cols()[0].length - old_len;
    dst.num_virtual_rows += num_selected;
    Ok(num_selected > 0)
}

/// CopySelectedJoinRowsWithSameOuterRows copies the selected joined rows from the source Chunk
/// to the destination Chunk.
/// Return true if at least one joined row was selected.
///
/// NOTE: All the outer rows in the source Chunk should be the same.
// go: CopySelectedJoinRowsWithSameOuterRows L1
#[allow(clippy::too_many_arguments)]
pub fn copy_selected_join_rows_with_same_outer_rows(
    src: &Chunk,
    inner_col_offset: usize,
    inner_col_len: usize,
    outer_col_offset: usize,
    outer_col_len: usize,
    selected: &[bool],
    dst: &mut Chunk,
) -> Result<bool, Error> {
    if src.num_rows() == 0 {
        return Ok(false);
    }
    if src.sel.is_some() || dst.sel.is_some() {
        return Err(errors::new(msgErrSelNotNil));
    }

    let num_selected =
        copy_selected_inner_rows(inner_col_offset, inner_col_len, src, selected, dst);
    copy_same_outer_rows(outer_col_offset, outer_col_len, src, num_selected, dst);
    dst.num_virtual_rows += num_selected;
    Ok(num_selected > 0)
}

/// CopySelectedRows copies the selected rows in srcCol to dstCol
// go: CopySelectedRows L1
pub fn copy_selected_rows(dst_col: &mut Column, src_col: &Column, selected: &[bool]) {
    copy_selected_rows_with_row_id_func(dst_col, src_col, selected, 0, selected.len(), |i| i);
}

/// CopySelectedRowsWithRowIDFunc copies the selected rows in srcCol to dstCol
// go: CopySelectedRowsWithRowIDFunc L1
pub fn copy_selected_rows_with_row_id_func(
    dst_col: &mut Column,
    src_col: &Column,
    selected: &[bool],
    start: usize,
    end: usize,
    row_id_func: impl Fn(usize) -> usize,
) {
    if src_col.is_fixed() {
        for i in start..end {
            if !selected[i] {
                continue;
            }
            let row_id = row_id_func(i);
            dst_col.append_null_bitmap_inner(!src_col.is_null(row_id));
            dst_col.length += 1;

            let elem_len = src_col.elem_len();
            let offset = row_id * elem_len;
            dst_col
                .data
                .append(&src_col.data.capb()[offset..offset + elem_len]);
        }
    } else {
        for i in start..end {
            if !selected[i] {
                continue;
            }
            let row_id = row_id_func(i);
            dst_col.append_null_bitmap_inner(!src_col.is_null(row_id));
            dst_col.length += 1;

            let (start, end) = (
                src_col.offsets.i64_at(row_id),
                src_col.offsets.i64_at(row_id + 1),
            );
            dst_col
                .data
                .append(&src_col.data.capb()[start as usize..end as usize]);
            dst_col.offsets.push_i64(dst_col.data.len() as i64);
        }
    }
}

/// copySelectedInnerRows copies the selected inner rows from the source Chunk
/// to the destination Chunk.
/// return the number of rows which is selected.
// go: copySelectedInnerRows L1
fn copy_selected_inner_rows(
    inner_col_offset: usize,
    inner_col_len: usize,
    src: &Chunk,
    selected: &[bool],
    dst: &mut Chunk,
) -> usize {
    let src_cols = &src.cols()[inner_col_offset..inner_col_offset + inner_col_len];
    if src_cols.is_empty() {
        return selected.iter().filter(|&&s| s).count();
    }
    let old_len = dst.cols()[inner_col_offset].length;
    for (j, src_col) in src_cols.iter().enumerate() {
        let dst_col = dst.column_mut(inner_col_offset + j);
        copy_selected_rows(dst_col, src_col, selected);
    }
    dst.cols()[inner_col_offset].length - old_len
}

/// copySameOuterRows copies the continuous 'numRows' outer rows in the source Chunk
/// to the destination Chunk.
// go: copySameOuterRows L1
fn copy_same_outer_rows(
    outer_col_offset: usize,
    outer_col_len: usize,
    src: &Chunk,
    num_rows: usize,
    dst: &mut Chunk,
) {
    if num_rows == 0 || outer_col_len == 0 {
        return;
    }
    let row = src.get_row(0);
    let src_cols = &src.cols()[outer_col_offset..outer_col_offset + outer_col_len];
    for (i, src_col) in src_cols.iter().enumerate() {
        let dst_col = col_mut(&mut dst.cols_mut()[outer_col_offset + i]);
        dst_col.append_multi_same_null_bitmap(!src_col.is_null(row.idx()), num_rows);
        dst_col.length += num_rows;
        if src_col.is_fixed() {
            let elem_len = src_col.elem_len();
            let start = row.idx() * elem_len;
            let end = start + num_rows * elem_len;
            dst_col.data.append(&src_col.data.capb()[start..end]);
        } else {
            let (start, end) = (
                src_col.offsets.i64_at(row.idx()),
                src_col.offsets.i64_at(row.idx() + num_rows),
            );
            dst_col
                .data
                .append(&src_col.data.capb()[start as usize..end as usize]);
            let elem_len =
                src_col.offsets.i64_at(row.idx() + 1) - src_col.offsets.i64_at(row.idx());
            for _ in 0..num_rows {
                let last = dst_col.offsets.last_i64();
                dst_col.offsets.push_i64(last + elem_len);
            }
        }
    }
}

/// ColumnSwapHelper is used to help swap columns in a chunk.
#[derive(Debug, Default)]
pub struct ColumnSwapHelper {
    /// InputIdxToOutputIdxes maps the input column index to the output column indexes.
    pub input_idx_to_output_idxes: HashMap<usize, Vec<usize>>,
    /// mergedInputIdxToOutputIdxes is only determined in runtime when saw the input chunk.
    /// Go: an atomic.Pointer set once with CompareAndSwap(nil, ...).
    merged_input_idx_to_output_idxes: OnceLock<Vec<(usize, Vec<usize>)>>,
}

impl ColumnSwapHelper {
    /// SwapColumns evaluates "Column" expressions.
    /// it will change the content of the input Chunk.
    ///
    /// Go ranges over a map (random order); the merged entries are visited in input index
    /// order here (mapping rule M-MAPORDER). Every order yields the same column sharing.
    // go: ColumnSwapHelper.SwapColumns L1 [M-MAPORDER]
    pub fn swap_columns(&self, input: &mut Chunk, output: &mut Chunk) -> Result<(), Error> {
        // mergedInputIdxToOutputIdxes only can be determined in runtime when we saw the input chunk structure.
        let merged = self.merged_input_idx_to_output_idxes.get_or_init(|| {
            self.merge_input_idx_to_output_idxes(input, &self.input_idx_to_output_idxes)
        });
        for (input_idx, output_idxes) in merged {
            output.swap_column(output_idxes[0], input, *input_idx)?;
            for &o in &output_idxes[1..] {
                output.make_ref(output_idxes[0], o);
            }
        }
        Ok(())
    }

    /// mergeInputIdxToOutputIdxes merges separate inputIdxToOutputIdxes entries when column references
    /// are detected within the input chunk. This process ensures consistent handling of columns derived
    /// from the same original source (see the Go comment for the full example: two output
    /// columns that reference the same input column must be swapped as one).
    ///
    /// Go groups the aliased input columns with a disjoint set whose root is the smallest
    /// index of the group (`Union(i, j)` with `i < j` keeps `i` as the root); the root is
    /// computed directly here. The output lists are concatenated in input index order (Go:
    /// map order, M-MAPORDER).
    // go: ColumnSwapHelper.mergeInputIdxToOutputIdxes L2 (disjointset inlined: the root is the smallest aliased index; M-MAPORDER: entries in input index order)
    fn merge_input_idx_to_output_idxes(
        &self,
        input: &Chunk,
        input_idx_to_output_idxes: &HashMap<usize, Vec<usize>>,
    ) -> Vec<(usize, Vec<usize>)> {
        let cols = input.cols();
        let root = |i: usize| -> usize {
            if i >= cols.len() {
                return i;
            }
            (0..i)
                .find(|&j| triomphe::Arc::ptr_eq(&cols[j], &cols[i]))
                .unwrap_or(i)
        };
        let mut keys: Vec<usize> = input_idx_to_output_idxes.keys().copied().collect();
        keys.sort_unstable();
        let mut merged: Vec<(usize, Vec<usize>)> = Vec::new();
        for input_idx in keys {
            let original_val = root(input_idx);
            let outs = &input_idx_to_output_idxes[&input_idx];
            match merged.iter_mut().find(|(k, _)| *k == original_val) {
                Some((_, v)) => v.extend_from_slice(outs),
                None => merged.push((original_val, outs.clone())),
            }
        }
        merged.sort_by_key(|(k, _)| *k);
        merged
    }
}

/// NewColumnSwapHelper creates a new ColumnSwapHelper.
// go: NewColumnSwapHelper L1
pub fn new_column_swap_helper(used_column_index: &[usize]) -> ColumnSwapHelper {
    let mut helper = ColumnSwapHelper::default();
    for (output_index, &input_index) in used_column_index.iter().enumerate() {
        helper
            .input_idx_to_output_idxes
            .entry(input_index)
            .or_default()
            .push(output_index);
    }
    helper
}
