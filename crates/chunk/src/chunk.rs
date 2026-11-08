//! go-file: pkg/util/chunk/chunk.go
//!
//! `Chunk` (N5a 3.1: L1). Carriers:
//! - Columns are `Arc<Column>` (N5a 3.2): `MakeRef` shares a column between slots,
//!   `SwapColumns` swaps the vectors. Writes go through [`col_mut`]: a debug build panics when
//!   the column is shared (Go writes through the alias; copy on write would diverge silently),
//!   a release build copies.
//! - Go distinguishes a nil `columns` / `sel` from an empty one (`renewEmpty`, `Reset`,
//!   `NumRows`): both are `Option<Vec<_>>`.
//! - No reuse pool (`M-POOL`): `NewChunkFromPoolWithCapacity` allocates, `Destroy` drops.
//! - `AppendEnum`, `AppendSet`, `AppendDatum` and `ToString` need block D01.1 (`types.Enum`,
//!   `types.Set`, `types.Datum`) and wait for it.

// Go names for package-level constants (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::errors;
use tidb_parser::types::FieldType;
use tidb_types::MyDecimal;
use tidb_types::json::BinaryJSON;
use tidb_types::time::Duration;
use tidb_types::time::Time;
use tidb_types::vector::VectorFloat32;

use crate::column::Column;
use crate::column::new_column;
use crate::column::new_column_for;
use crate::column::new_empty_column;
use crate::goslice::ColAlloc;
use crate::row::Row;

pub(crate) const msgErrSelNotNil: &str =
    "The selection vector of Chunk is not nil. Please file a bug to the TiDB Team";

/// Chunk stores multiple rows of data in columns. Columns are in Apache Arrow format.
/// See https://arrow.apache.org/docs/format/Columnar.html#physical-memory-layout.
/// Apache Arrow is not used directly because we want to access MySQL types without decoding.
///
/// Values are appended in compact format and can be directly accessed without decoding.
/// When the chunk is done processing, we can reuse the allocated memory by resetting it.
///
/// All Chunk's API should not do the validation work, and the user should ensure it is used correctly.
#[derive(Clone, Debug, Default)]
pub struct Chunk {
    /// sel indicates which rows are selected.
    /// If it is nil, all rows are selected.
    pub(crate) sel: Option<Vec<usize>>,

    pub(crate) columns: Option<Vec<Arc<Column>>>,
    /// numVirtualRows indicates the number of virtual rows, which have zero Column.
    /// It is used only when this Chunk doesn't hold any data, i.e. "len(columns)==0".
    pub(crate) num_virtual_rows: usize,
    /// capacity indicates the max number of rows this chunk can hold.
    /// TODO: replace all usages of capacity to requiredRows and remove this field
    pub(crate) capacity: usize,

    /// requiredRows indicates how many rows the parent executor want.
    pub(crate) required_rows: usize,

    /// inCompleteChunk means some of the columns in the chunk is not filled, used in
    /// join probe, the value will always be false unless set it explicitly
    pub(crate) in_complete_chunk: bool,
}

/// Capacity constants.
pub const InitialCapacity: usize = 32;
/// Capacity constants.
pub const ZeroCapacity: usize = 0;

/// The column in a slot, for writing (N5a 3.2).
// glue: N5a 3.2 Arc<Column> write access (debug assertion against writes through an alias)
#[inline]
pub(crate) fn col_mut(a: &mut Arc<Column>) -> &mut Column {
    debug_assert!(
        Arc::strong_count(a) == 1,
        "write to a chunk column shared by {} owners (N5a 3.2)",
        Arc::strong_count(a)
    );
    Arc::make_mut(a)
}

/// NewEmptyChunk creates an empty chunk
// go: NewEmptyChunk L1
pub fn new_empty_chunk(fields: &[FieldType]) -> Chunk {
    let mut columns = Vec::with_capacity(fields.len());
    for f in fields {
        columns.push(Arc::new(new_empty_column(f)));
    }
    Chunk {
        columns: Some(columns),
        ..Chunk::default()
    }
}

/// NewChunkWithCapacity creates a new chunk with field types and capacity.
// go: NewChunkWithCapacity L1
pub fn new_chunk_with_capacity(fields: &[FieldType], capacity: usize) -> Chunk {
    new(fields, capacity, capacity)
}

