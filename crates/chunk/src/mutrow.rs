//! go-file: pkg/util/chunk/mutrow.go
//!
//! `MutRow` (N5a 3.1: L1): a chunk holding one row whose cells are overwritten in place.
//!
//! Mapping:
//! - Go's `MutRow` is a `Row{c *Chunk, idx int}` copied by value, every copy sharing the
//!   chunk; here a `MutRow` owns its chunk (`Option<Chunk>`, Go's zero `MutRow{}` has a nil
//!   chunk) and the setters take `&mut self`. [`MutRow::to_row`] borrows it.
//! - Go's `any` values (`MutRowFromValues`, `SetValue`) are [`GoValue`]s. Go's
//!   `makeMutRowColumn` returns a nil `*Column` for a type it does not know and stores it in
//!   the chunk, which panics at the first access; a chunk column cannot be nil here, so the
//!   constructor panics at once (L2: the same inputs panic, earlier).
//! - `newMutRowFixedLenColumn` uses one array as both `elemBuf` and `data`; writes through
//!   `data` therefore change `elemBuf` until `data` is replaced. [`MutRow`] keeps a flag per
//!   column and mirrors such writes into `elemBuf` (observable only through the bytes a NULL
//!   append would copy, kept for the byte-identical dumps).
//! - `newMutRowVarLenColumn` cuts `data` and `nullBitmap` from one allocation
//!   (`data = buf[:n]`, capacity `n+1`; `nullBitmap = buf[n:]`): the last byte of `data`'s
//!   capacity is the null bit. [`MutRow`] keeps a flag per column and mirrors the null bit
//!   there (Go reads it when `GetInt64` reads 8 bytes of a short BIT cell, `Row.ToString`).
//! - `ShallowCopyPartialRow` makes the cell a sub-slice of the source column (Go aliases
//!   the source's buffer, capacity up to the source's). Here the cell is copied into a buffer
//!   of the same length and capacity (L2: a later write through the MutRow does not reach
//!   the source column, which no Go caller does).
//! - Go's unsafe stores `*(*T)(unsafe.Pointer(&col.data[0])) = x` check only index 0 and may
//!   write past the slice's length; here the bytes are written within the capacity and a
//!   write past the capacity panics (Go: memory corruption).

// Datum kinds and type codes keep their Go names (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use tidb_base::mysql;
use tidb_parser::types::FieldType;
use tidb_types::MY_DECIMAL_STRUCT_SIZE;
use tidb_types::binary_literal::BinaryLiteral;
use tidb_types::datum::Datum;
use tidb_types::datum::GoValue;
use tidb_types::datum::{
    KindBinaryLiteral, KindBytes, KindFloat32, KindFloat64, KindInt64, KindMysqlDecimal,
    KindMysqlDuration, KindMysqlEnum, KindMysqlJSON, KindMysqlSet, KindMysqlTime, KindString,
    KindUint64, KindVectorFloat32,
};
use tidb_types::enum_type::Enum;
use tidb_types::json::BinaryJSON;
use tidb_types::json::JSONValue;
use tidb_types::json::create_binary_json;
use tidb_types::mydecimal::new_dec_from_int;
use tidb_types::set_type::Set;
use tidb_types::time::ZeroDate;
use tidb_types::time::ZeroDatetime;
use tidb_types::time::ZeroDuration;
use tidb_types::time::ZeroTimestamp;
use tidb_types::vector::ZeroVectorFloat32;

use crate::ColArc as Arc;
use crate::chunk::Chunk;
use crate::column::Column;
use crate::column::ElemBuf;
use crate::column::sizeTime;
use crate::goslice::ColAlloc;
use crate::goslice::GoSlice;
use crate::row::Row;

/// MutRow represents a mutable Row.
/// The underlying columns only contains one row and not exposed to the user.
#[derive(Debug, Default)]
pub struct MutRow {
    c: Option<Chunk>,
    idx: usize,
    /// Per column: `elemBuf` and `data` are still the same array (see the module comment).
    elem_alias: Vec<bool>,
    /// Per column: `nullBitmap` is still the byte after `data`'s capacity-1 (module comment).
    nb_alias: Vec<bool>,
}

