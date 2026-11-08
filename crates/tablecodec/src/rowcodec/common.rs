//! go-file: pkg/util/rowcodec/common.go
//!
//! Row format constants, the little-endian variable-width integers of the v2 format, and the
//! row checksum input (`ColData`, `RowData`).
//!
//! Mapping:
//! - `bytesToU32Slice` / `bytes2U16Slice` reinterpret the row bytes in place (`unsafe`); the
//!   port reads the elements from the bytes on access in the machine's byte order (what Go's
//!   reinterpretation yields), through [`U32Slice`] / [`U16Slice`].
//! - The encoder side (`encodeInt`, `encodeUint`, the column sorters, `u16SliceToBytes`,
//!   `u32SliceToBytes`) is the write path (tracker block X).
//! - `appendDatumForChecksum` recovers the panic of a datum/type mismatch (Go: a failed type
//!   assertion in a `Datum` getter) and annotates it; the port catches the Rust panic of the
//!   same getter. The panic message is the Rust getter's, not Go's runtime message (L2).

// Flags keep their Go names (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use std::sync::Arc;
use std::sync::LazyLock;

use tidb_base::Error;
use tidb_base::errors;
use tidb_base::gofmt::Arg;
use tidb_base::gostd::time::{Location, utc};
use tidb_base::mysql;
use tidb_model::ColumnInfo;
use tidb_parser::types::FieldType;
use tidb_parser::types::type_str;
use tidb_types::context::DefaultStmtNoWarningContext;
use tidb_types::datum::Datum;

/// CodecVer is the constant number that represent the new row format.
pub const CodecVer: u8 = 128;

pub(crate) static errInvalidCodecVer: LazyLock<Error> =
    LazyLock::new(|| errors::new("invalid codec version"));
pub(crate) static errInvalidChecksumVer: LazyLock<Error> =
    LazyLock::new(|| errors::new("invalid checksum version"));
pub(crate) static errInvalidChecksumTyp: LazyLock<Error> =
    LazyLock::new(|| errors::new("invalid type for checksum"));

// First byte in the encoded value which specifies the encoding type.
pub const NilFlag: u8 = 0;
pub const BytesFlag: u8 = 1;
pub const CompactBytesFlag: u8 = 2;
pub const IntFlag: u8 = 3;
pub const UintFlag: u8 = 4;
pub const FloatFlag: u8 = 5;
pub const DecimalFlag: u8 = 6;
pub const VarintFlag: u8 = 8;
pub const VaruintFlag: u8 = 9;
pub const JSONFlag: u8 = 10;
pub const VectorFloat32Flag: u8 = 20;

/// A `[]uint32` aliasing row bytes (Go `bytesToU32Slice`).
#[derive(Clone, Copy, Debug, Default)]
pub struct U32Slice<'a>(&'a [u8]);

impl U32Slice<'_> {
    /// `len(s)`.
    // glue: length of the reinterpreted slice
    pub fn len(&self) -> usize {
        self.0.len() / 4
    }

    /// `len(s) == 0`.
    // glue: emptiness of the reinterpreted slice
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `s[i]` (panics out of range, as Go).
    // glue: element of the reinterpreted slice (machine byte order)
    pub fn get(&self, i: usize) -> u32 {
        assert!(i < self.len(), "index out of range [{i}] with length {}", self.len());
        let b = &self.0[i * 4..i * 4 + 4];
        u32::from_ne_bytes([b[0], b[1], b[2], b[3]])
    }
}

/// A `[]uint16` aliasing row bytes (Go `bytes2U16Slice`).
#[derive(Clone, Copy, Debug, Default)]
pub struct U16Slice<'a>(&'a [u8]);

impl U16Slice<'_> {
    /// `len(s)`.
    // glue: length of the reinterpreted slice
    pub fn len(&self) -> usize {
        self.0.len() / 2
    }

    /// `len(s) == 0`.
    // glue: emptiness of the reinterpreted slice
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `s[i]` (panics out of range, as Go).
    // glue: element of the reinterpreted slice (machine byte order)
    pub fn get(&self, i: usize) -> u16 {
        assert!(i < self.len(), "index out of range [{i}] with length {}", self.len());
        let b = &self.0[i * 2..i * 2 + 2];
        u16::from_ne_bytes([b[0], b[1]])
    }
}

// go: bytesToU32Slice L1
pub(crate) fn bytes_to_u32_slice(b: &[u8]) -> U32Slice<'_> {
    if b.is_empty() {
        return U32Slice::default();
    }
    U32Slice(&b[..b.len() / 4 * 4])
}

