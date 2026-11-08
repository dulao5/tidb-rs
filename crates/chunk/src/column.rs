//! go-file: pkg/util/chunk/column.go
//!
//! `Column` (N5a 3.1: L1). Carriers (N5a 3.2, 3.3):
//! - `nullBitmap`, `offsets` and `data` are [`GoSlice`]s: Go's length and capacity, owned or
//!   a zero-copy slice of a shared response buffer (copy on write). `offsets` is a `[]int64`
//!   stored little-endian.
//! - `elemBuf` is kept with Go's semantics ([`ElemBuf`]): appending a NULL to a fixed-length
//!   column appends the bytes of the last value written to the buffer, and those bytes are
//!   part of `Codec.Encode`'s output.
//! - The typed slices (`Int64s`, `Decimals`, ...) are views with 1-byte alignment
//!   (`zerocopy` little-endian types, decision D24): the data of a decoded column is
//!   generally not aligned.
//! - `avoidReusing` only exists for the column pool, which A1 removes (`M-POOL`).
//! - Go's `types.Enum` / `types.Set` are block D01.1: `AppendEnum`, `AppendSet`, `GetEnum`
//!   and `GetSet` wait for it (registered in the govec exemptions); `appendNameValue` /
//!   `getNameValue` are ported.

// Go names for package-level constants (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use std::borrow::Cow;

use tidb_parser::types::EvalType;
use tidb_parser::types::FieldType;
use tidb_parser::types::{
    ETDatetime, ETDecimal, ETDuration, ETInt, ETJson, ETReal, ETString, ETTimestamp,
    ETVectorFloat32,
};
use tidb_types::MY_DECIMAL_STRUCT_SIZE;
use tidb_types::MyDecimal;
use tidb_types::core_time::CoreTime;
use tidb_types::json::BinaryJSON;
use tidb_types::time::Duration;
use tidb_types::time::Time;
use tidb_types::vector::VectorFloat32;
use tidb_types::vector::zero_copy_deserialize_vector_float32;
use zerocopy::FromBytes;
use zerocopy::little_endian::F32;
use zerocopy::little_endian::F64;
use zerocopy::little_endian::I64;
use zerocopy::little_endian::U64;

use crate::codec::VarElemLen;
use crate::codec::get_fixed_len;
use crate::goslice::ColAlloc;
use crate::goslice::GoSlice;
use crate::goslice::growslice_cap;

/// For varLenColumn (e.g. varchar), the accurate length of an element is unknown.
/// Therefore, in the first executor.Next we use an experience value -- 8 (so it may make runtime.growslice)
const estimatedElemLen: usize = 8;

/// A decimal stored in a column: the 40-byte image of Go's `MyDecimal` (N5a 3.3), 1-byte
/// aligned. Convert with [`MyDecimal::from_wire`] / [`MyDecimal::as_wire`].
pub type MyDecimalBytes = [u8; MY_DECIMAL_STRUCT_SIZE];

/// Go's `elemBuf []byte`: a scratch buffer as long as one fixed-length element, `None` for
/// a variable-length column (Go: nil). The length and capacity follow Go, because the
/// content is observable (see the module comment).
#[derive(Clone, Copy, Debug)]
pub(crate) struct ElemBuf {
    len: usize,
    cap: usize,
    b: [u8; 48],
}

impl ElemBuf {
    // glue: Go make([]byte, n)
    fn make(n: usize) -> ElemBuf {
        ElemBuf {
            len: n,
            cap: n,
            b: [0; 48],
        }
    }

    // glue: Go elemBuf[:len]
    #[inline]
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.b[..self.len]
    }

    // glue: Go cap(elemBuf)
    #[inline]
    pub(crate) fn cap(&self) -> usize {
        self.cap
    }

    // glue: Go make([]byte, n)
    pub(crate) fn new_zeroed(n: usize) -> ElemBuf {
        ElemBuf::make(n)
    }
}

/// `dst = append(dst[:0], src...)` for an `elemBuf` (`CopyConstruct`).
// glue: Go append on elemBuf
fn elem_buf_copy(dst: Option<ElemBuf>, src: Option<ElemBuf>) -> Option<ElemBuf> {
    let Some(src) = src else {
        // append(dst[:0]) keeps a non-nil dst, with length 0.
        return dst.map(|mut d| {
            d.len = 0;
            d
        });
    };
    let mut d = match dst {
        Some(d) if d.cap >= src.len => d,
        Some(d) => ElemBuf {
            len: 0,
            cap: growslice_cap(src.len, d.cap, 1),
            b: d.b,
        },
        None => ElemBuf {
            len: 0,
            cap: growslice_cap(src.len, 0, 1),
            b: [0; 48],
        },
    };
    d.b[..src.len].copy_from_slice(src.bytes());
    d.len = src.len;
    Some(d)
}

impl Column {
    /// AppendDuration appends a duration value into this Column.
    /// Fsp is ignored.
    // go: Column.AppendDuration L1
    pub fn append_duration(&mut self, dur: Duration) {
        self.append_int64(dur.duration);
    }

    /// AppendMyDecimal appends a MyDecimal value into this Column.
    // go: Column.AppendMyDecimal L1
    pub fn append_my_decimal(&mut self, dec: &MyDecimal) {
        self.elem_buf_mut()[..MY_DECIMAL_STRUCT_SIZE].copy_from_slice(&dec.as_wire());
        self.finish_append_fixed();
    }

    /// appendNameValue appends the value and then the name (the chunk form of an Enum or a
    /// Set).
    // go: Column.appendNameValue L1
    pub(crate) fn append_name_value(&mut self, name: &[u8], val: u64) {
        self.data.append(&val.to_le_bytes());
        self.data.append(name);
        self.finish_append_var();
    }

    /// The column form of `AppendEnum` / `AppendSet` until block D01.1 ports `types.Enum`
    /// and `types.Set`.
    // glue: Go Column.AppendEnum / AppendSet body, types.Enum/Set wait for D01.1
    pub fn append_name_value_for_test(&mut self, name: &[u8], val: u64) {
        self.append_name_value(name, val);
    }