impl MutRow {
    /// ToRow converts the MutRow to Row, so it can be used to read data.
    // go: MutRow.ToRow L1
    pub fn to_row(&self) -> Row<'_> {
        match &self.c {
            Some(c) => Row::new(c, self.idx),
            None => Row::default(),
        }
    }

    /// Len returns the number of columns.
    // go: MutRow.Len L1
    pub fn len(&self) -> usize {
        self.chunk().num_cols()
    }

    /// Whether the row has no column.
    // glue: clippy len_without_is_empty
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Clone deep clone a MutRow.
    // go: MutRow.Clone L1
    pub fn clone_row(&self) -> MutRow {
        let new_chk = self.c.as_ref().map(|c| c.copy_construct());
        MutRow {
            elem_alias: vec![false; self.elem_alias.len()],
            nb_alias: vec![false; self.nb_alias.len()],
            c: new_chk,
            idx: self.idx,
        }
    }

    /// The chunk (Go dereferences the nil chunk of a zero `MutRow` and panics).
    // glue: Go mr.c
    fn chunk(&self) -> &Chunk {
        self.c
            .as_ref()
            .expect("invalid memory address or nil pointer dereference")
    }

    // glue: Go mr.c.columns[colIdx] (write)
    fn col(&mut self, col_idx: usize) -> &mut Column {
        self.c
            .as_mut()
            .expect("invalid memory address or nil pointer dereference")
            .column_mut(col_idx)
    }

    /// Mirrors a write through `data` into the aliased `elemBuf` (module comment).
    // glue: Go elemBuf and data are one array in newMutRowFixedLenColumn
    fn sync_elem(&mut self, col_idx: usize) {
        if !self.elem_alias[col_idx] {
            return;
        }
        let col = self.col(col_idx);
        let data = col.data.bytes().to_vec();
        if let Some(eb) = col.elem_buf.as_mut() {
            eb.write(&data);
        }
    }

    /// Writes the null bit of a cell (`nullBitmap[0] = v`), and the byte of `data` it is
    /// while the two share one allocation (module comment).
    // glue: Go write to nullBitmap[0], which may be the last byte of data's array
    fn set_null_bit(&mut self, col_idx: usize, v: u8) {
        let alias = self.nb_alias[col_idx];
        let col = self.col(col_idx);
        *col.null_bitmap.at_mut(0) = v;
        if alias {
            let cap = col.data.cap();
            col.data.capb_mut()[cap - 1] = v;
        }
    }

    /// A MutRow over `columns` (Go: `MutRow{c: &Chunk{columns: ...}}`).
    // glue: Go composite literal MutRow{c: c}
    fn from_columns(cols: Vec<(Column, bool)>) -> MutRow {
        let elem_alias = cols.iter().map(|(_, a)| *a).collect();
        let nb_alias = cols.iter().map(|(c, _)| c.elem_buf.is_none()).collect();
        let c = Chunk {
            columns: Some(cols.into_iter().map(|(c, _)| Arc::new(c)).collect()),
            ..Chunk::default()
        };
        let mut mr = MutRow {
            c: Some(c),
            idx: 0,
            elem_alias,
            nb_alias,
        };
        for i in 0..mr.nb_alias.len() {
            let v = mr.chunk().column(i).null_bitmap.at(0);
            mr.set_null_bit(i, v);
        }
        mr
    }

    /// SetRow sets the MutRow with Row.
    // go: MutRow.SetRow L1
    pub fn set_row(&mut self, row: Row<'_>) {
        let src = row.chunk();
        for col_idx in 0..src.num_cols() {
            let r_col = src.column(col_idx);
            self.clean(col_idx);
            let mr_col = self.col(col_idx);
            if r_col.is_null(row.idx()) {
                continue;
            }
            let elem_len = r_col.elem_buf.map(|e| e.bytes().len()).unwrap_or(0);
            if elem_len > 0 {
                // copy(mrCol.data, rCol.data[row.idx*elemLen:(row.idx+1)*elemLen])
                let s = &r_col.data.capb()[row.idx() * elem_len..(row.idx() + 1) * elem_len];
                let d = mr_col.data.bytes_mut();
                let n = d.len().min(s.len());
                d[..n].copy_from_slice(&s[..n]);
            } else {
                let (start, end) = (
                    r_col.offsets.i64_at(row.idx()) as usize,
                    r_col.offsets.i64_at(row.idx() + 1) as usize,
                );
                let bin = r_col.data.capb()[start..end].to_vec();
                if set_mut_row_bytes(mr_col, &bin) {
                    self.replaced(col_idx, Replaced::Cell);
                }
            }
            self.set_null_bit(col_idx, 1);
            self.sync_elem(col_idx);
        }
    }

    /// SetValues sets the MutRow with values.
    // go: MutRow.SetValues L1
    pub fn set_values(&mut self, vals: &[GoValue]) {
        for (i, v) in vals.iter().enumerate() {
            self.set_value(i, v);
        }
    }

    /// SetValue sets the MutRow with colIdx and value.
    // go: MutRow.SetValue L1
    pub fn set_value(&mut self, col_idx: usize, val: &GoValue) {
        self.clean(col_idx);
        let col = self.col(col_idx);
        let realloc = match val {
            GoValue::Nil => return,
            GoValue::Int(x) | GoValue::Int64(x) => {
                put_uint64(col, *x as u64);
                false
            }
            GoValue::Uint64(x) => {
                put_uint64(col, *x);
                false
            }
            GoValue::Float64(x) => {
                put_uint64(col, x.to_bits());
                false
            }
            GoValue::Float32(x) => {
                col.data.bytes_mut()[..4].copy_from_slice(&x.to_bits().to_le_bytes());
                false
            }
            GoValue::String(x) => set_mut_row_bytes(col, x),
            GoValue::Bytes(x) => set_mut_row_bytes(col, x.as_deref().unwrap_or(&[])),
            GoValue::BinaryLiteral(x) => set_mut_row_bytes(col, &x.0),
            GoValue::Duration(x) => {
                store_at0(col, &x.duration.to_le_bytes());
                false
            }
            GoValue::Decimal(x) => {
                store_at0(col, &x.as_wire());
                false
            }
            GoValue::Time(x) => {
                store_at0(col, &x.raw().0.to_le_bytes());
                false
            }
            GoValue::Enum(x) => set_mut_row_name_value(col, &x.name, x.value),
            GoValue::Set(x) => set_mut_row_name_value(col, &x.name, x.value),
            GoValue::BinaryJSON(x) => set_mut_row_json(col, x),
            GoValue::VectorFloat32(x) => set_mut_row_bytes(col, x.zero_copy_serialize()),
            _ => false,
        };
        if realloc {
            self.replaced(col_idx, Replaced::Cell);
        }
        self.set_null_bit(col_idx, 1);
        self.sync_elem(col_idx);
    }

    /// SetDatums sets the MutRow with datum slice.
    // go: MutRow.SetDatums L1
    pub fn set_datums(&mut self, datums: &[Datum]) {
        for (i, d) in datums.iter().enumerate() {
            self.set_datum(i, d);
        }
    }

    /// SetDatum sets the MutRow with colIdx and datum.
    // go: MutRow.SetDatum L1
    pub fn set_datum(&mut self, col_idx: usize, d: &Datum) {
        self.clean(col_idx);
        let col = self.col(col_idx);
        if d.is_null() {
            return;
        }
        // For all fixed-size types, col.data may be zero-length when the column was
        // originally null-allocated (a varlen column with empty buffer). Grow the
        // buffer to the required size before writing.
        let mut data_replaced = false;
        let realloc = match d.kind() {
            KindInt64 | KindUint64 | KindFloat64 => {
                let r = grow_data(col, 8);
                put_uint64(col, d.get_uint64());
                data_replaced = r;
                false
            }
            KindFloat32 => {
                let r = grow_data(col, 4);
                col.data.bytes_mut()[..4].copy_from_slice(&d.get_float32().to_bits().to_le_bytes());
                data_replaced = r;
                false
            }
            KindString | KindBytes | KindBinaryLiteral => set_mut_row_bytes(col, d.get_bytes()),
            KindMysqlTime => {
                let r = grow_data(col, sizeTime);
                store_at0(col, &d.get_mysql_time().raw().0.to_le_bytes());
                data_replaced = r;
                false
            }
            KindMysqlDuration => {
                let r = grow_data(col, 8);
                store_at0(col, &d.get_mysql_duration().duration.to_le_bytes());
                data_replaced = r;
                false
            }
            KindMysqlDecimal => {
                let r = grow_data(col, MY_DECIMAL_STRUCT_SIZE);
                store_at0(col, &d.get_mysql_decimal().as_wire());
                data_replaced = r;
                false
            }
            KindMysqlJSON => set_mut_row_json(col, &d.get_mysql_json()),
            KindVectorFloat32 => {
                set_mut_row_bytes(col, d.get_vector_float32().zero_copy_serialize())
            }
            KindMysqlEnum => {
                let e = d.get_mysql_enum();
                set_mut_row_name_value(col, &e.name, e.value)
            }
            KindMysqlSet => {
                let s = d.get_mysql_set();
                set_mut_row_name_value(col, &s.name, s.value)
            }
            _ => {
                // Go replaces the column; the `col.nullBitmap[0] = 1` that follows writes to
                // the old column, which is no longer reachable.
                let (new_col, alias) = make_mut_row_column(&d.get_value()).expect(
                    "chunk: makeMutRowColumn returned a nil *Column (unsupported value type)",
                );
                let nb = new_col.elem_buf.is_none();
                let c = self.c.as_mut().unwrap();
                c.cols_mut()[col_idx] = Arc::new(new_col);
                self.elem_alias[col_idx] = alias;
                self.nb_alias[col_idx] = nb;
                let v = self.chunk().column(col_idx).null_bitmap.at(0);
                self.set_null_bit(col_idx, v);
                return;
            }
        };
        if data_replaced {
            self.replaced(col_idx, Replaced::Data);
        }
        if realloc {
            self.replaced(col_idx, Replaced::Cell);
        }
        self.set_null_bit(col_idx, 1);
        self.sync_elem(col_idx);
    }

    /// ShallowCopyPartialRow shallow copies the data of `row` to MutRow.
    // go: MutRow.ShallowCopyPartialRow L2 (copies the cell, see the module comment)
    pub fn shallow_copy_partial_row(&mut self, col_idx: usize, row: Row<'_>) {
        let src = row.chunk();
        for i in 0..src.num_cols() {
            let src_col = src.column(i);
            if !src_col.is_null(row.idx()) {
                // MutRow only contains one row, so we can directly set the whole byte.
                self.set_null_bit(col_idx + i, 1);
            } else {
                self.set_null_bit(col_idx + i, 0);
            }
            let dst_col = self.col(col_idx + i);

            if src_col.is_fixed() {
                let elem_len = src_col.elem_buf.map(|e| e.bytes().len()).unwrap_or(0);
                let offset = row.idx() * elem_len;
                dst_col.data = sub_slice(&src_col.data, offset, offset + elem_len);
            } else {
                let (start, end) = (
                    src_col.offsets.i64_at(row.idx()) as usize,
                    src_col.offsets.i64_at(row.idx() + 1) as usize,
                );
                dst_col.data = sub_slice(&src_col.data, start, end);
                let n = dst_col.data.len() as i64;
                dst_col.offsets.set_i64(1, n);
            }
            self.replaced(col_idx + i, Replaced::Data);
        }
    }

    /// cleanColOfMutRow on column `col_idx` (the null bit through [`MutRow::set_null_bit`]).
    // go: cleanColOfMutRow L1
    fn clean(&mut self, col_idx: usize) {
        let col = self.col(col_idx);
        for i in 0..col.offsets.len() {
            col.offsets.set_i64(i, 0);
        }
        self.set_null_bit(col_idx, 0);
    }

    /// Updates the alias flags after a column's buffers were replaced.
    // glue: tracking of Go's shared arrays (module comment)
    fn replaced(&mut self, col_idx: usize, r: Replaced) {
        self.elem_alias[col_idx] = false;
        self.nb_alias[col_idx] = matches!(r, Replaced::Cell);
    }
}