// go: bytes2U16Slice L1
pub(crate) fn bytes2_u16_slice(b: &[u8]) -> U16Slice<'_> {
    if b.is_empty() {
        return U16Slice::default();
    }
    U16Slice(&b[..b.len() / 2 * 2])
}

/// `binary.LittleEndian.Uint64(val)`: panics on fewer than 8 bytes, as Go.
// go-std: encoding/binary.littleEndian.Uint64 L1
fn le_u64(val: &[u8]) -> u64 {
    let b: [u8; 8] = val[..8].try_into().expect("8 bytes");
    u64::from_le_bytes(b)
}

// go: decodeInt L1
pub(crate) fn decode_int(val: &[u8]) -> i64 {
    match val.len() {
        1 => val[0] as i8 as i64,
        2 => i16::from_le_bytes([val[0], val[1]]) as i64,
        4 => i32::from_le_bytes([val[0], val[1], val[2], val[3]]) as i64,
        _ => le_u64(val) as i64,
    }
}

// go: decodeUint L1
pub(crate) fn decode_uint(val: &[u8]) -> u64 {
    match val.len() {
        1 => val[0] as u64,
        2 => u16::from_le_bytes([val[0], val[1]]) as u64,
        4 => u32::from_le_bytes([val[0], val[1], val[2], val[3]]) as u64,
        _ => le_u64(val),
    }
}

/// IsNewFormat checks whether row data is in new-format.
// go: IsNewFormat L1
pub fn is_new_format(row_data: &[u8]) -> bool {
    row_data[0] == CodecVer
}

/// FieldTypeFromModelColumn creates a types.FieldType from model.ColumnInfo.
/// export for test case and CDC.
// go: FieldTypeFromModelColumn L1
pub fn field_type_from_model_column(col: &ColumnInfo) -> FieldType {
    col.field_type.clone()
}

/// ColData combines the column info as well as its datum. It's used to calculate checksum.
#[derive(Clone, Copy, Debug)]
pub struct ColData<'a> {
    pub column_info: &'a ColumnInfo,
    pub datum: &'a Datum,
}

impl ColData<'_> {
    /// Encode encodes the column datum into bytes for checksum. If buf provided, append encoded data to it.
    // go: ColData.Encode L1
    pub fn encode(&self, loc: Option<&Arc<Location>>, buf: Vec<u8>) -> Result<Vec<u8>, Error> {
        append_datum_for_checksum(loc, buf, self.datum, self.column_info.get_type())
    }
}

/// RowData is a list of ColData for row checksum calculation.
#[derive(Clone, Debug, Default)]
pub struct RowData<'a> {
    /// Cols is a list of ColData which is expected to be sorted by id before calling Encode/Checksum.
    pub cols: Vec<ColData<'a>>,
    /// Data stores the result of Encode. However, it mostly acts as a buffer for encoding columns on checksum
    /// calculation.
    pub data: Vec<u8>,
}

impl RowData<'_> {
    /// Len implements sort.Interface for RowData.
    // go: RowData.Len L1
    pub fn len(&self) -> usize {
        self.cols.len()
    }

    /// `len(r.Cols) == 0`.
    // glue: clippy len_without_is_empty
    pub fn is_empty(&self) -> bool {
        self.cols.is_empty()
    }

    /// Less implements sort.Interface for RowData.
    // go: RowData.Less L1
    pub fn less(&self, i: usize, j: usize) -> bool {
        self.cols[i].column_info.id < self.cols[j].column_info.id
    }

    /// Swap implements sort.Interface for RowData.
    // go: RowData.Swap L1
    pub fn swap(&mut self, i: usize, j: usize) {
        self.cols.swap(i, j);
    }

    /// Checksum calculates the checksum of columns. Callers should make sure columns are sorted by id.
    // go: RowData.Checksum L1
    pub fn checksum(&mut self, loc: Option<&Arc<Location>>) -> Result<u32, Error> {
        let mut checksum = 0u32;
        for col in &self.cols {
            let mut data = std::mem::take(&mut self.data);
            if !data.is_empty() {
                data.clear();
            }
            data = col.encode(loc, data)?;
            checksum = crc32_update(checksum, &data);
            self.data = data;
        }
        Ok(checksum)
    }
}

/// `crc32.Update(crc, crc32.IEEETable, p)`.
// go-std: hash/crc32.Update L1
pub(crate) fn crc32_update(crc: u32, p: &[u8]) -> u32 {
    static TABLE: LazyLock<[u32; 256]> = LazyLock::new(|| {
        let mut t = [0u32; 256];
        for (i, e) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 == 1 { (c >> 1) ^ 0xedb88320 } else { c >> 1 };
            }
            *e = c;
        }
        t
    });
    let mut c = !crc;
    for &b in p {
        c = TABLE[((c as u8) ^ b) as usize] ^ (c >> 8);
    }
    !c
}