/// NewChunkFromPoolWithCapacity creates a new chunk with field types and capacity from the pool.
// go: NewChunkFromPoolWithCapacity L2 (M-POOL: no pool; Go's pooled columns are created like New's)
pub fn new_chunk_from_pool_with_capacity(fields: &[FieldType], init_cap: usize) -> Chunk {
    new(fields, init_cap, init_cap)
}

/// New creates a new chunk.
///
///   cap: the limit for the max number of rows.
///   maxChunkSize: the max limit for the number of rows.
// go: New L1
pub fn new(fields: &[FieldType], capacity: usize, max_chunk_size: usize) -> Chunk {
    new_in(fields, capacity, max_chunk_size, ColAlloc::Global)
}

/// [`new`] with the columns allocating through `alloc` (A1 3.3, N5a 3.7).
// glue: N5a 3.7 New with a TrackedAlloc
pub fn new_in(fields: &[FieldType], capacity: usize, max_chunk_size: usize, alloc: ColAlloc) -> Chunk {
    let capacity = capacity.min(max_chunk_size);
    let mut columns = Vec::with_capacity(fields.len());
    for f in fields {
        let mut col = new_column_for(f, capacity);
        if !matches!(alloc, ColAlloc::Global) {
            col = new_column(col.type_size(), capacity, alloc.clone());
        }
        columns.push(Arc::new(col));
    }
    Chunk {
        columns: Some(columns),
        capacity,
        // set the default value of requiredRows to maxChunkSize to let chk.IsFull() behave
        // like how we judge whether a chunk is full now, then the statement
        // "chk.NumRows() < maxChunkSize"
        // equals to "!chk.IsFull()".
        required_rows: max_chunk_size,
        ..Chunk::default()
    }
}

/// renewWithCapacity creates a new Chunk based on an existing Chunk with capacity. The newly
/// created Chunk has the same data schema with the old Chunk.
// go: renewWithCapacity L1
pub(crate) fn renew_with_capacity(chk: &Chunk, capacity: usize, required_rows: usize) -> Chunk {
    let Some(columns) = &chk.columns else {
        return Chunk {
            in_complete_chunk: chk.in_complete_chunk,
            ..Chunk::default()
        };
    };
    Chunk {
        columns: Some(renew_columns(columns, capacity)),
        num_virtual_rows: 0,
        capacity,
        required_rows,
        in_complete_chunk: chk.in_complete_chunk,
        sel: None,
    }
}

/// Renew creates a new Chunk based on an existing Chunk. The newly created Chunk
/// has the same data schema with the old Chunk. The capacity of the new Chunk
/// might be doubled based on the capacity of the old Chunk and the maxChunkSize.
///
///   chk: old chunk(often used in previous call).
///   maxChunkSize: the limit for the max number of rows.
// go: Renew L1
pub fn renew(chk: &Chunk, max_chunk_size: usize) -> Chunk {
    let new_cap = re_calc_capacity(chk, max_chunk_size);
    renew_with_capacity(chk, new_cap, max_chunk_size)
}

/// renewColumns creates the columns of a Chunk. The capacity of the newly
/// created columns is equal to cap.
// go: renewColumns L1
fn renew_columns(old_col: &[Arc<Column>], capacity: usize) -> Vec<Arc<Column>> {
    let mut columns = Vec::with_capacity(old_col.len());
    for col in old_col {
        columns.push(Arc::new(new_column(col.type_size(), capacity, col.alloc.clone())));
    }
    columns
}

/// renewEmpty creates a new Chunk based on an existing Chunk
/// but keep columns empty.
// go: renewEmpty L1
fn renew_empty(chk: &Chunk) -> Chunk {
    Chunk {
        columns: None,
        num_virtual_rows: chk.num_virtual_rows,
        capacity: chk.capacity,
        required_rows: chk.required_rows,
        in_complete_chunk: chk.in_complete_chunk,
        sel: chk.sel.clone(),
    }
}

impl Chunk {
    /// The columns (empty when Go's `columns` is nil).
    // glue: Go c.columns
    #[inline]
    pub(crate) fn cols(&self) -> &[Arc<Column>] {
        self.columns.as_deref().unwrap_or(&[])
    }