/// What a write replaced in a MutRow column.
#[derive(Clone, Copy)]
enum Replaced {
    /// `data` and `nullBitmap`, cut from one new allocation.
    Cell,
    /// `data` only.
    Data,
}

/// MutRowFromValues creates a MutRow from a interface slice.
// go: MutRowFromValues L2 (an unsupported value panics here, see the module comment)
pub fn mut_row_from_values(vals: &[GoValue]) -> MutRow {
    let cols = vals
        .iter()
        .map(|v| {
            make_mut_row_column(v)
                .expect("chunk: makeMutRowColumn returned a nil *Column (unsupported value type)")
        })
        .collect();
    MutRow::from_columns(cols)
}

/// MutRowFromDatums creates a MutRow from a datum slice.
// go: MutRowFromDatums L2 (an unsupported value panics here, see the module comment)
pub fn mut_row_from_datums(datums: &[Datum]) -> MutRow {
    let cols = datums
        .iter()
        .map(|d| {
            make_mut_row_column(&d.get_value())
                .expect("chunk: makeMutRowColumn returned a nil *Column (unsupported value type)")
        })
        .collect();
    MutRow::from_columns(cols)
}

/// MutRowFromTypes creates a MutRow from a FieldType slice, each Column is initialized to zero value.
// go: MutRowFromTypes L2 (an unsupported type panics here, see the module comment)
pub fn mut_row_from_types(types: &[FieldType]) -> MutRow {
    let cols = types
        .iter()
        .map(|tp| {
            make_mut_row_column(&zero_val_for_type(tp))
                .expect("chunk: makeMutRowColumn returned a nil *Column (unsupported value type)")
        })
        .collect();
    MutRow::from_columns(cols)
}