/// The message of a recovered panic payload (Go `x.(error)`'s `Error()`).
// glue: panic payload as an error message
fn panic_message(p: &(dyn std::any::Any + Send)) -> String {
    p.downcast_ref::<String>()
        .cloned()
        .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_default()
}

// go: appendDatumForChecksum L1
fn append_datum_for_checksum(
    loc: Option<&Arc<Location>>,
    buf: Vec<u8>,
    dat: &Datum,
    typ: u8,
) -> Result<Vec<u8>, Error> {
    // catch panic when datum and type mismatch
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        append_datum_for_checksum_inner(loc, buf, dat, typ)
    }));
    match res {
        Ok(r) => r,
        // errors.Annotatef(err, ...): "<annotation>: <cause>".
        Err(p) => Err(errors::errorf(
            "encode datum(%s) as %s for checksum: %s",
            &[
                Arg::go_string(dat.string()),
                Arg::go_string(type_str(typ).as_bytes().to_vec()),
                Arg::go_string(panic_message(&*p).into_bytes()),
            ],
        )),
    }
}

/// The body of `appendDatumForChecksum` (the part its deferred `recover` guards).
// glue: body of appendDatumForChecksum under recover
fn append_datum_for_checksum_inner(
    loc: Option<&Arc<Location>>,
    mut buf: Vec<u8>,
    dat: &Datum,
    typ: u8,
) -> Result<Vec<u8>, Error> {
    if dat.is_null() {
        return Ok(buf);
    }
    match typ {
        mysql::TypeTiny
        | mysql::TypeShort
        | mysql::TypeLong
        | mysql::TypeLonglong
        | mysql::TypeInt24
        | mysql::TypeYear => {
            buf.extend_from_slice(&dat.get_uint64().to_le_bytes());
        }
        mysql::TypeVarchar
        | mysql::TypeVarString
        | mysql::TypeString
        | mysql::TypeTinyBlob
        | mysql::TypeMediumBlob
        | mysql::TypeLongBlob
        | mysql::TypeBlob => {
            append_length_value(&mut buf, dat.get_bytes());
        }
        mysql::TypeTimestamp | mysql::TypeDatetime | mysql::TypeDate | mysql::TypeNewDate => {
            let mut t = dat.get_mysql_time();
            if t.tp() == mysql::TypeTimestamp
                && let Some(loc) = loc
                && !Arc::ptr_eq(loc, &utc())
            {
                t.convert_time_zone(loc, &utc())?;
            }
            append_length_value(&mut buf, &t.string());
        }
        mysql::TypeDuration => {
            append_length_value(&mut buf, &dat.get_mysql_duration().string());
        }
        mysql::TypeFloat | mysql::TypeDouble => {
            let mut v = dat.get_float64();
            if v.is_infinite() || v.is_nan() {
                v = 0.0; // because ticdc has such a transform
            }
            buf.extend_from_slice(&v.to_bits().to_le_bytes());
        }
        mysql::TypeNewDecimal => {
            append_length_value(&mut buf, dat.get_mysql_decimal().string().as_bytes());
        }
        mysql::TypeEnum => {
            buf.extend_from_slice(&dat.get_mysql_enum().value.to_le_bytes());
        }
        mysql::TypeSet => {
            buf.extend_from_slice(&dat.get_mysql_set().value.to_le_bytes());
        }
        mysql::TypeBit => {
            // ticdc transforms a bit value as the following way, no need to handle truncate error here.
            let (v, _) = dat.get_binary_literal().to_int(&DefaultStmtNoWarningContext);
            buf.extend_from_slice(&v.to_le_bytes());
        }
        mysql::TypeJSON => {
            append_length_value(&mut buf, &dat.get_mysql_json().string());
        }
        mysql::TypeTiDBVectorFloat32 => {
            buf = dat.get_vector_float32().serialize_to(buf);
        }
        mysql::TypeNull | mysql::TypeGeometry => {}
        _ => return Err(errInvalidChecksumTyp.clone()),
    }
    Ok(buf)
}

// go: appendLengthValue L1
fn append_length_value(buf: &mut Vec<u8>, val: &[u8]) {
    buf.extend_from_slice(&(val.len() as u32).to_le_bytes());
    buf.extend_from_slice(val);
}