    /// The column slots, for writing.
    // glue: Go c.columns
    #[inline]
    pub(crate) fn cols_mut(&mut self) -> &mut [Arc<Column>] {
        self.columns.as_deref_mut().unwrap_or(&mut [])
    }

    /// The column `col_idx`, for writing (N5a 3.2: panics in a debug build when it is
    /// shared).
    // glue: N5a 3.2 Go c.columns[colIdx] (write)
    #[inline]
    pub fn column_mut(&mut self, col_idx: usize) -> &mut Column {
        col_mut(&mut self.cols_mut()[col_idx])
    }

    /// The column `col_idx` as a shared handle.
    // glue: Go c.columns[colIdx] (the pointer)
    pub fn column_arc(&self, col_idx: usize) -> &Arc<Column> {
        &self.cols()[col_idx]
    }

    /// SetInCompleteChunk will set c.inCompleteChunk, used in join
    // go: Chunk.SetInCompleteChunk L1
    pub fn set_in_complete_chunk(&mut self, is_in_complete_chunk: bool) {
        self.in_complete_chunk = is_in_complete_chunk;
    }

    /// IsInCompleteChunk returns true if this chunk is inCompleteChunk, used only in test
    // go: Chunk.IsInCompleteChunk L1
    pub fn is_in_complete_chunk(&self) -> bool {
        self.in_complete_chunk
    }

    /// GetNumVirtualRows return c.numVirtualRows, used only in test
    // go: Chunk.GetNumVirtualRows L1
    pub fn get_num_virtual_rows(&self) -> usize {
        self.num_virtual_rows
    }

    /// MemoryUsage returns the total memory usage of a Chunk in bytes.
    /// We ignore the size of Column.length and Column.nullCount
    /// since they have little effect of the total memory usage.
    ///
    /// The capacities are Go's (see `goslice`), so the result is Go's; 112 is
    /// `unsafe.Sizeof(Column{})` on 64-bit platforms.
    // go: Chunk.MemoryUsage L1
    pub fn memory_usage(&self) -> i64 {
        const SIZEOF_COLUMN: i64 = 112;
        let mut sum = 0i64;
        for col in self.cols() {
            let (nb, offs, data, eb) = col.caps();
            sum += SIZEOF_COLUMN + nb as i64 + (offs * 8) as i64 + data as i64 + eb as i64;
        }
        sum
    }

    /// RequiredRows returns how many rows is considered full.
    // go: Chunk.RequiredRows L1
    pub fn required_rows(&self) -> usize {
        self.required_rows
    }

    /// SetRequiredRows sets the number of required rows.
    // go: Chunk.SetRequiredRows L1
    pub fn set_required_rows(&mut self, required_rows: isize, max_chunk_size: usize) -> &mut Chunk {
        let required_rows = if required_rows <= 0 || required_rows as usize > max_chunk_size {
            max_chunk_size
        } else {
            required_rows as usize
        };
        self.required_rows = required_rows;
        self
    }

    /// IsFull returns if this chunk is considered full.
    // go: Chunk.IsFull L1
    pub fn is_full(&self) -> bool {
        self.num_rows() >= self.required_rows
    }

    /// Prune creates a new Chunk according to `c` and prunes the columns
    /// whose index is not in `usedColIdxs`
    // go: Chunk.Prune L1
    pub fn prune(&self, used_col_idxs: &[usize]) -> Chunk {
        let mut chk = renew_empty(self);
        chk.columns = Some(used_col_idxs.iter().map(|&idx| self.cols()[idx].clone()).collect());
        chk
    }

    /// MakeRef makes Column in "dstColIdx" reference to Column in "srcColIdx".
    // go: Chunk.MakeRef L1
    pub fn make_ref(&mut self, src_col_idx: usize, dst_col_idx: usize) {
        let cols = self.cols_mut();
        cols[dst_col_idx] = cols[src_col_idx].clone();
    }

    /// MakeRefTo copies columns `src.columns[srcColIdx]` to `c.columns[dstColIdx]`.
    // go: Chunk.MakeRefTo L1
    pub fn make_ref_to(&mut self, dst_col_idx: usize, src: &Chunk, src_col_idx: usize) -> Result<(), Error> {
        if self.sel.is_some() || src.sel.is_some() {
            return Err(errors::new(msgErrSelNotNil));
        }
        self.cols_mut()[dst_col_idx] = src.cols()[src_col_idx].clone();
        Ok(())
    }