// go: zeroValForType L1
fn zero_val_for_type(tp: &FieldType) -> GoValue {
    match tp.get_type() {
        mysql::TypeFloat => GoValue::Float32(0.0),
        mysql::TypeDouble => GoValue::Float64(0.0),
        mysql::TypeTiny
        | mysql::TypeShort
        | mysql::TypeInt24
        | mysql::TypeLong
        | mysql::TypeLonglong
        | mysql::TypeYear => {
            if mysql::has_unsigned_flag(tp.get_flag()) {
                GoValue::Uint64(0)
            } else {
                GoValue::Int64(0)
            }
        }
        mysql::TypeString | mysql::TypeVarString | mysql::TypeVarchar => {
            GoValue::String(Vec::new())
        }
        mysql::TypeBlob | mysql::TypeTinyBlob | mysql::TypeMediumBlob | mysql::TypeLongBlob => {
            GoValue::Bytes(Some(Vec::new()))
        }
        mysql::TypeDuration => GoValue::Duration(ZeroDuration),
        mysql::TypeNewDecimal => GoValue::Decimal(Box::new(new_dec_from_int(0))),
        mysql::TypeDate => GoValue::Time(*ZeroDate),
        mysql::TypeDatetime => GoValue::Time(*ZeroDatetime),
        mysql::TypeTimestamp => GoValue::Time(*ZeroTimestamp),
        mysql::TypeBit => GoValue::BinaryLiteral(BinaryLiteral(Vec::new())),
        mysql::TypeSet => GoValue::Set(Set::default()),
        mysql::TypeEnum => GoValue::Enum(Enum::default()),
        mysql::TypeJSON => GoValue::BinaryJSON(create_binary_json(JSONValue::Nil)),
        mysql::TypeTiDBVectorFloat32 => GoValue::VectorFloat32(ZeroVectorFloat32.clone()),
        _ => GoValue::Nil,
    }
}

