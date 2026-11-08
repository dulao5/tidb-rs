//! go-file: pkg/util/chunk/row.go
//!
//! `Row` (N5a 3.1: L1). Go's `Row{c *Chunk, idx int}` is a `Copy` borrow here,
//! `Row<'a>{c: Option<&'a Chunk>, idx}`; the zero `Row{}` (the iterators' end marker) has no
//! chunk. A row kept across `Next` calls is a `RowHandle` (mapping rule `M-ROW`, added with
//! the first operator that needs it).
//!
//! `GetDatum` and friends copy the cell into the datum: Go's datum references the chunk's
//! bytes (`hack.String`, `*MyDecimal` into the column), so it is only valid while the chunk
//! is unchanged; the Rust datum owns its value (L2, same values).

// Go names for package-level constants (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use tidb_base::gostd::strconv::append_float;
use tidb_base::mysql;
use tidb_parser::types::FieldType;
use tidb_parser::types::UnspecifiedLength;
use tidb_parser::types::{
    ETDatetime, ETDecimal, ETDuration, ETInt, ETJson, ETReal, ETString, ETTimestamp,
    ETVectorFloat32,
};
use tidb_types::MyDecimal;
use tidb_types::binary_literal::BinaryLiteral;
use tidb_types::datum::Datum;
use tidb_types::enum_type::Enum;
use tidb_types::json::BinaryJSON;
use tidb_types::set_type::Set;
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

    /// GetEnum returns the Enum value with the colIdx.
    // go: Row.GetEnum L1
    pub fn get_enum(&self, col_idx: usize) -> Enum {
        self.chunk().column(col_idx).get_enum(self.idx)
    }

    /// GetSet returns the Set value with the colIdx.
    // go: Row.GetSet L1
    pub fn get_set(&self, col_idx: usize) -> Set {
        self.chunk().column(col_idx).get_set(self.idx)
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

    /// GetDatumRow converts chunk.Row to types.DatumRow.
    /// Keep in mind that GetDatumRow has a reference to r.c, which is a chunk,
    /// this function works only if the underlying chunk is valid or unchanged.
    // go: Row.GetDatumRow L1
    pub fn get_datum_row(&self, fields: &[FieldType]) -> Vec<Datum> {
        let mut datum_row = vec![Datum::default(); self.chunk().num_cols()];
        self.get_datum_row_with_buffer(fields, &mut datum_row);
        datum_row
    }

    /// GetDatumRowWithBuffer gets datum using the buffer datumRow.
    // go: Row.GetDatumRowWithBuffer L1
    pub fn get_datum_row_with_buffer<'d>(
        &self,
        fields: &[FieldType],
        datum_row: &'d mut [Datum],
    ) -> &'d mut [Datum] {
        for (col_idx, d) in datum_row.iter_mut().enumerate() {
            self.datum_with_buffer(col_idx, &fields[col_idx], d);
        }
        datum_row
    }

    /// GetDatum implements the chunk.Row interface.
    // go: Row.GetDatum L1
    pub fn get_datum(&self, col_idx: usize, tp: &FieldType) -> Datum {
        let mut d = Datum::default();
        self.datum_with_buffer(col_idx, tp, &mut d);
        d
    }

    /// DatumWithBuffer gets datum using the buffer d.
    // go: Row.DatumWithBuffer L2 (the datum owns a copy of the cell, see the module comment)
    #[allow(clippy::collapsible_match)] // Go's switch with an if per case
    pub fn datum_with_buffer(&self, col_idx: usize, tp: &FieldType, d: &mut Datum) {
        match tp.get_type() {
            mysql::TypeTiny
            | mysql::TypeShort
            | mysql::TypeInt24
            | mysql::TypeLong
            | mysql::TypeLonglong => {
                if !self.is_null(col_idx) {
                    if mysql::has_unsigned_flag(tp.get_flag()) {
                        d.set_uint64(self.get_uint64(col_idx));
                    } else {
                        d.set_int64(self.get_int64(col_idx));
                    }
                }
            }
            mysql::TypeYear => {
                // FIXBUG: because insert type of TypeYear is definite int64, so we regardless of the unsigned flag.
                if !self.is_null(col_idx) {
                    d.set_int64(self.get_int64(col_idx));
                }
            }
            mysql::TypeFloat => {
                if !self.is_null(col_idx) {
                    d.set_float32(self.get_float32(col_idx));
                }
            }
            mysql::TypeDouble => {
                if !self.is_null(col_idx) {
                    d.set_float64(self.get_float64(col_idx));
                }
            }
            mysql::TypeVarchar
            | mysql::TypeVarString
            | mysql::TypeString
            | mysql::TypeBlob
            | mysql::TypeTinyBlob
            | mysql::TypeMediumBlob
            | mysql::TypeLongBlob => {
                if !self.is_null(col_idx) {
                    d.set_string(
                        self.get_string(col_idx).to_vec(),
                        tp.get_collate().to_owned(),
                    );
                }
            }
            mysql::TypeDate | mysql::TypeDatetime | mysql::TypeTimestamp => {
                if !self.is_null(col_idx) {
                    d.set_mysql_time(self.get_time(col_idx));
                }
            }
            mysql::TypeDuration => {
                if !self.is_null(col_idx) {
                    let duration = self.get_duration(col_idx, tp.get_decimal());
                    d.set_mysql_duration(duration);
                }
            }
            mysql::TypeNewDecimal => {
                if !self.is_null(col_idx) {
                    let dec = self.get_my_decimal(col_idx);
                    let digits_frac = dec.get_digits_frac();
                    d.set_mysql_decimal(dec);
                    d.set_length(tp.get_flen());
                    // If tp.decimal is unspecified(-1), we should set it to the real
                    // fraction length of the decimal value, if not, the d.Frac will
                    // be set to MAX_UINT16 which will cause unexpected BadNumber error
                    // when encoding.
                    if tp.get_decimal() == UnspecifiedLength {
                        d.set_frac(digits_frac as isize);
                    } else {
                        d.set_frac(tp.get_decimal());
                    }
                }
            }
            mysql::TypeEnum => {
                if !self.is_null(col_idx) {
                    d.set_mysql_enum(self.get_enum(col_idx), tp.get_collate().to_owned());
                }
            }
            mysql::TypeSet => {
                if !self.is_null(col_idx) {
                    d.set_mysql_set(self.get_set(col_idx), tp.get_collate().to_owned());
                }
            }
            mysql::TypeBit => {
                if !self.is_null(col_idx) {
                    d.set_mysql_bit(BinaryLiteral(self.get_bytes(col_idx).to_vec()));
                }
            }
            mysql::TypeJSON => {
                if !self.is_null(col_idx) {
                    d.set_mysql_json(self.get_json(col_idx));
                }
            }
            mysql::TypeTiDBVectorFloat32 => {
                if !self.is_null(col_idx) {
                    d.set_vector_float32(self.get_vector_float32(col_idx));
                }
            }
            _ => {}
        }
        if self.is_null(col_idx) {
            d.set_null();
        }
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

    /// ToString returns all the values in a row.
    // go: Row.ToString L1
    #[allow(clippy::needless_range_loop)] // Go's loop over the column indexes
    pub fn to_string(&self, ft: &[FieldType]) -> Vec<u8> {
        let mut buf: Vec<u8> = Vec::new();
        let num_cols = self.chunk().num_cols();
        for col_idx in 0..num_cols {
            if self.is_null(col_idx) {
                buf.extend_from_slice(b"NULL");
            } else {
                match ft[col_idx].eval_type() {
                    ETInt => buf.extend_from_slice(self.get_int64(col_idx).to_string().as_bytes()),
                    ETString => match ft[col_idx].get_type() {
                        mysql::TypeEnum => buf.extend_from_slice(&self.get_enum(col_idx).string()),
                        mysql::TypeSet => buf.extend_from_slice(&self.get_set(col_idx).string()),
                        _ => buf.extend_from_slice(self.get_string(col_idx)),
                    },
                    ETDatetime | ETTimestamp => {
                        buf.extend_from_slice(&self.get_time(col_idx).string())
                    }
                    ETDecimal => buf.extend_from_slice(&self.get_my_decimal(col_idx).to_string()),
                    ETDuration => buf.extend_from_slice(
                        &self
                            .get_duration(col_idx, ft[col_idx].get_decimal())
                            .string(),
                    ),
                    ETJson => buf.extend_from_slice(&self.get_json(col_idx).string()),
                    ETReal => match ft[col_idx].get_type() {
                        mysql::TypeFloat => {
                            append_float(&mut buf, self.get_float32(col_idx) as f64, b'f', -1, 32)
                        }
                        mysql::TypeDouble => {
                            append_float(&mut buf, self.get_float64(col_idx), b'f', -1, 64)
                        }
                        _ => {}
                    },
                    ETVectorFloat32 => {
                        buf.extend_from_slice(self.get_vector_float32(col_idx).string().as_bytes())
                    }
                    _ => {}
                }
            }
            if col_idx != num_cols - 1 {
                buf.extend_from_slice(b", ");
            }
        }
        buf
    }
}