    /// swapColumn swaps Column "c.columns[colIdx]" with Column
    /// "other.columns[otherIdx]". If there exists columns refer to the Column to be
    /// swapped, we need to re-build the reference.
    /// this function should not be used directly, if you wants to swap columns between two chunks,
    /// use ColumnSwapHelper.SwapColumns instead.
    // go: Chunk.swapColumn L1
    pub(crate) fn swap_column(&mut self, mut col_idx: usize, other: &mut Chunk, mut other_idx: usize) -> Result<(), Error> {
        if self.sel.is_some() || other.sel.is_some() {
            return Err(errors::new(msgErrSelNotNil));
        }
        // Find the leftmost Column of the reference which is the actual Column to
        // be swapped.
        {
            let cols = self.cols();
            for i in 0..col_idx {
                if Arc::ptr_eq(&cols[i], &cols[col_idx]) {
                    col_idx = i;
                }
            }
            let ocols = other.cols();
            for i in 0..other_idx {
                if Arc::ptr_eq(&ocols[i], &ocols[other_idx]) {
                    other_idx = i;
                }
            }
        }

        // Find the columns which refer to the actual Column to be swapped.
        let cols = self.cols();
        let ref_cols_idx: Vec<usize> = (col_idx..cols.len())
            .filter(|&i| Arc::ptr_eq(&cols[i], &cols[col_idx]))
            .collect();
        let ocols = other.cols();
        let ref_cols_idx4_other: Vec<usize> = (other_idx..ocols.len())
            .filter(|&i| Arc::ptr_eq(&ocols[i], &ocols[other_idx]))
            .collect();

        // Swap columns from two chunks.
        std::mem::swap(&mut self.cols_mut()[col_idx], &mut other.cols_mut()[other_idx]);

        // Rebuild the reference.
        for i in ref_cols_idx {
            self.make_ref(col_idx, i);
        }
        for i in ref_cols_idx4_other {
            other.make_ref(other_idx, i);
        }
        Ok(())
    }

    /// SwapColumns swaps columns with another Chunk.
    // go: Chunk.SwapColumns L1
    pub fn swap_columns(&mut self, other: &mut Chunk) {
        std::mem::swap(&mut self.sel, &mut other.sel);
        std::mem::swap(&mut self.columns, &mut other.columns);
        std::mem::swap(&mut self.num_virtual_rows, &mut other.num_virtual_rows);
    }

    /// SetNumVirtualRows sets the virtual row number for a Chunk.
    /// It should only be used when there exists no Column in the Chunk.
    // go: Chunk.SetNumVirtualRows L1
    pub fn set_num_virtual_rows(&mut self, num_virtual_rows: usize) {
        self.num_virtual_rows = num_virtual_rows;
    }

    /// Reset resets the chunk, so the memory it allocated can be reused.
    /// Make sure all the data in the chunk is not used anymore before you reuse this chunk.
    ///
    /// Go resets a column shared by several slots once per slot (idempotent); here each
    /// shared column is reset once, in place when this chunk is its only owner.
    // go: Chunk.Reset L2 (a column shared by several slots is reset once, without copying)
    pub fn reset(&mut self) {
        self.sel = None;
        let Some(cols) = &mut self.columns else {
            return;
        };
        let n = cols.len();
        for i in 0..n {
            if (0..i).any(|j| Arc::ptr_eq(&cols[j], &cols[i])) {
                continue;
            }
            // Detach the aliases so this slot owns the column, reset it, re-attach.
            let aliases: Vec<usize> = (i + 1..n).filter(|&j| Arc::ptr_eq(&cols[j], &cols[i])).collect();
            for &j in &aliases {
                cols[j] = Arc::new(Column::zero(ColAlloc::Global));
            }
            Arc::make_mut(&mut cols[i]).reset();
            for &j in &aliases {
                cols[j] = cols[i].clone();
            }
        }
        self.num_virtual_rows = 0;
    }