/// The column for one value, and whether its `elemBuf` and `data` are one array; `None` is
/// Go's nil `*Column` (an unsupported type).
// go: makeMutRowColumn L1
fn make_mut_row_column(in_: &GoValue) -> Option<(Column, bool)> {
    let fixed = |c: Column| Some((c, true));
    let var = |c: Column| Some((c, false));
    match in_ {
        GoValue::Nil => {
            let mut col = make_mut_row_bytes_column(&[]);
            *col.null_bitmap.at_mut(0) = 0;
            var(col)
        }
        GoValue::Int(x) | GoValue::Int64(x) => fixed(make_mut_row_uint64_column(*x as u64)),
        GoValue::Uint64(x) => fixed(make_mut_row_uint64_column(*x)),
        GoValue::Float64(x) => fixed(make_mut_row_uint64_column(x.to_bits())),
        GoValue::Float32(x) => {
            let col = new_mut_row_fixed_len_column_with(&x.to_bits().to_le_bytes());
            fixed(col)
        }
        GoValue::String(x) => var(make_mut_row_bytes_column(x)),
        GoValue::Bytes(x) => var(make_mut_row_bytes_column(x.as_deref().unwrap_or(&[]))),
        GoValue::BinaryLiteral(x) => var(make_mut_row_bytes_column(&x.0)),
        GoValue::Decimal(x) => fixed(new_mut_row_fixed_len_column_with(&x.as_wire())),
        GoValue::Time(x) => fixed(new_mut_row_fixed_len_column_with(&x.raw().0.to_le_bytes())),
        GoValue::BinaryJSON(x) => {
            let mut col = new_mut_row_var_len_column(x.value.len() + 1);
            let d = col.data.bytes_mut();
            d[0] = x.type_code;
            d[1..].copy_from_slice(&x.value);
            var(col)
        }
        GoValue::VectorFloat32(x) => var(make_mut_row_bytes_column(x.zero_copy_serialize())),
        GoValue::Duration(x) => fixed(new_mut_row_fixed_len_column_with(&x.duration.to_le_bytes())),
        GoValue::Enum(Enum { name, value }) | GoValue::Set(Set { name, value }) => {
            let mut col = new_mut_row_var_len_column(name.len() + 8);
            let d = col.data.bytes_mut();
            d[..8].copy_from_slice(&value.to_le_bytes());
            d[8..].copy_from_slice(name);
            var(col)
        }
        _ => None,
    }
}