    /// AppendJSON appends a BinaryJSON value into this Column.
    // go: Column.AppendJSON L1
    pub fn append_json(&mut self, j: &BinaryJSON<'_>) {
        self.data.append(&[j.type_code]);
        self.data.append(&j.value);
        self.finish_append_var();
    }

    /// AppendVectorFloat32 appends a VectorFloat32 value into this Column.
    // go: Column.AppendVectorFloat32 L1
    pub fn append_vector_float32(&mut self, v: &VectorFloat32<'_>) {
        // Go: c.data = v.SerializeTo(c.data), i.e. append(c.data, v.data...).
        self.data.append(v.zero_copy_serialize());
        self.finish_append_var();
    }
}

/// Column stores one column of data in Apache Arrow format.
/// See https://arrow.apache.org/docs/format/Columnar.html#format-columnar
#[derive(Clone, Debug)]
pub struct Column {
    pub(crate) length: usize,
    /// bit 0 is null, 1 is not null
    pub(crate) null_bitmap: GoSlice<1>,
    /// used for varLen column. Row i starts from data[offsets[i]]
    pub(crate) offsets: GoSlice<8>,
    pub(crate) data: GoSlice<1>,
    pub(crate) elem_buf: Option<ElemBuf>,
    /// The allocator of buffers this column allocates (N5a 3.7).
    pub(crate) alloc: ColAlloc,
}