    /// CopyConstruct creates a new chunk and copies this chunk's data into it.
    // go: Chunk.CopyConstruct L1
    pub fn copy_construct(&self) -> Chunk {
        let mut new_chk = renew_empty(self);
        new_chk.columns = Some(self.cols().iter().map(|c| Arc::new(c.copy_construct(None))).collect());
        new_chk
    }

    /// CopyConstructSel is just like CopyConstruct,
    /// but ignore the rows that was not selected.
    // go: Chunk.CopyConstructSel L1
    pub fn copy_construct_sel(&self) -> Chunk {
        let Some(sel) = &self.sel else {
            return self.copy_construct();
        };
        let mut new_chk = renew_with_capacity(self, self.capacity, self.required_rows);
        for (col_idx, dst_col) in new_chk.cols_mut().iter_mut().enumerate() {
            let dst_col = col_mut(dst_col);
            for &row_idx in sel {
                append_cell_by_cell(dst_col, &self.cols()[col_idx], row_idx);
            }
        }
        new_chk
    }

    /// GrowAndReset resets the Chunk and doubles the capacity of the Chunk.
    /// The doubled capacity should not be larger than maxChunkSize.
    // go: Chunk.GrowAndReset L1
    pub fn grow_and_reset(&mut self, max_chunk_size: usize) {
        self.sel = None;
        if self.columns.is_none() {
            return;
        }
        let new_cap = re_calc_capacity(self, max_chunk_size);
        if new_cap <= self.capacity {
            self.reset();
            return;
        }
        self.capacity = new_cap;
        self.columns = Some(renew_columns(self.cols(), new_cap));
        self.num_virtual_rows = 0;
        self.required_rows = max_chunk_size;
    }

    /// Capacity returns the capacity of the Chunk.
    // go: Chunk.Capacity L1
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// NumCols returns the number of columns in the chunk.
    // go: Chunk.NumCols L1
    #[inline]
    pub fn num_cols(&self) -> usize {
        self.cols().len()
    }

    /// NumRows returns the number of rows in the chunk.
    // go: Chunk.NumRows L1
    #[inline]
    pub fn num_rows(&self) -> usize {
        if let Some(sel) = &self.sel {
            return sel.len();
        }
        if self.in_complete_chunk || self.num_cols() == 0 {
            return self.num_virtual_rows;
        }
        self.cols()[0].length
    }