// go: newMutRowFixedLenColumn L1
fn new_mut_row_fixed_len_column(elem_size: usize) -> Column {
    let mut col = Column::zero(ColAlloc::Global);
    col.length = 1;
    col.elem_buf = Some(ElemBuf::new_zeroed(elem_size));
    col.data = GoSlice::make(elem_size, elem_size, ColAlloc::Global);
    col.null_bitmap = GoSlice::make(1, 1, ColAlloc::Global);
    *col.null_bitmap.at_mut(0) = 1;
    col
}

/// `newMutRowFixedLenColumn(len(v))` followed by Go's store of `v` at `data[0]` (which is
/// also `elemBuf[0]`).
// glue: Go newMutRowFixedLenColumn + unsafe store through the shared array
fn new_mut_row_fixed_len_column_with(v: &[u8]) -> Column {
    let mut col = new_mut_row_fixed_len_column(v.len());
    col.data.bytes_mut().copy_from_slice(v);
    if let Some(eb) = col.elem_buf.as_mut() {
        eb.write(v);
    }
    col
}

// go: newMutRowVarLenColumn L1
fn new_mut_row_var_len_column(val_size: usize) -> Column {
    let mut col = Column::zero(ColAlloc::Global);
    col.length = 1;
    col.offsets = GoSlice::make(2, 2, ColAlloc::Global);
    col.offsets.set_i64(1, val_size as i64);
    col.data = GoSlice::make(val_size, val_size + 1, ColAlloc::Global);
    col.null_bitmap = GoSlice::make(1, 1, ColAlloc::Global);
    *col.null_bitmap.at_mut(0) = 1;
    col
}

// go: makeMutRowUint64Column L1
fn make_mut_row_uint64_column(val: u64) -> Column {
    new_mut_row_fixed_len_column_with(&val.to_le_bytes())
}

