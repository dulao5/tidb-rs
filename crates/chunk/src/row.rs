//! go-file: pkg/util/chunk/row.go
//!
//! `Row` (N5a 3.1: L1). Go's `Row{c *Chunk, idx int}` is a `Copy` borrow here,
//! `Row<'a>{c: Option<&'a Chunk>, idx}`; the zero `Row{}` (the iterators' end marker) has no
//! chunk. A row kept across `Next` calls is a `RowHandle` (mapping rule `M-ROW`, added with
//! the first operator that needs it).
//!
//! `GetEnum`, `GetSet`, `GetDatumRow`, `GetDatumRowWithBuffer`, `GetDatum`,
//! `DatumWithBuffer` and `ToString` need block D01.1 (`types.Enum`, `types.Set`,
//! `types.Datum`, `Enum.String`) and wait for it.

// Go names for package-level constants (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use tidb_types::MyDecimal;
use tidb_types::json::BinaryJSON;
use tidb_types::time::Duration;
use tidb_types::time::Time;
use tidb_types::vector::VectorFloat32;

use crate::chunk::Chunk;
use crate::chunk::renew_with_capacity;

/// RowSize is the size of `Row{}` (Go's `unsafe.Sizeof(Row{})` on 64-bit platforms, used for
/// memory accounting).
pub const RowSize: i64 = 16;

/// Row represents a row of data, can be used to access values.
#[derive(Clone, Copy, Debug, Default)]
pub struct Row<'a> {
    c: Option<&'a Chunk>,
    idx: usize,
}

impl PartialEq for Row<'_> {
    /// Go compares the chunk pointer and the index.
    // glue: Go struct == on Row
    fn eq(&self, other: &Self) -> bool {
        self.idx == other.idx
            && match (self.c, other.c) {
                (None, None) => true,
                (Some(a), Some(b)) => std::ptr::eq(a, b),
                _ => false,
            }
    }
}

impl Eq for Row<'_> {}

impl<'a> Row<'a> {
    /// `Row{c: c, idx: idx}`.
    // glue: Go composite literal Row{c, idx}
    #[inline]
    pub(crate) fn new(c: &'a Chunk, idx: usize) -> Row<'a> {
        Row { c: Some(c), idx }
    }

    /// Chunk returns the Chunk which the row belongs to.
    // go: Row.Chunk L1
    #[inline]
    pub fn chunk(&self) -> &'a Chunk {
        self.c
            .expect("invalid memory address or nil pointer dereference")
    }

    /// IsEmpty returns true if the Row is empty.
    // go: Row.IsEmpty L1
    pub fn is_empty(&self) -> bool {
        *self == Row::default()
    }

    /// Idx returns the row index of Chunk.
    // go: Row.Idx L1
    #[inline]
    pub fn idx(&self) -> usize {
        self.idx
    }

    /// Len returns the number of values in the row.
    // go: Row.Len L1
    pub fn len(&self) -> usize {
        self.chunk().num_cols()
    }

    /// GetInt64 returns the int64 value with the colIdx.
    // go: Row.GetInt64 L1
    #[inline]
    pub fn get_int64(&self, col_idx: usize) -> i64 {
        self.chunk().column(col_idx).get_int64(self.idx)
    }

    /// GetUint64 returns the uint64 value with the colIdx.
    // go: Row.GetUint64 L1
    #[inline]
    pub fn get_uint64(&self, col_idx: usize) -> u64 {
        self.chunk().column(col_idx).get_uint64(self.idx)
    }

    /// GetFloat32 returns the float32 value with the colIdx.
    // go: Row.GetFloat32 L1
    #[inline]
    pub fn get_float32(&self, col_idx: usize) -> f32 {
        self.chunk().column(col_idx).get_float32(self.idx)
    }

    /// GetFloat64 returns the float64 value with the colIdx.
    // go: Row.GetFloat64 L1
    #[inline]
    pub fn get_float64(&self, col_idx: usize) -> f64 {
        self.chunk().column(col_idx).get_float64(self.idx)
    }

    /// GetString returns the string value with the colIdx.
    // go: Row.GetString L1
    #[inline]
    pub fn get_string(&self, col_idx: usize) -> &'a [u8] {
        self.chunk().column(col_idx).get_string(self.idx)
    }

    /// GetBytes returns the bytes value with the colIdx.
    // go: Row.GetBytes L1
    #[inline]
    pub fn get_bytes(&self, col_idx: usize) -> &'a [u8] {
        self.chunk().column(col_idx).get_bytes(self.idx)
    }

    /// GetTime returns the Time value with the colIdx.
    // go: Row.GetTime L1
    #[inline]
    pub fn get_time(&self, col_idx: usize) -> Time {
        self.chunk().column(col_idx).get_time(self.idx)
    }

    /// GetDuration returns the Duration value with the colIdx.
    // go: Row.GetDuration L1
    #[inline]
    pub fn get_duration(&self, col_idx: usize, fill_fsp: isize) -> Duration {
        self.chunk()
            .column(col_idx)
            .get_duration(self.idx, fill_fsp)
    }

    // go: Row.getNameValue L1
    pub(crate) fn get_name_value(&self, col_idx: usize) -> (&'a [u8], u64) {
        self.chunk().column(col_idx).get_name_value(self.idx)
    }

    /// The `(name, value)` of an Enum or Set cell until block D01.1 ports `types.Enum` and
    /// `types.Set` (Go: `GetEnum` / `GetSet`).
    // glue: Go GetEnum / GetSet body, types.Enum/Set wait for D01.1
    pub fn get_name_value_pub(&self, col_idx: usize) -> (&'a [u8], u64) {
        self.get_name_value(col_idx)
    }

    /// GetMyDecimal returns the MyDecimal value with the colIdx.
    // go: Row.GetMyDecimal L2 (returns the value, see Column.GetDecimal)
    #[inline]
    pub fn get_my_decimal(&self, col_idx: usize) -> MyDecimal {
        self.chunk().column(col_idx).get_decimal(self.idx)
    }

    /// GetJSON returns the JSON value with the colIdx.
    // go: Row.GetJSON L1
    pub fn get_json(&self, col_idx: usize) -> BinaryJSON<'a> {
        self.chunk().column(col_idx).get_json(self.idx)
    }

    /// GetVectorFloat32 returns the VectorFloat32 value with the colIdx.
    // go: Row.GetVectorFloat32 L1
    pub fn get_vector_float32(&self, col_idx: usize) -> VectorFloat32<'a> {
        self.chunk().column(col_idx).get_vector_float32(self.idx)
    }

    /// GetRaw returns the underlying raw bytes with the colIdx.
    // go: Row.GetRaw L1
    pub fn get_raw(&self, col_idx: usize) -> &'a [u8] {
        self.chunk().column(col_idx).get_raw(self.idx)
    }

    /// IsNull returns if the datum in the chunk.Row is null.
    // go: Row.IsNull L1
    #[inline]
    pub fn is_null(&self, col_idx: usize) -> bool {
        self.chunk().column(col_idx).is_null(self.idx)
    }

    /// CopyConstruct creates a new row and copies this row's data into it.
    ///
    /// Go returns a row of a new chunk; the chunk is returned here (the row is its row 0).
    // go: Row.CopyConstruct L2 (returns the new one-row chunk; its row 0 is Go's result)
    pub fn copy_construct(&self) -> Chunk {
        let mut new_chk = renew_with_capacity(self.chunk(), 1, 1);
        new_chk.append_row(*self);
        new_chk
    }
}