/// See [`Column::govec_state`].
#[doc(hidden)]
#[derive(Debug)]
pub struct ColumnState<'a> {
    /// `length`.
    pub length: usize,
    /// `nullBitmap` and its capacity.
    pub null_bitmap: (&'a [u8], usize),
    /// `offsets` (little-endian bytes) and its capacity in elements.
    pub offsets: (&'a [u8], usize),
    /// `data` and its capacity.
    pub data: (&'a [u8], usize),
    /// `elemBuf` and its capacity, `None` when nil.
    pub elem_buf: Option<(&'a [u8], usize)>,
}

/// ColumnAllocator defines an allocator for Column.
pub trait ColumnAllocator {
    /// NewColumn creates a column for `ft` with room for `count` rows.
    // glue: Go interface method
    fn new_column(&mut self, ft: &FieldType, count: usize) -> Column;
}

/// DefaultColumnAllocator is the default implementation of ColumnAllocator.
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultColumnAllocator;

impl ColumnAllocator for DefaultColumnAllocator {
    /// NewColumn implements the ColumnAllocator interface.
    // go: DefaultColumnAllocator.NewColumn L1
    fn new_column(&mut self, ft: &FieldType, capacity: usize) -> Column {
        new_column(get_fixed_len(ft), capacity, ColAlloc::Global)
    }
}

/// NewEmptyColumn creates a new column with nothing.
// go: NewEmptyColumn L1
pub fn new_empty_column(ft: &FieldType) -> Column {
    let elem_len = get_fixed_len(ft);
    let mut col = Column::zero(ColAlloc::Global);
    if elem_len != VarElemLen {
        col.elem_buf = Some(ElemBuf::make(elem_len as usize));
    } else {
        col.offsets.push_i64(0);
    }
    col
}

/// NewColumn creates a new column with the specific type and capacity.
// go: NewColumn L1
pub fn new_column_for(ft: &FieldType, capacity: usize) -> Column {
    new_column(get_fixed_len(ft), capacity, ColAlloc::Global)
}

// go: newColumn L1
pub(crate) fn new_column(ts: isize, capacity: usize, alloc: ColAlloc) -> Column {
    if ts == VarElemLen {
        new_var_len_column(capacity, alloc)
    } else {
        new_fixed_len_column(ts as usize, capacity, alloc)
    }
}

/// newFixedLenColumn creates a fixed length Column with elemLen and initial data capacity.
// go: newFixedLenColumn L1
pub(crate) fn new_fixed_len_column(elem_len: usize, capacity: usize, alloc: ColAlloc) -> Column {
    Column {
        length: 0,
        elem_buf: Some(ElemBuf::make(elem_len)),
        data: GoSlice::make(0, get_data_mem_cap(capacity, elem_len), alloc.clone()),
        null_bitmap: GoSlice::make(0, get_null_bitmap_cap(capacity), alloc.clone()),
        offsets: GoSlice::nil(alloc.clone()),
        alloc,
    }
}

/// newVarLenColumn creates a variable length Column with initial data capacity.
// go: newVarLenColumn L1
pub(crate) fn new_var_len_column(capacity: usize, alloc: ColAlloc) -> Column {
    Column {
        length: 0,
        offsets: GoSlice::make(1, get_offsets_cap(capacity), alloc.clone()),
        data: GoSlice::make(
            0,
            get_data_mem_cap(estimatedElemLen, capacity),
            alloc.clone(),
        ),
        null_bitmap: GoSlice::make(0, get_null_bitmap_cap(capacity), alloc.clone()),
        elem_buf: None,
        alloc,
    }
}

// go: getDataMemCap L1
fn get_data_mem_cap(capacity: usize, elem_len: usize) -> usize {
    elem_len * capacity
}

// go: getNullBitmapCap L1
fn get_null_bitmap_cap(capacity: usize) -> usize {
    (capacity + 7) >> 3
}

// go: getOffsetsCap L1
fn get_offsets_cap(capacity: usize) -> usize {
    capacity + 1
}

/// The sizes of the fixed-length element types.
pub(crate) const sizeInt64: usize = 8;
pub(crate) const sizeUint64: usize = 8;
pub(crate) const sizeFloat32: usize = 4;
pub(crate) const sizeFloat64: usize = 8;
pub(crate) const sizeMyDecimal: usize = MY_DECIMAL_STRUCT_SIZE;
pub(crate) const sizeGoDuration: usize = 8;
pub(crate) const sizeTime: usize = 8;

impl Column {
    /// Go's zero `Column{}` (every slice nil).
    // glue: Go composite literal &Column{}
    pub(crate) fn zero(alloc: ColAlloc) -> Column {
        Column {
            length: 0,
            null_bitmap: GoSlice::nil(alloc.clone()),
            offsets: GoSlice::nil(alloc.clone()),
            data: GoSlice::nil(alloc.clone()),
            elem_buf: None,
            alloc,
        }
    }

    /// `&mut c.elemBuf[0]` as a byte slice (Go indexes it, so a nil or empty buffer panics).
    // glue: Go unsafe.Pointer(&c.elemBuf[0])
    #[inline(always)]
    fn elem_buf_mut(&mut self) -> &mut [u8] {
        let eb = self
            .elem_buf
            .as_mut()
            .filter(|e| e.len > 0)
            .expect("index out of range [0] with length 0");
        &mut eb.b
    }

    // go: Column.typeSize L1
    pub(crate) fn type_size(&self) -> isize {
        match &self.elem_buf {
            Some(e) if e.len > 0 => e.len as isize,
            _ => VarElemLen,
        }
    }

    // go: Column.isFixed L1
    #[inline]
    pub(crate) fn is_fixed(&self) -> bool {
        self.elem_buf.is_some()
    }

    /// The element length of a fixed-length column (Go `len(c.elemBuf)`).
    // glue: Go len(c.elemBuf)
    #[inline]
    pub(crate) fn elem_len(&self) -> usize {
        self.elem_buf.as_ref().map_or(0, |e| e.len)
    }

    /// Reset resets this Column according to the EvalType.
    /// Different from reset, Reset will reset the elemBuf.
    // go: Column.Reset L1
    pub fn reset_for(&mut self, e_type: EvalType) {
        match e_type {
            ETInt => self.resize_int64(0, false),
            ETReal => self.resize_float64(0, false),
            ETDecimal => self.resize_decimal(0, false),
            ETString => self.reserve_string(0),
            ETDatetime | ETTimestamp => self.resize_time(0, false),
            ETDuration => self.resize_go_duration(0, false),
            ETJson => self.reserve_json(0),
            ETVectorFloat32 => self.reserve_vector_float32(0),
            _ => panic!("invalid EvalType {}", e_type.0),
        }
    }

    /// Rows returns the row number in current column
    // go: Column.Rows L1
    pub fn rows(&self) -> usize {
        self.length
    }

    /// reset resets the underlying data of this Column but doesn't modify its data type.
    // go: Column.reset L1
    pub(crate) fn reset(&mut self) {
        self.length = 0;
        self.null_bitmap.reslice(0);
        if self.offsets.len() > 0 {
            // The first offset is always 0, it makes slicing the data easier, we need to keep it.
            self.offsets.reslice(1);
        } else if !self.is_fixed() {
            self.offsets.push_i64(0);
        }
        self.data.reslice(0);
    }

    /// IsNull returns if this row is null.
    // go: Column.IsNull L1
    #[inline]
    pub fn is_null(&self, row_idx: usize) -> bool {
        let null_byte = self.null_bitmap.at(row_idx / 8);
        null_byte & (1 << (row_idx & 7)) == 0
    }

    /// CopyConstruct copies this Column to dst.
    /// If dst is nil, it creates a new Column and returns it.
    // go: Column.CopyConstruct L1
    pub fn copy_construct(&self, dst: Option<Column>) -> Column {
        if let Some(mut dst) = dst {
            dst.length = self.length;
            dst.null_bitmap.reslice(0);
            dst.null_bitmap.append(self.null_bitmap.bytes());
            dst.offsets.reslice(0);
            dst.offsets.append(self.offsets.bytes());
            dst.data.reslice(0);
            dst.data.append(self.data.bytes());
            dst.elem_buf = elem_buf_copy(dst.elem_buf, self.elem_buf);
            return dst;
        }
        let mut new_col = Column::zero(self.alloc.clone());
        new_col.length = self.length;
        new_col.null_bitmap.append(self.null_bitmap.bytes());
        new_col.offsets.append(self.offsets.bytes());
        new_col.data.append(self.data.bytes());
        new_col.elem_buf = elem_buf_copy(None, self.elem_buf);
        new_col
    }

    /// AppendNullBitmap append a null/notnull value to the column's null map
    // go: Column.AppendNullBitmap L1
    pub fn append_null_bitmap(&mut self, not_null: bool) {
        self.append_null_bitmap_inner(not_null);
    }

    // go: Column.appendNullBitmap L1
    #[inline(always)]
    pub(crate) fn append_null_bitmap_inner(&mut self, not_null: bool) {
        let idx = self.length >> 3;
        if idx >= self.null_bitmap.len() {
            self.null_bitmap.append(&[0]);
        }
        if not_null {
            let pos = self.length & 7;
            self.null_bitmap.or_at(idx, 1 << pos);
        }
    }

    /// AppendCellNTimes append the pos-th Cell in source column to target column N times
    // go: Column.AppendCellNTimes L1
    pub fn append_cell_n_times(&mut self, src: &Column, pos: usize, times: usize) {
        let not_null = !src.is_null(pos);
        if times == 1 {
            self.append_null_bitmap_inner(not_null);
        } else {
            self.append_multi_same_null_bitmap(not_null, times);
        }
        if self.is_fixed() {
            let elem_len = src.elem_len();
            let offset = pos * elem_len;
            for _ in 0..times {
                self.data
                    .append(&src.data.capb()[offset..offset + elem_len]);
            }
        } else {
            let (start, end) = (src.offsets.i64_at(pos), src.offsets.i64_at(pos + 1));
            for _ in 0..times {
                self.data
                    .append(&src.data.capb()[start as usize..end as usize]);
                self.offsets.push_i64(self.data.len() as i64);
            }
        }
        self.length += times;
    }

    /// appendMultiSameNullBitmap appends multiple same bit value to `nullBitMap`.
    /// notNull means not null.
    /// num means the number of bits that should be appended.
    // go: Column.appendMultiSameNullBitmap L1
    pub(crate) fn append_multi_same_null_bitmap(&mut self, not_null: bool, num: usize) {
        let num_new_bytes =
            ((self.length + num + 7) >> 3) as isize - self.null_bitmap.len() as isize;
        let b = if not_null { 0xff } else { 0 };
        if num_new_bytes > 0 {
            self.null_bitmap.append_fill(num_new_bytes as usize, b);
        }
        if !not_null {
            return;
        }
        // 1. Set all the remaining bits in the last slot of old c.numBitMap to 1.
        let num_remaining_bits = (self.length % 8) as u32;
        let bit_mask = !((1u8 << num_remaining_bits).wrapping_sub(1));
        *self.null_bitmap.at_mut(self.length / 8) |= bit_mask;
        // 2. Set all the redundant bits in the last slot of new c.numBitMap to 0.
        let num_redundant_bits = (self.null_bitmap.len() * 8 - self.length - num) as u32;
        // Go: byte(1<<(8-numRedundantBits)) - 1, computed in byte arithmetic.
        let bit_mask = (1u32 << (8 - num_redundant_bits)) as u8;
        let bit_mask = bit_mask.wrapping_sub(1);
        let last = self.null_bitmap.len() - 1;
        *self.null_bitmap.at_mut(last) &= bit_mask;
    }

    /// AppendNNulls append n nulls to the column
    // go: Column.AppendNNulls L1
    pub fn append_n_nulls(&mut self, n: usize) {
        self.append_multi_same_null_bitmap(false, n);
        if self.is_fixed() {
            let eb = self.elem_buf.unwrap();
            for _ in 0..n {
                self.data.append(eb.bytes());
            }
        } else {
            let current_length = self.offsets.i64_at(self.length);
            for _ in 0..n {
                self.offsets.push_i64(current_length);
            }
        }
        self.length += n;
    }

    /// AppendNull appends a null value into this Column.
    // go: Column.AppendNull L1
    pub fn append_null(&mut self) {
        self.append_null_bitmap_inner(false);
        if let Some(eb) = &self.elem_buf {
            self.data.append(eb.bytes());
        } else {
            let o = self.offsets.i64_at(self.length);
            self.offsets.push_i64(o);
        }
        self.length += 1;
    }

    // go: Column.finishAppendFixed L1
    #[inline]
    pub(crate) fn finish_append_fixed(&mut self) {
        let eb = self.elem_buf.as_ref().unwrap();
        self.data.append(&eb.b[..eb.len]);
        self.append_null_bitmap_inner(true);
        self.length += 1;
    }

    /// AppendInt64 appends an int64 value into this Column.
    // go: Column.AppendInt64 L1
    #[inline]
    pub fn append_int64(&mut self, i: i64) {
        self.elem_buf_mut()[..8].copy_from_slice(&i.to_le_bytes());
        self.finish_append_fixed();
    }

    /// AppendUint64 appends a uint64 value into this Column.
    // go: Column.AppendUint64 L1
    #[inline]
    pub fn append_uint64(&mut self, u: u64) {
        self.elem_buf_mut()[..8].copy_from_slice(&u.to_le_bytes());
        self.finish_append_fixed();
    }

    /// AppendFloat32 appends a float32 value into this Column.
    // go: Column.AppendFloat32 L1
    #[inline]
    pub fn append_float32(&mut self, f: f32) {
        self.elem_buf_mut()[..4].copy_from_slice(&f.to_le_bytes());
        self.finish_append_fixed();
    }

    /// AppendFloat64 appends a float64 value into this Column.
    // go: Column.AppendFloat64 L1
    #[inline]
    pub fn append_float64(&mut self, f: f64) {
        self.elem_buf_mut()[..8].copy_from_slice(&f.to_le_bytes());
        self.finish_append_fixed();
    }

    // go: Column.finishAppendVar L1
    #[inline]
    pub(crate) fn finish_append_var(&mut self) {
        self.append_null_bitmap_inner(true);
        self.offsets.push_i64(self.data.len() as i64);
        self.length += 1;
    }

    /// AppendString appends a string value into this Column.
    // go: Column.AppendString L1
    #[inline]
    pub fn append_string(&mut self, str: &[u8]) {
        self.data.append(str);
        self.finish_append_var();
    }

    /// AppendBytes appends a byte slice into this Column.
    // go: Column.AppendBytes L1
    #[inline]
    pub fn append_bytes(&mut self, b: &[u8]) {
        self.data.append(b);
        self.finish_append_var();
    }

    /// AppendTime appends a time value into this Column.
    // go: Column.AppendTime L1
    #[inline]
    pub fn append_time(&mut self, t: Time) {
        self.elem_buf_mut()[..8].copy_from_slice(&t.raw().0.to_le_bytes());
        self.finish_append_fixed();
    }

    /// resize resizes the column so that it contains n elements, only valid for fixed-length types.
    // go: Column.resize L1
    fn resize(&mut self, n: usize, type_size: usize, is_null: bool) {
        let size_data = n * type_size;
        if self.data.cap() >= size_data {
            self.data.reslice(size_data);
        } else {
            self.data = GoSlice::make(size_data, size_data, self.alloc.clone());
        }
        if !is_null {
            // Go copies a 4 KiB zero buffer over the data.
            self.data.bytes_mut().fill(0);
        }

        let mut new_nulls = false;
        let size_nulls = (n + 7) >> 3;
        if self.null_bitmap.cap() >= size_nulls {
            self.null_bitmap.reslice(size_nulls);
        } else {
            self.null_bitmap = GoSlice::make(size_nulls, size_nulls, self.alloc.clone());
            new_nulls = true;
        }
        if !is_null || !new_nulls {
            let null_val = if is_null { 0 } else { 0xFF };

            // Fill the null bitmap
            self.null_bitmap.bytes_mut().fill(null_val);
            // Revise the last byte if necessary, when it's not divided by 8.
            let x = n % 8;
            if x != 0 && !is_null {
                let last_byte = ((1u32 << x) - 1) as u8;
                if self.null_bitmap.len() > 0 {
                    let l = self.null_bitmap.len() - 1;
                    *self.null_bitmap.at_mut(l) = last_byte;
                }
            }
        }

        match &mut self.elem_buf {
            Some(e) if e.cap >= type_size => e.len = type_size,
            _ => self.elem_buf = Some(ElemBuf::make(type_size)),
        }

        self.length = n;
    }

    /// reserve makes the column capacity be at least enough to contain n elements.
    /// this method is only valid for var-length types and estElemSize is the estimated size of this type.
    // go: Column.reserve L1
    fn reserve(&mut self, n: usize, est_elem_size: usize) {
        let size_data = n * est_elem_size;
        if self.data.cap() >= size_data {
            self.data.reslice(0);
        } else {
            self.data = GoSlice::make(0, size_data, self.alloc.clone());
        }

        let size_nulls = (n + 7) >> 3;
        if self.null_bitmap.cap() >= size_nulls {
            self.null_bitmap.reslice(0);
        } else {
            self.null_bitmap = GoSlice::make(0, size_nulls, self.alloc.clone());
        }

        let size_offs = n + 1;
        if self.offsets.cap() >= size_offs {
            self.offsets.reslice(1);
        } else {
            self.offsets = GoSlice::make(1, size_offs, self.alloc.clone());
        }

        self.elem_buf = None;
        self.length = 0;
    }

    /// SetNull sets the rowIdx to null.
    // go: Column.SetNull L1
    #[inline]
    pub fn set_null(&mut self, row_idx: usize, is_null: bool) {
        let b = self.null_bitmap.at_mut(row_idx >> 3);
        if is_null {
            *b &= !(1 << (row_idx & 7));
        } else {
            *b |= 1 << (row_idx & 7);
        }
    }

    /// SetNulls sets rows in [begin, end) to null.
    // go: Column.SetNulls L1
    pub fn set_nulls(&mut self, mut begin: usize, end: usize, is_null: bool) {
        let i = ((begin + 7) >> 3) << 3;
        while begin < i && begin < end {
            self.set_null(begin, is_null);
            begin += 1;
        }
        let v: u8 = if is_null { 0 } else { 0xff };
        while begin + 8 <= end {
            *self.null_bitmap.at_mut(begin >> 3) = v;
            begin += 8;
        }
        while begin < end {
            self.set_null(begin, is_null);
            begin += 1;
        }
    }

    /// nullCount returns the number of nulls in this Column.
    // go: Column.nullCount L1
    pub(crate) fn null_count(&self) -> usize {
        let mut cnt = 0;
        let mut i = 0;
        let nb = self.null_bitmap.bytes();
        while i + 8 <= self.length {
            // 0 is null and 1 is not null
            cnt += 8 - nb[i >> 3].count_ones() as usize;
            i += 8;
        }
        while i < self.length {
            if self.is_null(i) {
                cnt += 1;
            }
            i += 1;
        }
        cnt
    }

    /// ResizeInt64 resizes the column so that it contains n int64 elements.
    // go: Column.ResizeInt64 L1
    pub fn resize_int64(&mut self, n: usize, is_null: bool) {
        self.resize(n, sizeInt64, is_null);
    }

    /// ResizeUint64 resizes the column so that it contains n uint64 elements.
    // go: Column.ResizeUint64 L1
    pub fn resize_uint64(&mut self, n: usize, is_null: bool) {
        self.resize(n, sizeUint64, is_null);
    }

    /// ResizeFloat32 resizes the column so that it contains n float32 elements.
    // go: Column.ResizeFloat32 L1
    pub fn resize_float32(&mut self, n: usize, is_null: bool) {
        self.resize(n, sizeFloat32, is_null);
    }

    /// ResizeFloat64 resizes the column so that it contains n float64 elements.
    // go: Column.ResizeFloat64 L1
    pub fn resize_float64(&mut self, n: usize, is_null: bool) {
        self.resize(n, sizeFloat64, is_null);
    }

    /// ResizeDecimal resizes the column so that it contains n decimal elements.
    // go: Column.ResizeDecimal L1
    pub fn resize_decimal(&mut self, n: usize, is_null: bool) {
        self.resize(n, sizeMyDecimal, is_null);
    }

    /// ResizeGoDuration resizes the column so that it contains n duration elements.
    // go: Column.ResizeGoDuration L1
    pub fn resize_go_duration(&mut self, n: usize, is_null: bool) {
        self.resize(n, sizeGoDuration, is_null);
    }

    /// ResizeTime resizes the column so that it contains n Time elements.
    // go: Column.ResizeTime L1
    pub fn resize_time(&mut self, n: usize, is_null: bool) {
        self.resize(n, sizeTime, is_null);
    }

    /// ReserveString changes the column capacity to store n string elements and set the length to zero.
    // go: Column.ReserveString L1
    pub fn reserve_string(&mut self, n: usize) {
        self.reserve(n, 8);
    }

    /// ReserveBytes changes the column capacity to store n bytes elements and set the length to zero.
    // go: Column.ReserveBytes L1
    pub fn reserve_bytes(&mut self, n: usize) {
        self.reserve(n, 8);
    }

    /// ReserveJSON changes the column capacity to store n JSON elements and set the length to zero.
    // go: Column.ReserveJSON L1
    pub fn reserve_json(&mut self, n: usize) {
        self.reserve(n, 8);
    }

    /// ReserveVectorFloat32 changes the column capacity to store n vectorFloat32 elements and set the length to zero.
    // go: Column.ReserveVectorFloat32 L1
    pub fn reserve_vector_float32(&mut self, n: usize) {
        self.reserve(n, 8);
    }

    /// ReserveSet changes the column capacity to store n set elements and set the length to zero.
    // go: Column.ReserveSet L1
    pub fn reserve_set(&mut self, n: usize) {
        self.reserve(n, 8);
    }

    /// ReserveEnum changes the column capacity to store n enum elements and set the length to zero.
    // go: Column.ReserveEnum L1
    pub fn reserve_enum(&mut self, n: usize) {
        self.reserve(n, 8);
    }

    /// The data bytes of the first `c.length` elements (Go's slice header cast keeps the
    /// pointer, sets the length to `c.length`).
    // go: Column.castSliceHeader L2 (N5a 3.3: 1-byte aligned views instead of a slice header cast)
    fn cast_slice_header(&self, type_size: usize) -> &[u8] {
        &self.data.capb()[..self.length * type_size]
    }

    // glue: mutable counterpart of castSliceHeader (N5a 3.5: result columns)
    fn cast_slice_header_mut(&mut self, type_size: usize) -> &mut [u8] {
        let n = self.length * type_size;
        &mut self.data.capb_mut()[..n]
    }

    /// Int64s returns an int64 slice stored in this Column.
    // go: Column.Int64s L2 (N5a 3.3: little-endian 1-byte aligned view)
    pub fn int64s(&self) -> &[I64] {
        <[I64]>::ref_from_bytes(self.cast_slice_header(sizeInt64)).unwrap()
    }

    /// Int64s as a mutable view (N5a 3.5).
    // glue: N5a 3.5 result column view
    pub fn int64s_mut(&mut self) -> &mut [I64] {
        <[I64]>::mut_from_bytes(self.cast_slice_header_mut(sizeInt64)).unwrap()
    }

    /// Uint64s returns a uint64 slice stored in this Column.
    // go: Column.Uint64s L2 (N5a 3.3: little-endian 1-byte aligned view)
    pub fn uint64s(&self) -> &[U64] {
        <[U64]>::ref_from_bytes(self.cast_slice_header(sizeUint64)).unwrap()
    }

    /// Uint64s as a mutable view (N5a 3.5).
    // glue: N5a 3.5 result column view
    pub fn uint64s_mut(&mut self) -> &mut [U64] {
        <[U64]>::mut_from_bytes(self.cast_slice_header_mut(sizeUint64)).unwrap()
    }

    /// Float32s returns a float32 slice stored in this Column.
    // go: Column.Float32s L2 (N5a 3.3: little-endian 1-byte aligned view)
    pub fn float32s(&self) -> &[F32] {
        <[F32]>::ref_from_bytes(self.cast_slice_header(sizeFloat32)).unwrap()
    }

    /// Float32s as a mutable view (N5a 3.5).
    // glue: N5a 3.5 result column view
    pub fn float32s_mut(&mut self) -> &mut [F32] {
        <[F32]>::mut_from_bytes(self.cast_slice_header_mut(sizeFloat32)).unwrap()
    }

    /// Float64s returns a float64 slice stored in this Column.
    // go: Column.Float64s L2 (N5a 3.3: little-endian 1-byte aligned view)
    pub fn float64s(&self) -> &[F64] {
        <[F64]>::ref_from_bytes(self.cast_slice_header(sizeFloat64)).unwrap()
    }

    /// Float64s as a mutable view (N5a 3.5).
    // glue: N5a 3.5 result column view
    pub fn float64s_mut(&mut self) -> &mut [F64] {
        <[F64]>::mut_from_bytes(self.cast_slice_header_mut(sizeFloat64)).unwrap()
    }

    /// GoDurations returns a Golang time.Duration slice stored in this Column.
    /// Different from the Row.GetDuration method, the argument Fsp is ignored, so the user should handle it outside.
    // go: Column.GoDurations L2 (N5a 3.3: little-endian 1-byte aligned view)
    pub fn go_durations(&self) -> &[I64] {
        <[I64]>::ref_from_bytes(self.cast_slice_header(sizeGoDuration)).unwrap()
    }

    /// GoDurations as a mutable view (N5a 3.5).
    // glue: N5a 3.5 result column view
    pub fn go_durations_mut(&mut self) -> &mut [I64] {
        <[I64]>::mut_from_bytes(self.cast_slice_header_mut(sizeGoDuration)).unwrap()
    }

    /// Decimals returns a MyDecimal slice stored in this Column.
    // go: Column.Decimals L2 (N5a 3.3: 40-byte images, read with MyDecimal::from_wire)
    pub fn decimals(&self) -> &[MyDecimalBytes] {
        <[MyDecimalBytes]>::ref_from_bytes(self.cast_slice_header(sizeMyDecimal)).unwrap()
    }

    /// Decimals as a mutable view (N5a 3.5).
    // glue: N5a 3.5 result column view
    pub fn decimals_mut(&mut self) -> &mut [MyDecimalBytes] {
        <[MyDecimalBytes]>::mut_from_bytes(self.cast_slice_header_mut(sizeMyDecimal)).unwrap()
    }

    /// Times returns a Time slice stored in this Column.
    // go: Column.Times L2 (N5a 3.3: the raw words, read with Time::from_raw)
    pub fn times(&self) -> &[U64] {
        <[U64]>::ref_from_bytes(self.cast_slice_header(sizeTime)).unwrap()
    }

    /// Times as a mutable view (N5a 3.5).
    // glue: N5a 3.5 result column view
    pub fn times_mut(&mut self) -> &mut [U64] {
        <[U64]>::mut_from_bytes(self.cast_slice_header_mut(sizeTime)).unwrap()
    }

    /// `*(*[N]byte)(unsafe.Pointer(&c.data[off]))`: Go checks `off < len(c.data)` (the
    /// index expression) and reads `N` bytes from there.
    // glue: Go unsafe read of an element at &c.data[off]
    #[inline]
    fn elem<const N: usize>(&self, off: usize) -> [u8; N] {
        assert!(
            off < self.data.len(),
            "index out of range [{off}] with length {}",
            self.data.len()
        );
        self.data.capb()[off..off + N].try_into().unwrap()
    }

    // glue: Go *(*[8]byte)(unsafe.Pointer(&c.data[off]))
    #[inline]
    fn word8(&self, off: usize) -> [u8; 8] {
        self.elem::<8>(off)
    }

    /// GetInt64 returns the int64 in the specific row.
    // go: Column.GetInt64 L1
    #[inline]
    pub fn get_int64(&self, row_id: usize) -> i64 {
        i64::from_le_bytes(self.word8(row_id * 8))
    }

    /// GetUint64 returns the uint64 in the specific row.
    // go: Column.GetUint64 L1
    #[inline]
    pub fn get_uint64(&self, row_id: usize) -> u64 {
        u64::from_le_bytes(self.word8(row_id * 8))
    }

    /// GetFloat32 returns the float32 in the specific row.
    // go: Column.GetFloat32 L1
    #[inline]
    pub fn get_float32(&self, row_id: usize) -> f32 {
        f32::from_le_bytes(self.elem::<4>(row_id * 4))
    }

    /// GetFloat64 returns the float64 in the specific row.
    // go: Column.GetFloat64 L1
    #[inline]
    pub fn get_float64(&self, row_id: usize) -> f64 {
        f64::from_le_bytes(self.word8(row_id * 8))
    }

    /// GetDecimal returns the decimal in the specific row.
    ///
    /// Go returns a pointer into the column; this returns a copy (write back with
    /// [`Column::decimals_mut`]). A `negative` byte other than 0 or 1 reads as true, as in
    /// gc.
    // go: Column.GetDecimal L2 (returns the value, not a pointer into the column)
    pub fn get_decimal(&self, row_id: usize) -> MyDecimal {
        let b: MyDecimalBytes =
            self.elem::<MY_DECIMAL_STRUCT_SIZE>(row_id * MY_DECIMAL_STRUCT_SIZE);
        MyDecimal::from_wire(&b).expect("invalid MyDecimal image in chunk column")
    }

    /// GetString returns the string in the specific row.
    // go: Column.GetString L1
    #[inline]
    pub fn get_string(&self, row_id: usize) -> &[u8] {
        self.get_bytes(row_id)
    }

    /// GetJSON returns the JSON in the specific row.
    // go: Column.GetJSON L1
    pub fn get_json(&self, row_id: usize) -> BinaryJSON<'_> {
        let start = self.offsets.i64_at(row_id) as usize;
        let end = self.offsets.i64_at(row_id + 1) as usize;
        let type_code = self.data.at(start);
        BinaryJSON {
            type_code,
            value: Cow::Borrowed(&self.data.capb()[start + 1..end]),
        }
    }

    /// GetVectorFloat32 returns the VectorFloat32 in the specific row.
    // go: Column.GetVectorFloat32 L1
    pub fn get_vector_float32(&self, row_id: usize) -> VectorFloat32<'_> {
        let data = self.get_bytes(row_id);
        match zero_copy_deserialize_vector_float32(data) {
            Ok((v, _)) => v,
            Err(err) => panic!("{err}"),
        }
    }

    /// GetBytes returns the byte slice in the specific row.
    // go: Column.GetBytes L1
    #[inline]
    pub fn get_bytes(&self, row_id: usize) -> &[u8] {
        let start = self.offsets.i64_at(row_id) as usize;
        let end = self.offsets.i64_at(row_id + 1) as usize;
        &self.data.capb()[start..end]
    }

    /// GetTime returns the Time in the specific row.
    // go: Column.GetTime L1
    #[inline]
    pub fn get_time(&self, row_id: usize) -> Time {
        Time::from_raw(CoreTime(u64::from_le_bytes(self.word8(row_id * sizeTime))))
    }

    /// GetDuration returns the Duration in the specific row.
    // go: Column.GetDuration L1
    #[inline]
    pub fn get_duration(&self, row_id: usize, fill_fsp: isize) -> Duration {
        let dur = i64::from_le_bytes(self.word8(row_id * 8));
        Duration {
            duration: dur,
            fsp: fill_fsp,
        }
    }

    // go: Column.getNameValue L1
    pub(crate) fn get_name_value(&self, row_id: usize) -> (&[u8], u64) {
        let start = self.offsets.i64_at(row_id) as usize;
        let end = self.offsets.i64_at(row_id + 1) as usize;
        if start == end {
            return (&[], 0);
        }
        let d = self.data.bytes();
        // Go: copy((*[8]byte)(unsafe.Pointer(&val))[:], c.data[start:]) copies at most 8 bytes.
        let mut v = [0u8; 8];
        let n = (d.len() - start).min(8);
        v[..n].copy_from_slice(&d[start..start + n]);
        (&self.data.capb()[start + 8..end], u64::from_le_bytes(v))
    }

    /// GetRaw returns the underlying raw bytes in the specific row.
    // go: Column.GetRaw L1
    pub fn get_raw(&self, row_id: usize) -> &[u8] {
        if self.is_fixed() {
            let elem_len = self.elem_len();
            &self.data.capb()[row_id * elem_len..row_id * elem_len + elem_len]
        } else {
            self.get_bytes(row_id)
        }
    }

    /// SetRaw sets the raw bytes for the rowIdx-th element.
    /// NOTE: Two conditions must be satisfied before calling this function:
    /// 1. The column should be stored with variable-length elements.
    /// 2. The length of the new element should be exactly the same as the old one.
    // go: Column.SetRaw L1
    pub fn set_raw(&mut self, row_id: usize, bs: &[u8]) {
        let start = self.offsets.i64_at(row_id) as usize;
        let end = self.offsets.i64_at(row_id + 1) as usize;
        let dst = &mut self.data.capb_mut()[start..end];
        let n = dst.len().min(bs.len());
        dst[..n].copy_from_slice(&bs[..n]);
    }

    /// reconstruct reconstructs this Column by removing all filtered rows in it according to sel.
    // go: Column.reconstruct L1
    pub(crate) fn reconstruct(&mut self, sel: Option<&[usize]>) {
        let Some(sel) = sel else {
            return;
        };
        if self.is_fixed() {
            let elem_len = self.elem_len();
            for (dst, &src) in sel.iter().enumerate() {
                let idx = dst >> 3;
                let pos = dst & 7;
                if self.is_null(src) {
                    *self.null_bitmap.at_mut(idx) &= !(1u8 << pos);
                } else {
                    self.data
                        .capb_mut()
                        .copy_within(src * elem_len..src * elem_len + elem_len, dst * elem_len);
                    *self.null_bitmap.at_mut(idx) |= 1u8 << pos;
                }
            }
            self.data.reslice(sel.len() * elem_len);
        } else {
            let mut tail = 0usize;
            for (dst, &src) in sel.iter().enumerate() {
                let idx = dst >> 3;
                let pos = dst & 7;
                if self.is_null(src) {
                    *self.null_bitmap.at_mut(idx) &= !(1u8 << pos);
                    self.offsets.set_i64(dst + 1, tail as i64);
                } else {
                    let (start, end) = (
                        self.offsets.i64_at(src) as usize,
                        self.offsets.i64_at(src + 1) as usize,
                    );
                    self.data.capb_mut().copy_within(start..end, tail);
                    tail += end - start;
                    self.offsets.set_i64(dst + 1, tail as i64);
                    *self.null_bitmap.at_mut(idx) |= 1u8 << pos;
                }
            }
            self.data.reslice(tail);
            self.offsets.reslice(sel.len() + 1);
        }
        self.length = sel.len();

        // clean nullBitmap
        self.null_bitmap.reslice((sel.len() + 7) >> 3);
        let idx = sel.len() >> 3;
        if idx < self.null_bitmap.len() {
            let pos = sel.len() & 7;
            *self.null_bitmap.at_mut(idx) &= ((1u32 << pos) - 1) as u8;
        }
    }

    /// CopyReconstruct copies this Column to dst and removes unselected rows.
    /// If dst is nil, it creates a new Column and returns it.
    // go: Column.CopyReconstruct L1
    pub fn copy_reconstruct(&self, sel: Option<&[usize]>, dst: Option<Column>) -> Column {
        let Some(sel) = sel else {
            return self.copy_construct(dst);
        };

        let sel_length = sel.len();
        if sel_length == self.length {
            // The variable 'ascend' is used to check if the sel array is in ascending order
            let ascend = sel.windows(2).all(|w| w[1] >= w[0]);
            if ascend {
                return self.copy_construct(dst);
            }
        }

        let mut dst = match dst {
            None => new_column(self.type_size(), sel.len(), self.alloc.clone()),
            Some(mut d) => {
                d.reset();
                d
            }
        };

        if self.is_fixed() {
            let elem_len = self.elem_len();
            dst.elem_buf = Some(ElemBuf::make(elem_len));
            for &i in sel {
                dst.append_null_bitmap_inner(!self.is_null(i));
                dst.data
                    .append(&self.data.capb()[i * elem_len..i * elem_len + elem_len]);
                dst.length += 1;
            }
        } else {
            dst.elem_buf = None;
            if dst.offsets.len() == 0 {
                dst.offsets.push_i64(0);
            }
            for &i in sel {
                dst.append_null_bitmap_inner(!self.is_null(i));
                let (start, end) = (self.offsets.i64_at(i), self.offsets.i64_at(i + 1));
                dst.data
                    .append(&self.data.capb()[start as usize..end as usize]);
                dst.offsets.push_i64(dst.data.len() as i64);
                dst.length += 1;
            }
        }
        dst
    }

    /// MergeNulls merges these columns' null bitmaps.
    /// For a row, if any column of it is null, the result is null.
    /// It works like: if col1.IsNull || col2.IsNull || col3.IsNull.
    /// The caller should ensure that all these columns have the same
    /// length, and data stored in the result column is fixed-length type.
    // go: Column.MergeNulls L1
    pub fn merge_nulls(&mut self, cols: &[&Column]) {
        if !self.is_fixed() {
            panic!("result column should be fixed-length type");
        }
        for col in cols {
            if self.length != col.length {
                panic!(
                    "should ensure all columns have the same length, expect {}, but got {}",
                    self.length, col.length
                );
            }
        }
        for col in cols {
            let src = col.null_bitmap.bytes();
            let dst = self.null_bitmap.bytes_mut();
            for i in 0..dst.len() {
                // bit 0 is null, 1 is not null, so do AND operations here.
                dst[i] &= src[i];
            }
        }
    }

    /// DestroyDataForTest destroies data in the column so that
    /// we can test if data in column are deeply copied.
    // go: Column.DestroyDataForTest L2 (a fixed pseudo-random sequence instead of math/rand)
    pub fn destroy_data_for_test(&mut self) {
        let mut x: u32 = 0x9e37_79b9;
        for b in self.data.bytes_mut() {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            *b = x as u8;
        }
    }

    /// The bytes of the data buffer (diagnostics and tests).
    // glue: test and benchmark access to c.data
    pub fn data_bytes(&self) -> &[u8] {
        self.data.bytes()
    }

    /// The bytes of the null bitmap (diagnostics and tests).
    // glue: test access to c.nullBitmap
    pub fn null_bitmap_bytes(&self) -> &[u8] {
        self.null_bitmap.bytes()
    }

    /// Whether any buffer is a slice of a shared response buffer (N5a 3.4).
    // glue: N5a 3.4
    pub fn is_shared(&self) -> bool {
        self.data.is_shared() || self.offsets.is_shared() || self.null_bitmap.is_shared()
    }

    /// The column's fields as Go sees them, for the govec replay: the length, then each of
    /// `nullBitmap`, `offsets` (little-endian bytes), `data` with its capacity, and `elemBuf`
    /// (`None` when nil) with its capacity.
    // glue: govec replay access to the unexported fields
    #[doc(hidden)]
    pub fn govec_state(&self) -> ColumnState<'_> {
        ColumnState {
            length: self.length,
            null_bitmap: (self.null_bitmap.bytes(), self.null_bitmap.cap()),
            offsets: (self.offsets.bytes(), self.offsets.cap()),
            data: (self.data.bytes(), self.data.cap()),
            elem_buf: self.elem_buf.as_ref().map(|e| (e.bytes(), e.cap())),
        }
    }

    /// Go's `cap()` of the three buffers and of `elemBuf`, as `MemoryUsage` sums them.
    // glue: Go cap(col.nullBitmap), cap(col.offsets)*8, cap(col.data), cap(col.elemBuf)
    pub(crate) fn caps(&self) -> (usize, usize, usize, usize) {
        (
            self.null_bitmap.cap(),
            self.offsets.cap(),
            self.data.cap(),
            self.elem_buf.as_ref().map_or(0, |e| e.cap),
        )
    }
}