// go: makeMutRowBytesColumn L1
fn make_mut_row_bytes_column(bin: &[u8]) -> Column {
    let mut col = new_mut_row_var_len_column(bin.len());
    col.data.bytes_mut().copy_from_slice(bin);
    col
}

/// `binary.LittleEndian.PutUint64(col.data, v)` (panics when `len(col.data) < 8`).
// go-std: encoding/binary.littleEndian.PutUint64 L1
fn put_uint64(col: &mut Column, v: u64) {
    col.data.bytes_mut()[..8].copy_from_slice(&v.to_le_bytes());
}

/// `*(*T)(unsafe.Pointer(&col.data[0])) = x`: index 0 is checked, the store may reach past
/// the length within the capacity (module comment).
// glue: Go unsafe store at &col.data[0]
fn store_at0(col: &mut Column, v: &[u8]) {
    assert!(col.data.len() > 0, "index out of range [0] with length 0");
    col.data.capb_mut()[..v.len()].copy_from_slice(v);
}

/// `if len(col.data) < n { col.data = make([]byte, n) }`; returns whether `data` was
/// replaced.
// glue: Go growth of a MutRow cell in SetDatum
fn grow_data(col: &mut Column, n: usize) -> bool {
    if col.data.len() < n {
        col.data = GoSlice::make(n, n, ColAlloc::Global);
        return true;
    }
    false
}

/// `src[from:to]` as a new buffer with Go's length and capacity (`cap(src) - from`), holding
/// the source's bytes up to its capacity (Go's sub-slice reads them past its length).
// glue: Go sub-slice (copied, see the module comment)
fn sub_slice(src: &GoSlice<1>, from: usize, to: usize) -> GoSlice<1> {
    let tail = &src.capb()[from..];
    let mut s = GoSlice::make(to - from, tail.len(), ColAlloc::Global);
    s.capb_mut().copy_from_slice(tail);
    s
}

/// The `data` / `nullBitmap` pair of a reallocated cell (Go: `buf := make([]byte, n+1)`,
/// `data = buf[:n]`, `nullBitmap = buf[n:]`).
// glue: Go buf split of setMutRowBytes / setMutRowNameValue / setMutRowJSON
fn realloc_cell(col: &mut Column, n: usize) {
    col.data = GoSlice::make(n, n + 1, ColAlloc::Global);
    col.null_bitmap = GoSlice::make(1, 1, ColAlloc::Global);
}

/// Returns whether `data` was replaced.
// go: setMutRowBytes L1
fn set_mut_row_bytes(col: &mut Column, bin: &[u8]) -> bool {
    let realloc = if col.data.len() >= bin.len() {
        col.data.reslice(bin.len());
        false
    } else {
        realloc_cell(col, bin.len());
        true
    };
    col.data.bytes_mut().copy_from_slice(bin);
    col.offsets.set_i64(1, bin.len() as i64);
    realloc
}

/// Returns whether `data` was replaced.
// go: setMutRowNameValue L1
fn set_mut_row_name_value(col: &mut Column, name: &[u8], val: u64) -> bool {
    let data_len = name.len() + 8;
    let realloc = if col.data.len() >= data_len {
        col.data.reslice(data_len);
        false
    } else {
        realloc_cell(col, data_len);
        true
    };
    let d = col.data.bytes_mut();
    d[..8].copy_from_slice(&val.to_le_bytes());
    d[8..].copy_from_slice(name);
    col.offsets.set_i64(1, data_len as i64);
    realloc
}

/// Returns whether `data` was replaced.
// go: setMutRowJSON L1
fn set_mut_row_json(col: &mut Column, j: &BinaryJSON<'_>) -> bool {
    let data_len = j.value.len() + 1;
    let realloc = if col.data.len() >= data_len {
        col.data.reslice(data_len);
        false
    } else {
        // In MutRow, there always exists 1 data in every Column,
        // we should allocate one more byte for null bitmap.
        realloc_cell(col, data_len);
        true
    };
    let d = col.data.bytes_mut();
    d[0] = j.type_code;
    d[1..].copy_from_slice(&j.value);
    col.offsets.set_i64(1, data_len as i64);
    realloc
}