    /// GetRow gets the Row in the chunk with the row index.
    // go: Chunk.GetRow L1
    #[inline]
    pub fn get_row(&self, idx: usize) -> Row<'_> {
        if let Some(sel) = &self.sel {
            // mapping the logical RowIdx to the actual physical RowIdx;
            // for example, if the Sel is [1, 5, 6], then
            //	logical 0 -> physical 1,
            //	logical 1 -> physical 5,
            //	logical 2 -> physical 6.
            // Then when we iterate this Chunk according to Row, only selected rows will be
            // accessed while all filtered rows will be ignored.
            return Row::new(self, sel[idx]);
        }
        Row::new(self, idx)
    }

    /// AppendRow appends a row to the chunk.
    // go: Chunk.AppendRow L1
    pub fn append_row(&mut self, row: Row<'_>) {
        self.append_partial_row(0, row);
        self.num_virtual_rows += 1;
    }

    /// AppendPartialRow appends a row to the chunk, starting from colOff.
    // go: Chunk.AppendPartialRow L1
    pub fn append_partial_row(&mut self, col_off: usize, row: Row<'_>) {
        self.append_sel(col_off);
        for (i, row_col) in row.chunk().cols().iter().enumerate() {
            let chk_col = self.column_mut(col_off + i);
            append_cell_by_cell(chk_col, row_col, row.idx());
        }
    }

    /// AppendRowByColIdxs appends a row to the chunk, using the row's columns specified by colIdxs.
    /// 1. every columns are used if colIdxs is nil.
    /// 2. no columns are used if colIdxs is not nil but the size of colIdxs is 0.
    // go: Chunk.AppendRowByColIdxs L1
    pub fn append_row_by_col_idxs(&mut self, row: Row<'_>, col_idxs: Option<&[usize]>) -> usize {
        let wide = self.append_partial_row_by_col_idxs(0, row, col_idxs);
        self.num_virtual_rows += 1;
        wide
    }

    /// AppendPartialRowByColIdxs appends a row to the chunk starting from colOff,
    /// using the row's columns specified by colIdxs.
    /// 1. every columns are used if colIdxs is nil.
    /// 2. no columns are used if colIdxs is not nil but the size of colIdxs is 0.
    // go: Chunk.AppendPartialRowByColIdxs L1
    pub fn append_partial_row_by_col_idxs(&mut self, col_off: usize, row: Row<'_>, col_idxs: Option<&[usize]>) -> usize {
        let Some(col_idxs) = col_idxs else {
            self.append_partial_row(col_off, row);
            return row.len();
        };

        self.append_sel(col_off);
        for (i, &col_idx) in col_idxs.iter().enumerate() {
            let row_col = &row.chunk().cols()[col_idx];
            let chk_col = self.column_mut(col_off + i);
            append_cell_by_cell(chk_col, row_col, row.idx());
        }
        col_idxs.len()
    }

    /// Append appends rows in [begin, end) in another Chunk to a Chunk.
    // go: Chunk.Append L1
    pub fn append(&mut self, other: &Chunk, begin: usize, end: usize) {
        for (col_id, src) in other.cols().iter().enumerate() {
            {
                let dst = self.column_mut(col_id);
                if src.is_fixed() {
                    let elem_len = src.elem_len();
                    dst.data.append(&src.data.capb()[begin * elem_len..end * elem_len]);
                } else {
                    let (begin_offset, end_offset) = (src.offsets.i64_at(begin), src.offsets.i64_at(end));
                    dst.data.append(&src.data.capb()[begin_offset as usize..end_offset as usize]);
                    let mut last_offset = dst.offsets.last_i64();
                    for i in begin..end {
                        last_offset += src.offsets.i64_at(i + 1) - src.offsets.i64_at(i);
                        dst.offsets.push_i64(last_offset);
                    }
                }
            }
            for i in begin..end {
                self.append_sel(col_id);
                let dst = self.column_mut(col_id);
                dst.append_null_bitmap_inner(!src.is_null(i));
                dst.length += 1;
            }
        }
        self.num_virtual_rows += end - begin;
    }

    /// TruncateTo truncates rows from tail to head in a Chunk to "numRows" rows.
    // go: Chunk.TruncateTo L1
    pub fn truncate_to(&mut self, num_rows: usize) {
        self.reconstruct();
        for col in self.cols_mut() {
            let col = col_mut(col);
            if col.is_fixed() {
                let elem_len = col.elem_len();
                col.data.reslice(num_rows * elem_len);
            } else {
                let o = col.offsets.i64_at(num_rows) as usize;
                col.data.reslice(o);
                col.offsets.reslice(num_rows + 1);
            }
            col.length = num_rows;
            let bitmap_len = col.length.div_ceil(8);
            col.null_bitmap.reslice(bitmap_len);
            if col.length % 8 != 0 {
                // When we append null, we simply increment the nullCount,
                // so we need to clear the unused bits in the last bitmap byte.
                let mut last_byte = col.null_bitmap.at(bitmap_len - 1);
                let unused_bits_len = 8 - (col.length % 8) as u32;
                last_byte <<= unused_bits_len;
                last_byte >>= unused_bits_len;
                *col.null_bitmap.at_mut(bitmap_len - 1) = last_byte;
            }
        }
        self.num_virtual_rows = num_rows;
    }

    /// AppendNull appends a null value to the chunk.
    // go: Chunk.AppendNull L1
    pub fn append_null(&mut self, col_idx: usize) {
        self.append_sel(col_idx);
        self.column_mut(col_idx).append_null();
    }

    /// AppendInt64 appends a int64 value to the chunk.
    // go: Chunk.AppendInt64 L1
    #[inline]
    pub fn append_int64(&mut self, col_idx: usize, i: i64) {
        self.append_sel(col_idx);
        self.column_mut(col_idx).append_int64(i);
    }

    /// AppendUint64 appends a uint64 value to the chunk.
    // go: Chunk.AppendUint64 L1
    pub fn append_uint64(&mut self, col_idx: usize, u: u64) {
        self.append_sel(col_idx);
        self.column_mut(col_idx).append_uint64(u);
    }

    /// AppendFloat32 appends a float32 value to the chunk.
    // go: Chunk.AppendFloat32 L1
    pub fn append_float32(&mut self, col_idx: usize, f: f32) {
        self.append_sel(col_idx);
        self.column_mut(col_idx).append_float32(f);
    }

    /// AppendFloat64 appends a float64 value to the chunk.
    // go: Chunk.AppendFloat64 L1
    pub fn append_float64(&mut self, col_idx: usize, f: f64) {
        self.append_sel(col_idx);
        self.column_mut(col_idx).append_float64(f);
    }

    /// AppendString appends a string value to the chunk.
    // go: Chunk.AppendString L1
    pub fn append_string(&mut self, col_idx: usize, str: &[u8]) {
        self.append_sel(col_idx);
        self.column_mut(col_idx).append_string(str);
    }

    /// AppendBytes appends a bytes value to the chunk.
    // go: Chunk.AppendBytes L1
    pub fn append_bytes(&mut self, col_idx: usize, b: &[u8]) {
        self.append_sel(col_idx);
        self.column_mut(col_idx).append_bytes(b);
    }

    /// AppendTime appends a Time value to the chunk.
    // go: Chunk.AppendTime L1
    pub fn append_time(&mut self, col_idx: usize, t: Time) {
        self.append_sel(col_idx);
        self.column_mut(col_idx).append_time(t);
    }

    /// AppendDuration appends a Duration value to the chunk.
    /// Fsp is ignored.
    // go: Chunk.AppendDuration L1
    pub fn append_duration(&mut self, col_idx: usize, dur: Duration) {
        self.append_sel(col_idx);
        self.column_mut(col_idx).append_duration(dur);
    }

    /// AppendMyDecimal appends a MyDecimal value to the chunk.
    // go: Chunk.AppendMyDecimal L1
    pub fn append_my_decimal(&mut self, col_idx: usize, dec: &MyDecimal) {
        self.append_sel(col_idx);
        self.column_mut(col_idx).append_my_decimal(dec);
    }

    /// The chunk form of `AppendEnum` / `AppendSet` until block D01.1 ports `types.Enum` and
    /// `types.Set`: appends `(name, value)`.
    // glue: Go AppendEnum / AppendSet body (c.columns[colIdx].appendNameValue), types.Enum/Set wait for D01.1
    pub fn append_name_value(&mut self, col_idx: usize, name: &[u8], value: u64) {
        self.append_sel(col_idx);
        self.column_mut(col_idx).append_name_value(name, value);
    }

    /// AppendJSON appends a JSON value to the chunk.
    // go: Chunk.AppendJSON L1
    pub fn append_json(&mut self, col_idx: usize, j: &BinaryJSON<'_>) {
        self.append_sel(col_idx);
        self.column_mut(col_idx).append_json(j);
    }

    /// AppendVectorFloat32 appends a VectorFloat32 value to the chunk.
    // go: Chunk.AppendVectorFloat32 L1
    pub fn append_vector_float32(&mut self, col_idx: usize, v: &VectorFloat32<'_>) {
        self.append_sel(col_idx);
        self.column_mut(col_idx).append_vector_float32(v);
    }

    // go: Chunk.appendSel L1
    #[inline]
    pub(crate) fn append_sel(&mut self, col_idx: usize) {
        if col_idx == 0
            && let Some(sel) = &mut self.sel
        {
            // use column 0 as standard
            sel.push(self.columns.as_ref().unwrap()[0].length);
        }
    }

    /// Column returns the specific column.
    // go: Chunk.Column L1
    #[inline]
    pub fn column(&self, col_idx: usize) -> &Column {
        &self.cols()[col_idx]
    }

    /// SetCol sets the colIdx Column to col and returns the old Column.
    // go: Chunk.SetCol L1
    pub fn set_col(&mut self, col_idx: usize, col: Arc<Column>) -> Option<Arc<Column>> {
        let slot = &mut self.cols_mut()[col_idx];
        if Arc::ptr_eq(&col, slot) {
            return None;
        }
        Some(std::mem::replace(slot, col))
    }

    /// Sel returns Sel of this Chunk.
    // go: Chunk.Sel L1
    pub fn sel(&self) -> Option<&[usize]> {
        self.sel.as_deref()
    }

    /// SetSel sets a Sel for this Chunk.
    // go: Chunk.SetSel L1
    pub fn set_sel(&mut self, sel: Option<Vec<usize>>) {
        self.sel = sel;
    }

    /// CloneEmpty returns an empty chunk that has the same schema with current chunk
    // go: Chunk.CloneEmpty L1
    pub fn clone_empty(&self, max_capacity: usize) -> Chunk {
        renew_with_capacity(self, max_capacity, max_capacity)
    }

    /// Reconstruct removes all filtered rows in this Chunk.
    // go: Chunk.Reconstruct L1
    pub fn reconstruct(&mut self) {
        let Some(sel) = self.sel.take() else {
            return;
        };
        for col in self.cols_mut() {
            col_mut(col).reconstruct(Some(&sel));
        }
        self.num_virtual_rows = sel.len();
    }

    /// AppendRows appends multiple rows to the chunk.
    // go: Chunk.AppendRows L1
    pub fn append_rows(&mut self, rows: &[Row<'_>]) {
        self.append_partial_rows(0, rows);
        self.num_virtual_rows += rows.len();
    }

    /// AppendPartialRows appends multiple rows to the chunk.
    // go: Chunk.AppendPartialRows L1
    pub fn append_partial_rows(&mut self, col_off: usize, rows: &[Row<'_>]) {
        let n = self.num_cols() - col_off;
        for i in 0..n {
            for src_row in rows {
                if i == 0 {
                    self.append_sel(col_off);
                }
                let dst_col = self.column_mut(col_off + i);
                append_cell_by_cell(dst_col, &src_row.chunk().cols()[i], src_row.idx());
            }
        }
    }

    /// Destroy is to destroy the Chunk and put Chunk into the pool
    // go: Chunk.Destroy L2 (M-POOL: no pool, the chunk is dropped)
    pub fn destroy(self, _init_cap: usize, _fields: &[FieldType]) {}
}

/// reCalcCapacity calculates the capacity for another Chunk based on the current
/// Chunk. The new capacity is doubled only when the current Chunk is full.
// go: reCalcCapacity L1
fn re_calc_capacity(c: &Chunk, max_chunk_size: usize) -> usize {
    if c.num_rows() < c.capacity {
        return c.capacity;
    }
    let mut new_capacity = c.capacity * 2;
    if new_capacity == 0 {
        new_capacity = InitialCapacity;
    }
    new_capacity.min(max_chunk_size)
}

/// appendCellByCell appends the cell with rowIdx of src into dst.
// go: appendCellByCell L1
#[inline]
pub(crate) fn append_cell_by_cell(dst: &mut Column, src: &Column, row_idx: usize) {
    dst.append_null_bitmap_inner(!src.is_null(row_idx));
    if src.is_fixed() {
        let elem_len = src.elem_len();
        let offset = row_idx * elem_len;
        dst.data.append(&src.data.capb()[offset..offset + elem_len]);
    } else {
        let (start, end) = (src.offsets.i64_at(row_idx), src.offsets.i64_at(row_idx + 1));
        dst.data.append(&src.data.capb()[start as usize..end as usize]);
        dst.offsets.push_i64(dst.data.len() as i64);
    }
    dst.length += 1;
}

/// AppendCellFromRawData appends the cell from raw data
///
/// Go reads from a raw pointer; here `row_data` is the row's bytes and `current_offset`
/// the position of the cell: fixed-length cells are `len(dst.elemBuf)` bytes, variable-length
/// cells a native-endian `uint64` length and the bytes.
// go: AppendCellFromRawData L2 (a byte slice instead of an unsafe.Pointer)
pub fn append_cell_from_raw_data(dst: &mut Column, row_data: &[u8], mut current_offset: usize) -> usize {
    if dst.is_fixed() {
        let elem_len = dst.elem_len();
        dst.data.append(&row_data[current_offset..current_offset + elem_len]);
        current_offset += elem_len;
    } else {
        let elem_len = u64::from_ne_bytes(row_data[current_offset..current_offset + 8].try_into().unwrap()) as usize;
        if elem_len > 0 {
            dst.data.append(&row_data[current_offset + 8..current_offset + 8 + elem_len]);
        }
        dst.offsets.push_i64(dst.data.len() as i64);
        current_offset += elem_len + 8;
    }
    dst.length += 1;
    current_offset
}
