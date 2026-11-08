//! go-file: pkg/util/codec/codec.go
//!
//! The Datum codec (`EncodeKey` / `EncodeValue`, `Decode*`, `CutOne`, `peek`), decoding into
//! a chunk (`Decoder`), and the chunk-row hashing and serialisation used by hash join and
//! aggregation (`HashChunkRow`, `HashChunkSelected`, `SerializeKeys`, `HashGroupKey`).
//!
//! Mapping:
//! - Appending functions take `b: &mut Vec<u8>`; where Go returns `b` together with an
//!   error the vector keeps what Go's returned slice holds (M-ERRVAL). Go returns a nil
//!   slice from `EncodeMySQLTime` on error: the port empties the vector.
//! - Decoders return the remaining input borrowed from the argument. Where Go's results are
//!   meaningful next to the error (`DecodeOne`'s remain and datum, `DecodeRange`'s values),
//!   the port returns them with a `Result<(), Error>` (M-ERRVAL).
//! - `hash.Hash64` is `std::hash::Hasher` (`Write` never fails; `Sum64` is `finish`);
//!   `io.Writer` is `std::io::Write`, its error converted to an `Error` with the same text.
//! - `unsafe.Slice((*byte)(unsafe.Pointer(&v)), 8)` of a `uint64` / `float64` is the
//!   native-endian byte image (`to_ne_bytes`).
//! - `time.Location` is `Arc<Location>`; Go's `loc != time.UTC` is a pointer comparison
//!   with the `time.UTC` singleton, kept as `Arc::ptr_eq` with [`utc`].
//! - `Hash64` takes the parser's `IHasher` (the `Hasher` of `planner/cascades/base` minus
//!   `Cache` / `SetCache`, which only reuse the hasher's scratch buffer): the encoded datum is
//!   built in a fresh vector (L2). Go's `init` sets `types.Hash64ForDatum`; Rust has no
//!   package initialisers, the program calls [`init`] once at start-up (L2).

// Flags and datum kinds keep their Go names (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]
// Index loops over Go's `for i := range rows` (style lint allowed for ported files).
#![allow(clippy::needless_range_loop)]

use std::borrow::Cow;
use std::hash::Hasher;
use std::sync::Arc;

use tidb_base::Error;
use tidb_base::errors;
use tidb_base::gofmt::Arg;
use tidb_base::gostd::time::{Location, utc};
use tidb_base::mysql;
use tidb_base::terror;
use tidb_chunk::{Chunk, Column, Row};
use tidb_parser::types::{
    ETDatetime, ETDecimal, ETDuration, ETInt, ETJson, ETReal, ETString, ETTimestamp,
    ETVectorFloat32, FieldType, IHasher,
};
use tidb_types::binary_literal::{BinaryLiteral, new_binary_literal_from_uint};
use tidb_types::collate;
use tidb_types::context::{Context, StrictContext};
use tidb_types::core_time::ZeroCoreTime;
use tidb_types::datum::{
    Datum, Hash64ForDatum, KindBinaryLiteral, KindBytes, KindFloat32, KindFloat64, KindInt64,
    KindMaxValue, KindMinNotNull, KindMysqlBit, KindMysqlDecimal, KindMysqlDuration, KindMysqlEnum,
    KindMysqlJSON, KindMysqlSet, KindMysqlTime, KindNull, KindString, KindUint64,
    KindVectorFloat32, max_value_datum, min_not_null_datum,
};
use tidb_types::enum_type::parse_enum_value;
use tidb_types::etc::{is_type_float, is_type_time};
use tidb_types::fsp::MaxFsp;
use tidb_types::json::{BinaryJSON, peek_bytes_as_json};
use tidb_types::set_type::parse_set_value;
use tidb_types::time::{Duration, Time, new_time};
use tidb_types::vector::{peek_bytes_as_vector_float32, zero_copy_deserialize_vector_float32};
use tidb_types::{MODE_HALF_UP, MY_DECIMAL_STRUCT_SIZE, MyDecimal, decimal_peak};

use crate::bytes::{
    decode_bytes, decode_compact_bytes, encGroupSize, encMarker, encode_bytes,
    encode_compact_bytes, realloc_bytes,
};
use crate::decimal::{decode_decimal, encode_decimal, value_size_of_decimal};
use crate::float::{decode_float, encode_float};
use crate::gobinary::{self, MaxVarintLen64};
use crate::number::{
    decode_int, decode_uint, decode_uvarint, decode_varint, encode_int, encode_uint,
    encode_uvarint, encode_varint, err_insufficient,
};

// First byte in the encoded value which specifies the encoding type.
pub const NilFlag: u8 = 0;
pub const bytesFlag: u8 = 1;
pub const compactBytesFlag: u8 = 2;
pub const intFlag: u8 = 3;
pub const uintFlag: u8 = 4;
pub const floatFlag: u8 = 5;
pub const decimalFlag: u8 = 6;
pub const durationFlag: u8 = 7;
pub const varintFlag: u8 = 8;
pub const uvarintFlag: u8 = 9;
pub const jsonFlag: u8 = 10;
pub const vectorFloat32Flag: u8 = 20;
pub const maxFlag: u8 = 250;

/// IntHandleFlag is only used to encode int handle key.
pub const IntHandleFlag: u8 = intFlag;

const sizeUint64: usize = 8;
const sizeUint8: usize = 1;
const sizeUint32: usize = 4;
const sizeFloat64: usize = 8;

/// `errors.Errorf("invalid encoded key flag %v", flag)`.
// glue: the error of the decoders on an unknown flag byte
fn err_invalid_flag(flag: u8) -> Error {
    errors::errorf("invalid encoded key flag %v", &[Arg::from(flag)])
}

/// `errors.New("invalid encoded key")`.
// glue: the error of the decoders on an empty input
fn err_invalid_key() -> Error {
    errors::new("invalid encoded key")
}

// go: preRealloc L1
fn pre_realloc(b: &mut Vec<u8>, vals: &[Datum], comparable1: bool) {
    let mut size = 0;
    for v in vals {
        match v.kind() {
            KindInt64 | KindUint64 | KindMysqlEnum | KindMysqlSet | KindMysqlBit
            | KindBinaryLiteral => size += size_int(comparable1),
            KindString | KindBytes => size += size_bytes(v.get_bytes(), comparable1),
            KindMysqlTime | KindMysqlDuration | KindFloat32 | KindFloat64 => size += 9,
            KindNull | KindMinNotNull | KindMaxValue => size += 1,
            KindMysqlJSON => size += 2 + v.get_bytes().len(),
            KindVectorFloat32 => size += 1 + v.get_vector_float32().serialized_size(),
            KindMysqlDecimal => size += 1 + MY_DECIMAL_STRUCT_SIZE,
            _ => return,
        }
    }
    realloc_bytes(b, size);
}

/// encode will encode a datum and append it to a byte slice. If comparable1 is true, the encoded bytes can be sorted as it's original order.
/// If hash is true, the encoded bytes can be checked equal as it's original value.
///
/// Like Go, `err` is one variable for the whole loop: a decimal encoding error is kept only
/// until a later datum assigns `err` again (a time, a BIT value or another decimal).
// go: encode L1 [M-ERRVAL]
fn encode(
    loc: &Arc<Location>,
    b: &mut Vec<u8>,
    vals: &[Datum],
    comparable1: bool,
) -> Result<(), Error> {
    pre_realloc(b, vals, comparable1);
    let mut err: Result<(), Error> = Ok(());
    for v in vals {
        match v.kind() {
            KindInt64 => encode_signed_int(b, v.get_int64(), comparable1),
            KindUint64 => encode_unsigned_int(b, v.get_uint64(), comparable1),
            KindFloat32 | KindFloat64 => {
                b.push(floatFlag);
                encode_float(b, v.get_float64());
            }
            KindString => encode_string(b, v, comparable1),
            KindBytes => encode_bytes_flag(b, v.get_bytes(), comparable1),
            KindMysqlTime => {
                b.push(uintFlag);
                err = encode_mysql_time(loc, v.get_mysql_time(), mysql::TypeUnspecified, b);
                err.as_ref().map_err(Error::clone)?;
            }
            KindMysqlDuration => {
                // duration may have negative value, so we cannot use String to encode directly.
                b.push(durationFlag);
                encode_int(b, v.get_mysql_duration().duration);
            }
            KindMysqlDecimal => {
                b.push(decimalFlag);
                err = encode_decimal(b, v.get_mysql_decimal(), v.length(), v.frac());
            }
            KindMysqlEnum => encode_unsigned_int(b, v.get_mysql_enum().value, comparable1),
            KindMysqlSet => encode_unsigned_int(b, v.get_mysql_set().value, comparable1),
            KindMysqlBit | KindBinaryLiteral => {
                // We don't need to handle errors here since the literal is ensured to be able to store in uint64 in convertToMysqlBit.
                let (val, e) = v.get_binary_literal().to_int(&StrictContext);
                err = e;
                terror::log(err.as_ref().err());
                encode_unsigned_int(b, val, comparable1);
            }
            KindMysqlJSON => {
                b.push(jsonFlag);
                let j = v.get_mysql_json();
                b.push(j.type_code);
                b.extend_from_slice(&j.value);
            }
            KindVectorFloat32 => {
                // Always do a small deser + ser for sanity check
                b.push(vectorFloat32Flag);
                let vf = v.get_vector_float32();
                *b = vf.serialize_to(std::mem::take(b));
            }
            KindNull => b.push(NilFlag),
            KindMinNotNull => b.push(bytesFlag),
            KindMaxValue => b.push(maxFlag),
            k => {
                return Err(errors::errorf("unsupport encode type %d", &[Arg::from(k)]));
            }
        }
    }

    err
}

/// EstimateValueSize uses to estimate the value  size of the encoded values.
// go: EstimateValueSize L1
pub fn estimate_value_size(type_ctx: &Context, val: &Datum) -> Result<isize, Error> {
    let l = match val.kind() {
        KindInt64 => value_size_of_signed_int(val.get_int64()),
        KindUint64 => value_size_of_unsigned_int(val.get_uint64()),
        KindFloat32 | KindFloat64 | KindMysqlTime | KindMysqlDuration => 9,
        KindString | KindBytes => value_size_of_bytes(val.get_bytes()),
        KindMysqlDecimal => {
            let l = value_size_of_decimal(val.get_mysql_decimal(), val.length(), val.frac())?;
            l + 1
        }
        KindMysqlEnum => value_size_of_unsigned_int(val.get_mysql_enum().value),
        KindMysqlSet => value_size_of_unsigned_int(val.get_mysql_set().value),
        KindMysqlBit | KindBinaryLiteral => {
            let (v, err) = val.get_binary_literal().to_int(type_ctx);
            terror::log(err.as_ref().err());
            value_size_of_unsigned_int(v)
        }
        KindMysqlJSON => 2 + val.get_mysql_json().value.len() as isize,
        KindVectorFloat32 => 1 + val.get_vector_float32().serialized_size() as isize,
        KindNull | KindMinNotNull | KindMaxValue => 1,
        k => {
            return Err(errors::errorf(
                "unsupported encode type %d",
                &[Arg::from(k)],
            ));
        }
    };
    Ok(l)
}

/// EncodeMySQLTime encodes datum of `KindMysqlTime` to []byte.
///
/// On error Go returns a nil slice: `b` is emptied.
// go: EncodeMySQLTime L1 [M-ERRVAL]
pub fn encode_mysql_time(
    loc: &Arc<Location>,
    mut t: Time,
    mut tp: u8,
    b: &mut Vec<u8>,
) -> Result<(), Error> {
    // Encoding timestamp need to consider timezone. If it's not in UTC, transform to UTC first.
    // This is compatible with `PBToExpr > convertTime`, and coprocessor assumes the passed timestamp is in UTC as well.
    if tp == mysql::TypeUnspecified {
        tp = t.tp();
    }
    if tp == mysql::TypeTimestamp
        && !Arc::ptr_eq(loc, &utc())
        && let Err(err) = t.convert_time_zone(loc, &utc())
    {
        *b = Vec::new();
        return Err(err);
    }
    let v = match t.to_packed_uint() {
        Ok(v) => v,
        Err(err) => {
            *b = Vec::new();
            return Err(err);
        }
    };
    encode_uint(b, v);
    Ok(())
}

// go: encodeString L1
fn encode_string(b: &mut Vec<u8>, val: &Datum, comparable1: bool) {
    if collate::new_collation_enabled() && comparable1 {
        let key = collate::get_collator(val.collation()).key(val.get_string());
        encode_bytes_flag(b, &key, true);
        return;
    }
    encode_bytes_flag(b, val.get_bytes(), comparable1);
}

/// `encodeBytes` (the flagged form; [`encode_bytes`] is `EncodeBytes`).
// go: encodeBytes L1
fn encode_bytes_flag(b: &mut Vec<u8>, v: &[u8], comparable1: bool) {
    if comparable1 {
        b.push(bytesFlag);
        encode_bytes(b, v);
    } else {
        b.push(compactBytesFlag);
        encode_compact_bytes(b, v);
    }
}

// go: valueSizeOfBytes L1
fn value_size_of_bytes(v: &[u8]) -> isize {
    value_size_of_signed_int(v.len() as i64) + v.len() as isize
}

// go: sizeBytes L1
fn size_bytes(v: &[u8], comparable1: bool) -> usize {
    if comparable1 {
        let realloc_size = (v.len() / encGroupSize + 1) * (encGroupSize + 1);
        return 1 + realloc_size;
    }
    let realloc_size = MaxVarintLen64 + v.len();
    1 + realloc_size
}

// go: encodeSignedInt L1
fn encode_signed_int(b: &mut Vec<u8>, v: i64, comparable1: bool) {
    if comparable1 {
        b.push(intFlag);
        encode_int(b, v);
    } else {
        b.push(varintFlag);
        encode_varint(b, v);
    }
}

// go: valueSizeOfSignedInt L1
fn value_size_of_signed_int(mut v: i64) -> isize {
    if v < 0 {
        v = 0i64.wrapping_sub(v).wrapping_sub(1);
    }
    // flag occupy 1 bit and at lease 1 bit.
    let mut size = 2;
    v >>= 6;
    while v > 0 {
        size += 1;
        v >>= 7;
    }
    size
}

// go: encodeUnsignedInt L1
fn encode_unsigned_int(b: &mut Vec<u8>, v: u64, comparable1: bool) {
    if comparable1 {
        b.push(uintFlag);
        encode_uint(b, v);
    } else {
        b.push(uvarintFlag);
        encode_uvarint(b, v);
    }
}

// go: valueSizeOfUnsignedInt L1
fn value_size_of_unsigned_int(mut v: u64) -> isize {
    // flag occupy 1 bit and at lease 1 bit.
    let mut size = 2;
    v >>= 7;
    while v > 0 {
        size += 1;
        v >>= 7;
    }
    size
}

// go: sizeInt L1
fn size_int(comparable1: bool) -> usize {
    if comparable1 {
        return 9;
    }
    1 + MaxVarintLen64
}

/// EncodeKey appends the encoded values to byte slice b, returns the appended
/// slice. It guarantees the encoded value is in ascending order for comparison.
/// For decimal type, datum must set datum's length and frac.
// go: EncodeKey L1 [M-ERRVAL]
pub fn encode_key(loc: &Arc<Location>, b: &mut Vec<u8>, v: &[Datum]) -> Result<(), Error> {
    encode(loc, b, v, true)
}

/// EncodeValue appends the encoded values to byte slice b, returning the appended
/// slice. It does not guarantee the order for comparison.
// go: EncodeValue L1 [M-ERRVAL]
pub fn encode_value(loc: &Arc<Location>, b: &mut Vec<u8>, v: &[Datum]) -> Result<(), Error> {
    encode(loc, b, v, false)
}

/// The native-endian image of a float64 with -0 folded into 0 (the hash functions' rule:
/// "For negative zero. In memory, 0 is [0, 0, 0, 0, 0, 0, 0, 0] and -0 is [0, 0, 0, 0, 0, 0, 0, 128].
/// It makes -0's hash val different from 0's.").
// glue: `if f == 0 { f = 0 }; unsafe.Slice((*byte)(unsafe.Pointer(&f)), 8)`
fn float_hash_bytes(mut f: f64) -> [u8; sizeFloat64] {
    if f == 0.0 {
        f = 0.0;
    }
    f.to_ne_bytes()
}

/// EncodeHashChunkRowIdx encodes value for further comparison
///
/// `b` borrows the row where Go's slice aliases chunk memory and owns computed bytes.
// go: EncodeHashChunkRowIdx L1
pub fn encode_hash_chunk_row_idx<'a>(
    type_ctx: &Context,
    row: Row<'a>,
    tp: &FieldType,
    idx: usize,
) -> Result<(u8, Cow<'a, [u8]>), Error> {
    if row.is_null(idx) {
        return Ok((NilFlag, Cow::Borrowed(&[])));
    }
    let res = match tp.get_type() {
        mysql::TypeTiny
        | mysql::TypeShort
        | mysql::TypeInt24
        | mysql::TypeLong
        | mysql::TypeLonglong
        | mysql::TypeYear => {
            let mut flag = uvarintFlag;
            if !mysql::has_unsigned_flag(tp.get_flag()) && row.get_int64(idx) < 0 {
                flag = varintFlag;
            }
            (flag, Cow::Borrowed(row.get_raw(idx)))
        }
        mysql::TypeFloat => {
            let f = f64::from(row.get_float32(idx));
            (floatFlag, Cow::Owned(float_hash_bytes(f).to_vec()))
        }
        mysql::TypeDouble => {
            let f = row.get_float64(idx);
            (floatFlag, Cow::Owned(float_hash_bytes(f).to_vec()))
        }
        mysql::TypeVarchar
        | mysql::TypeVarString
        | mysql::TypeString
        | mysql::TypeBlob
        | mysql::TypeTinyBlob
        | mysql::TypeMediumBlob
        | mysql::TypeLongBlob => {
            let b = row.get_bytes(idx);
            (compactBytesFlag, Cow::Owned(convert_by_collation(b, tp)))
        }
        mysql::TypeDate | mysql::TypeDatetime | mysql::TypeTimestamp => {
            let t = row.get_time(idx);
            let v = t.to_packed_uint()?;
            (uintFlag, Cow::Owned(v.to_ne_bytes().to_vec()))
        }
        // duration may have negative value, so we cannot use String to encode directly.
        mysql::TypeDuration => (durationFlag, Cow::Borrowed(row.get_raw(idx))),
        mysql::TypeNewDecimal => {
            // If hash is true, we only consider the original value of this decimal and ignore it's precision.
            let dec = row.get_my_decimal(idx);
            let (b, err) = dec.to_hash_key();
            err?;
            (decimalFlag, Cow::Owned(b))
        }
        mysql::TypeEnum => {
            if mysql::has_enum_set_as_int_flag(tp.get_flag()) {
                let v = row.get_enum(idx).value;
                (uvarintFlag, Cow::Owned(v.to_ne_bytes().to_vec()))
            } else {
                let v = row.get_enum(idx).value;
                // str will be empty string if v out of definition of enum.
                let str = parse_enum_value(tp.get_elems().unwrap_or(&[]), v)
                    .map(|e| e.name)
                    .unwrap_or_default();
                (compactBytesFlag, Cow::Owned(convert_by_collation(&str, tp)))
            }
        }
        mysql::TypeSet => {
            let s = parse_set_value(tp.get_elems().unwrap_or(&[]), row.get_set(idx).value)?;
            (
                compactBytesFlag,
                Cow::Owned(convert_by_collation(&s.name, tp)),
            )
        }
        mysql::TypeBit => {
            // We don't need to handle errors here since the literal is ensured to be able to store in uint64 in convertToMysqlBit.
            let (v, err1) = BinaryLiteral(row.get_bytes(idx).to_vec()).to_int(type_ctx);
            terror::log(err1.as_ref().err());
            (uvarintFlag, Cow::Owned(v.to_ne_bytes().to_vec()))
        }
        mysql::TypeJSON => {
            let json = row.get_json(idx);
            (jsonFlag, Cow::Owned(json.hash_value(Vec::new())))
        }
        mysql::TypeTiDBVectorFloat32 => {
            let v = row.get_vector_float32(idx);
            (vectorFloat32Flag, Cow::Owned(v.serialize_to(Vec::new())))
        }
        t => {
            return Err(errors::errorf(
                "unsupport column type for encode %d",
                &[Arg::from(t)],
            ));
        }
    };
    Ok(res)
}

/// SerializeMode is for some special cases during serialize key
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SerializeMode(pub isize);

/// Normal means serialize in the normal way
pub const Normal: SerializeMode = SerializeMode(0);
/// NeedSignFlag when serialize integer column, if the join key is <signed, signed> or <unsigned, unsigned>,
/// the unsigned flag can be ignored, if the join key is <unsigned, signed> or <signed, unsigned>
/// the unsigned flag can not be ignored, if the unsigned flag can not be ignored, the key can not be inlined
pub const NeedSignFlag: SerializeMode = SerializeMode(1);
/// KeepVarColumnLength when serialize var-length column, whether record the column length or not. If the join key only contains one var-length
/// column, and the key is not inlined, then no need to record the column length, otherwise, always need to record the column length
pub const KeepVarColumnLength: SerializeMode = SerializeMode(2);

/// SerializeKeys is used in join
///
/// `null_vector` is Go's `nullVector`: a NULL cell with `None` panics like Go's write to a
/// nil slice.
// go: SerializeKeys L1
#[allow(clippy::too_many_arguments)]
pub fn serialize_keys(
    type_ctx: &Context,
    chk: &Chunk,
    tp: &FieldType,
    col_idx: usize,
    used_rows: &[usize],
    filter_vector: Option<&[bool]>,
    mut null_vector: Option<&mut [bool]>,
    serialize_mode: SerializeMode,
    serialized_keys_vector: &mut [Vec<u8>],
) -> Result<(), Error> {
    let column = chk.column(col_idx);
    let mut can_skip = |index: usize| -> bool {
        if column.is_null(index) {
            match null_vector.as_deref_mut() {
                Some(nv) => nv[index] = true,
                None => panic!("runtime error: index out of range [{index}] with length 0"),
            }
        }
        filter_vector.is_some_and(|fv| !fv[index])
            || null_vector.as_deref().is_some_and(|nv| nv[index])
    };
    let mut json_hash_buffer: Vec<u8> = Vec::new();
    match tp.get_type() {
        mysql::TypeTiny
        | mysql::TypeShort
        | mysql::TypeInt24
        | mysql::TypeLong
        | mysql::TypeLonglong
        | mysql::TypeYear => {
            let i64s = column.int64s();
            for (logical_row_index, &physical_rowindex) in used_rows.iter().enumerate() {
                if can_skip(physical_rowindex) {
                    continue;
                }
                let key = &mut serialized_keys_vector[logical_row_index];
                if serialize_mode == NeedSignFlag {
                    if !mysql::has_unsigned_flag(tp.get_flag()) && i64s[physical_rowindex].get() < 0
                    {
                        key.push(intFlag);
                    } else {
                        key.push(uintFlag);
                    }
                }
                key.extend_from_slice(column.get_raw(physical_rowindex));
            }
        }
        mysql::TypeFloat => {
            let f32s = column.float32s();
            for (logical_row_index, &physical_rowindex) in used_rows.iter().enumerate() {
                if can_skip(physical_rowindex) {
                    continue;
                }
                let d = f64::from(f32s[physical_rowindex].get());
                serialized_keys_vector[logical_row_index].extend_from_slice(&float_hash_bytes(d));
            }
        }
        mysql::TypeDouble => {
            let f64s = column.float64s();
            for (logical_row_index, &physical_rowindex) in used_rows.iter().enumerate() {
                if can_skip(physical_rowindex) {
                    continue;
                }
                let f = f64s[physical_rowindex].get();
                serialized_keys_vector[logical_row_index].extend_from_slice(&float_hash_bytes(f));
            }
        }
        mysql::TypeVarchar
        | mysql::TypeVarString
        | mysql::TypeString
        | mysql::TypeBlob
        | mysql::TypeTinyBlob
        | mysql::TypeMediumBlob
        | mysql::TypeLongBlob => {
            for (logical_row_index, &physical_row_index) in used_rows.iter().enumerate() {
                if can_skip(physical_row_index) {
                    continue;
                }
                let data = convert_by_collation(column.get_bytes(physical_row_index), tp);
                let size = data.len() as u64;
                let key = &mut serialized_keys_vector[logical_row_index];
                if serialize_mode == KeepVarColumnLength {
                    key.extend_from_slice(&size.to_ne_bytes()[..sizeUint64]);
                }
                key.extend_from_slice(&data);
            }
        }
        mysql::TypeDate | mysql::TypeDatetime | mysql::TypeTimestamp => {
            for (logical_row_index, &physical_row_index) in used_rows.iter().enumerate() {
                if can_skip(physical_row_index) {
                    continue;
                }
                let v = column.get_time(physical_row_index).to_packed_uint()?;
                // don't need to check serializeMode since date/datetime/timestamp must be compared with date/datetime/timestamp, so the serializeMode must be normal
                serialized_keys_vector[logical_row_index].extend_from_slice(&v.to_ne_bytes());
            }
        }
        mysql::TypeDuration => {
            for (logical_row_index, &physical_row_index) in used_rows.iter().enumerate() {
                if can_skip(physical_row_index) {
                    continue;
                }
                // don't need to check serializeMode since duration must be compared with duration, so the serializeMode must be normal
                serialized_keys_vector[logical_row_index]
                    .extend_from_slice(column.get_raw(physical_row_index));
            }
        }
        mysql::TypeNewDecimal => {
            for (logical_row_index, &physical_rowindex) in used_rows.iter().enumerate() {
                if can_skip(physical_rowindex) {
                    continue;
                }
                let (b, err) = column.get_decimal(physical_rowindex).to_hash_key();
                err?;
                let key = &mut serialized_keys_vector[logical_row_index];
                if serialize_mode == KeepVarColumnLength {
                    // for decimal, the size must be less than uint8.MAX, so use uint8 here
                    let size = b.len() as u8;
                    key.extend_from_slice(&[size][..sizeUint8]);
                }
                key.extend_from_slice(&b);
            }
        }
        mysql::TypeEnum => {
            for (logical_row_index, &physical_rowindex) in used_rows.iter().enumerate() {
                if can_skip(physical_rowindex) {
                    continue;
                }
                let v = column.get_enum(physical_rowindex).value;
                let key = &mut serialized_keys_vector[logical_row_index];
                if mysql::has_enum_set_as_int_flag(tp.get_flag()) {
                    // check serializeMode here because enum maybe compare to integer type directly
                    if serialize_mode == NeedSignFlag {
                        key.push(uintFlag);
                    }
                    key.extend_from_slice(&v.to_ne_bytes());
                } else {
                    let str = parse_enum_value(tp.get_elems().unwrap_or(&[]), v)
                        .map(|e| e.name)
                        .unwrap_or_default();
                    let b = convert_by_collation(&str, tp);
                    if serialize_mode == KeepVarColumnLength {
                        // for enum, the size must be less than uint32.MAX, so use uint32 here
                        let size = b.len() as u32;
                        key.extend_from_slice(&size.to_ne_bytes()[..sizeUint32]);
                    }
                    key.extend_from_slice(&b);
                }
            }
        }
        mysql::TypeSet => {
            for (logical_row_index, &physical_rowindex) in used_rows.iter().enumerate() {
                if can_skip(physical_rowindex) {
                    continue;
                }
                let s = parse_set_value(
                    tp.get_elems().unwrap_or(&[]),
                    column.get_set(physical_rowindex).value,
                )?;
                let b = convert_by_collation(&s.name, tp);
                let key = &mut serialized_keys_vector[logical_row_index];
                if serialize_mode == KeepVarColumnLength {
                    // for enum, the size must be less than uint32.MAX, so use uint32 here
                    let size = b.len() as u32;
                    key.extend_from_slice(&size.to_ne_bytes()[..sizeUint32]);
                }
                key.extend_from_slice(&b);
            }
        }
        mysql::TypeBit => {
            for (logical_row_index, &physical_rowindex) in used_rows.iter().enumerate() {
                if can_skip(physical_rowindex) {
                    continue;
                }
                let (v, err1) =
                    BinaryLiteral(column.get_bytes(physical_rowindex).to_vec()).to_int(type_ctx);
                terror::log(err1.as_ref().err());
                let key = &mut serialized_keys_vector[logical_row_index];
                // check serializeMode here because bit maybe compare to integer type directly
                if serialize_mode == NeedSignFlag {
                    key.push(uintFlag);
                }
                key.extend_from_slice(&v.to_ne_bytes()[..sizeUint64]);
            }
        }
        mysql::TypeJSON => {
            for (logical_row_index, &physical_rowindex) in used_rows.iter().enumerate() {
                if can_skip(physical_rowindex) {
                    continue;
                }
                json_hash_buffer.clear();
                json_hash_buffer = column
                    .get_json(physical_rowindex)
                    .hash_value(std::mem::take(&mut json_hash_buffer));
                let key = &mut serialized_keys_vector[logical_row_index];
                if serialize_mode == KeepVarColumnLength {
                    let size = json_hash_buffer.len() as u64;
                    key.extend_from_slice(&size.to_ne_bytes()[..sizeUint64]);
                }
                key.extend_from_slice(&json_hash_buffer);
            }
        }
        mysql::TypeNull => {
            for &physical_rowindex in used_rows {
                if can_skip(physical_rowindex) {
                    continue;
                }
            }
        }
        t => {
            return Err(errors::errorf(
                "unsupport column type for encode %d",
                &[Arg::from(t)],
            ));
        }
    }
    Ok(())
}

/// HashChunkColumns writes the encoded value of each row's column, which of index `colIdx`, to h.
// go: HashChunkColumns L1
pub fn hash_chunk_columns<H: Hasher>(
    type_ctx: &Context,
    h: &mut [H],
    chk: &Chunk,
    tp: &FieldType,
    col_idx: usize,
    buf: &mut [u8],
    is_null: &mut [bool],
) -> Result<(), Error> {
    hash_chunk_selected(type_ctx, h, chk, tp, col_idx, buf, is_null, None, false)
}

/// HashChunkSelected writes the encoded value of selected row's column, which of index `colIdx`, to h.
/// sel indicates which rows are selected. If it is nil, all rows are selected.
///
/// Go writes the whole of `buf` (whose first byte is the flag) and then the value bytes.
// go: HashChunkSelected L1
#[allow(clippy::too_many_arguments)]
pub fn hash_chunk_selected<H: Hasher>(
    type_ctx: &Context,
    h: &mut [H],
    chk: &Chunk,
    tp: &FieldType,
    col_idx: usize,
    buf: &mut [u8],
    is_null: &mut [bool],
    sel: Option<&[bool]>,
    ignore_null: bool,
) -> Result<(), Error> {
    let column = chk.column(col_idx);
    let rows = chk.num_rows();
    let skip = |i: usize| sel.is_some_and(|s| !s[i]);
    // One row: the flag byte in buf[0], then the value bytes, written to h[i].
    let write = |h: &mut [H], buf: &mut [u8], i: usize, flag: u8, b: &[u8]| {
        buf[0] = flag;
        // As the golang doc described, `Hash.Write` never returns an error.
        // See https://golang.org/pkg/hash/#Hash
        h[i].write(buf);
        h[i].write(b);
    };
    match tp.get_type() {
        mysql::TypeTiny
        | mysql::TypeShort
        | mysql::TypeInt24
        | mysql::TypeLong
        | mysql::TypeLonglong
        | mysql::TypeYear => {
            let i64s = column.int64s();
            for (i, v) in i64s.iter().enumerate() {
                if skip(i) {
                    continue;
                }
                if column.is_null(i) {
                    is_null[i] = !ignore_null;
                    write(h, buf, i, NilFlag, &[]);
                } else {
                    let mut flag = uvarintFlag;
                    if !mysql::has_unsigned_flag(tp.get_flag()) && v.get() < 0 {
                        flag = varintFlag;
                    }
                    write(h, buf, i, flag, column.get_raw(i));
                }
            }
        }
        mysql::TypeFloat => {
            let f32s = column.float32s();
            for (i, f) in f32s.iter().enumerate() {
                if skip(i) {
                    continue;
                }
                if column.is_null(i) {
                    is_null[i] = !ignore_null;
                    write(h, buf, i, NilFlag, &[]);
                } else {
                    let d = f64::from(f.get());
                    write(h, buf, i, floatFlag, &float_hash_bytes(d));
                }
            }
        }
        mysql::TypeDouble => {
            let f64s = column.float64s();
            for i in 0..f64s.len() {
                let f = f64s[i].get();
                if skip(i) {
                    continue;
                }
                if column.is_null(i) {
                    is_null[i] = !ignore_null;
                    write(h, buf, i, NilFlag, &[]);
                } else {
                    write(h, buf, i, floatFlag, &float_hash_bytes(f));
                }
            }
        }
        mysql::TypeVarchar
        | mysql::TypeVarString
        | mysql::TypeString
        | mysql::TypeBlob
        | mysql::TypeTinyBlob
        | mysql::TypeMediumBlob
        | mysql::TypeLongBlob => {
            for i in 0..rows {
                if skip(i) {
                    continue;
                }
                if column.is_null(i) {
                    is_null[i] = !ignore_null;
                    write(h, buf, i, NilFlag, &[]);
                } else {
                    let b = convert_by_collation(column.get_bytes(i), tp);
                    write(h, buf, i, compactBytesFlag, &b);
                }
            }
        }
        mysql::TypeDate | mysql::TypeDatetime | mysql::TypeTimestamp => {
            for i in 0..column.times().len() {
                if skip(i) {
                    continue;
                }
                if column.is_null(i) {
                    is_null[i] = !ignore_null;
                    write(h, buf, i, NilFlag, &[]);
                } else {
                    let v = column.get_time(i).to_packed_uint()?;
                    write(h, buf, i, uintFlag, &v.to_ne_bytes());
                }
            }
        }
        mysql::TypeDuration => {
            for i in 0..rows {
                if skip(i) {
                    continue;
                }
                if column.is_null(i) {
                    is_null[i] = !ignore_null;
                    write(h, buf, i, NilFlag, &[]);
                } else {
                    // duration may have negative value, so we cannot use String to encode directly.
                    write(h, buf, i, durationFlag, column.get_raw(i));
                }
            }
        }
        mysql::TypeNewDecimal => {
            for i in 0..column.decimals().len() {
                if skip(i) {
                    continue;
                }
                if column.is_null(i) {
                    is_null[i] = !ignore_null;
                    write(h, buf, i, NilFlag, &[]);
                } else {
                    // If hash is true, we only consider the original value of this decimal and ignore it's precision.
                    let (b, err) = column.get_decimal(i).to_hash_key();
                    err?;
                    write(h, buf, i, decimalFlag, &b);
                }
            }
        }
        mysql::TypeEnum => {
            for i in 0..rows {
                if skip(i) {
                    continue;
                }
                if column.is_null(i) {
                    is_null[i] = !ignore_null;
                    write(h, buf, i, NilFlag, &[]);
                } else if mysql::has_enum_set_as_int_flag(tp.get_flag()) {
                    let v = column.get_enum(i).value;
                    write(h, buf, i, uvarintFlag, &v.to_ne_bytes()[..sizeUint64]);
                } else {
                    let v = column.get_enum(i).value;
                    // str will be empty string if v out of definition of enum.
                    let str = parse_enum_value(tp.get_elems().unwrap_or(&[]), v)
                        .map(|e| e.name)
                        .unwrap_or_default();
                    let b = convert_by_collation(&str, tp);
                    write(h, buf, i, compactBytesFlag, &b);
                }
            }
        }
        mysql::TypeSet => {
            for i in 0..rows {
                if skip(i) {
                    continue;
                }
                if column.is_null(i) {
                    is_null[i] = !ignore_null;
                    write(h, buf, i, NilFlag, &[]);
                } else {
                    let s =
                        parse_set_value(tp.get_elems().unwrap_or(&[]), column.get_set(i).value)?;
                    let b = convert_by_collation(&s.name, tp);
                    write(h, buf, i, compactBytesFlag, &b);
                }
            }
        }
        mysql::TypeBit => {
            for i in 0..rows {
                if skip(i) {
                    continue;
                }
                if column.is_null(i) {
                    is_null[i] = !ignore_null;
                    write(h, buf, i, NilFlag, &[]);
                } else {
                    // We don't need to handle errors here since the literal is ensured to be able to store in uint64 in convertToMysqlBit.
                    let (v, err1) = BinaryLiteral(column.get_bytes(i).to_vec()).to_int(type_ctx);
                    terror::log(err1.as_ref().err());
                    write(h, buf, i, uvarintFlag, &v.to_ne_bytes()[..sizeUint64]);
                }
            }
        }
        mysql::TypeJSON => {
            for i in 0..rows {
                if skip(i) {
                    continue;
                }
                if column.is_null(i) {
                    is_null[i] = !ignore_null;
                    write(h, buf, i, NilFlag, &[]);
                } else {
                    let json = column.get_json(i);
                    let b = json.hash_value(Vec::new());
                    write(h, buf, i, jsonFlag, &b);
                }
            }
        }
        mysql::TypeTiDBVectorFloat32 => {
            for i in 0..rows {
                if skip(i) {
                    continue;
                }
                if column.is_null(i) {
                    is_null[i] = !ignore_null;
                    write(h, buf, i, NilFlag, &[]);
                } else {
                    let v = column.get_vector_float32(i);
                    let b = v.serialize_to(Vec::new());
                    write(h, buf, i, vectorFloat32Flag, &b);
                }
            }
        }
        mysql::TypeNull => {
            for i in 0..rows {
                if skip(i) {
                    continue;
                }
                is_null[i] = !ignore_null;
                buf[0] = NilFlag;
                h[i].write(buf);
            }
        }
        t => {
            return Err(errors::errorf(
                "unsupport column type for encode %d",
                &[Arg::from(t)],
            ));
        }
    }
    Ok(())
}

/// `io.Writer.Write`: the writer's error as an `Error` with the same text.
// glue: io.Writer error to Error
fn write_all(w: &mut impl std::io::Write, p: &[u8]) -> Result<(), Error> {
    w.write_all(p).map_err(|e| errors::new(e.to_string()))
}

/// HashChunkRow writes the encoded values to w.
/// If two rows are logically equal, it will generate the same bytes.
// go: HashChunkRow L1
pub fn hash_chunk_row(
    type_ctx: &Context,
    w: &mut impl std::io::Write,
    row: Row<'_>,
    all_types: &[FieldType],
    col_idx: &[usize],
    buf: &mut [u8],
) -> Result<(), Error> {
    for (i, &idx) in col_idx.iter().enumerate() {
        let (flag, b) = encode_hash_chunk_row_idx(type_ctx, row, &all_types[i], idx)?;
        buf[0] = flag;
        write_all(w, buf)?;
        write_all(w, &b)?;
    }
    Ok(())
}

/// EqualChunkRow returns a boolean reporting whether row1 and row2
/// with their types and column index are logically equal.
// go: EqualChunkRow L1
pub fn equal_chunk_row(
    type_ctx: &Context,
    row1: Row<'_>,
    all_types1: &[FieldType],
    col_idx1: &[usize],
    row2: Row<'_>,
    all_types2: &[FieldType],
    col_idx2: &[usize],
) -> Result<bool, Error> {
    if col_idx1.len() != col_idx2.len() {
        return Err(errors::errorf(
            "Internal error: Hash columns count mismatch, col1: %d, col2: %d",
            &[
                Arg::from(col_idx1.len() as isize),
                Arg::from(col_idx2.len() as isize),
            ],
        ));
    }
    for i in 0..col_idx1.len() {
        let (idx1, idx2) = (col_idx1[i], col_idx2[i]);
        let (flag1, b1) = encode_hash_chunk_row_idx(type_ctx, row1, &all_types1[i], idx1)?;
        let (flag2, b2) = encode_hash_chunk_row_idx(type_ctx, row2, &all_types2[i], idx2)?;
        if !(flag1 == flag2 && b1 == b2) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Decode decodes values from a byte slice generated with EncodeKey or EncodeValue
/// before.
/// size is the size of decoded datum slice.
// go: Decode L1
pub fn decode(mut b: &[u8], size: usize) -> Result<Vec<Datum>, Error> {
    if b.is_empty() {
        return Err(err_invalid_key());
    }

    let mut values = Vec::with_capacity(size);

    while !b.is_empty() {
        let (rest, d, err) = decode_one(b);
        err?;
        b = rest;

        values.push(d);
    }

    Ok(values)
}

/// DecodeRange decodes the range values from a byte slice that generated by EncodeKey.
/// It handles some special values like `MinNotNull` and `MaxValueDatum`.
/// loc can be nil and only used in when the corresponding type is `mysql.TypeTimestamp`.
///
/// Returns `(values, remain, err)` as Go does: on error the values decoded so far and the
/// remaining input; on success Go's remain is nil (an empty slice here).
// go: DecodeRange L1 [M-ERRVAL]
pub fn decode_range<'a>(
    mut b: &'a [u8],
    size: usize,
    idx_column_types: Option<&[u8]>,
    loc: Option<&Arc<Location>>,
) -> (Vec<Datum>, &'a [u8], Result<(), Error>) {
    if b.is_empty() {
        return (
            Vec::new(),
            b,
            Err(errors::new("invalid encoded key: length of key is zero")),
        );
    }

    let mut values = Vec::with_capacity(size);

    let mut i = 0;
    while b.len() > 1 {
        let (rest, d, err) = match idx_column_types {
            None => decode_one(b),
            Some(types) => {
                if i >= types.len() {
                    return (
                        values,
                        b,
                        Err(errors::new("invalid length of index's columns")),
                    );
                }
                if is_type_time(types[i]) {
                    // handle datetime values specially since they are encoded to int and we'll get int values if using DecodeOne.
                    decode_as_date_time(b, types[i], loc)
                } else if is_type_float(types[i]) {
                    decode_as_float32(b, types[i])
                } else {
                    decode_one(b)
                }
            }
        };
        b = rest;
        if let Err(err) = err {
            return (values, b, Err(err));
        }
        values.push(d);
        i += 1;
    }

    if b.len() == 1 {
        match b[0] {
            NilFlag => values.push(Datum::default()),
            bytesFlag => values.push(min_not_null_datum()),
            // `maxFlag + 1` for PrefixNext
            f if f == maxFlag || f == maxFlag + 1 => values.push(max_value_datum()),
            f => {
                return (
                    values,
                    b,
                    Err(errors::errorf(
                        "invalid encoded key flag %v",
                        &[Arg::from(f)],
                    )),
                );
            }
        }
    }
    (values, &[], Ok(()))
}

/// DecodeOne decodes on datum from a byte slice generated with EncodeKey or EncodeValue.
///
/// Returns `(remain, d, err)` as Go does: on error `remain` and `d` are what Go returns with
/// it (for example `DecodeInt`'s nil remain and the `Int64(0)` datum set before the check).
// go: DecodeOne L1 [M-ERRVAL]
pub fn decode_one(b: &[u8]) -> (&[u8], Datum, Result<(), Error>) {
    let mut d = Datum::default();
    if b.is_empty() {
        return (&[], d, Err(err_invalid_key()));
    }
    let flag = b[0];
    let b = &b[1..];
    // Go's decoders return a nil remain and a zero value with their error.
    let res: (&[u8], Result<(), Error>) = match flag {
        intFlag => match decode_int(b) {
            Ok((b, v)) => {
                d.set_int64(v);
                (b, Ok(()))
            }
            Err(e) => {
                d.set_int64(0);
                (&[], Err(e))
            }
        },
        uintFlag => match decode_uint(b) {
            Ok((b, v)) => {
                d.set_uint64(v);
                (b, Ok(()))
            }
            Err(e) => {
                d.set_uint64(0);
                (&[], Err(e))
            }
        },
        varintFlag => match decode_varint(b) {
            Ok((b, v)) => {
                d.set_int64(v);
                (b, Ok(()))
            }
            Err(e) => {
                d.set_int64(0);
                (&[], Err(e))
            }
        },
        uvarintFlag => match decode_uvarint(b) {
            Ok((b, v)) => {
                d.set_uint64(v);
                (b, Ok(()))
            }
            Err(e) => {
                d.set_uint64(0);
                (&[], Err(e))
            }
        },
        floatFlag => match decode_float(b) {
            Ok((b, v)) => {
                d.set_float64(v);
                (b, Ok(()))
            }
            Err(e) => {
                // decodeCmpUintToFloat(0): ^0 as float64 bits, a NaN.
                d.set_float64(f64::from_bits(!0));
                (&[], Err(e))
            }
        },
        bytesFlag => match decode_bytes(b, None) {
            Ok((b, v)) => {
                d.set_bytes(v);
                (b, Ok(()))
            }
            Err(e) => {
                d.set_bytes(None);
                (&[], Err(e))
            }
        },
        compactBytesFlag => match decode_compact_bytes(b) {
            Ok((b, v)) => {
                d.set_bytes(v.to_vec());
                (b, Ok(()))
            }
            Err(e) => {
                d.set_bytes(None);
                (&[], Err(e))
            }
        },
        decimalFlag => {
            let (b, dec, precision, frac, err) = decode_decimal(b);
            if let (Ok(()), Some(dec)) = (&err, dec) {
                d.set_mysql_decimal(dec);
                d.set_length(precision);
                d.set_frac(frac);
            }
            (b, err)
        }
        durationFlag => match decode_int(b) {
            Ok((b, r)) => {
                // use max fsp, let outer to do round manually.
                let v = Duration {
                    duration: r,
                    fsp: MaxFsp,
                };
                d.set_mysql_duration(v);
                (b, Ok(()))
            }
            Err(e) => (&[], Err(e)),
        },
        jsonFlag => {
            let size = match peek_bytes_as_json(b) {
                Ok(size) => size as usize,
                Err(err) => return (b, d, Err(err)),
            };
            let j = BinaryJSON::borrowed(b[0], &b[1..size]);
            d.set_mysql_json(j);
            (&b[size..], Ok(()))
        }
        vectorFloat32Flag => match zero_copy_deserialize_vector_float32(b) {
            Ok((v, remaining)) => {
                d.set_vector_float32(v);
                (remaining, Ok(()))
            }
            Err(err) => return (b, d, Err(err)),
        },
        NilFlag => (b, Ok(())),
        _ => return (b, d, Err(err_invalid_flag(flag))),
    };
    let (b, err) = res;
    (b, d, err)
}

/// DecodeAsDateTime decodes on datum from []byte of `KindMysqlTime`.
// go: DecodeAsDateTime L1 [M-ERRVAL]
pub fn decode_as_date_time<'a>(
    b: &'a [u8],
    tp: u8,
    loc: Option<&Arc<Location>>,
) -> (&'a [u8], Datum, Result<(), Error>) {
    let mut d = Datum::default();
    if b.is_empty() {
        return (&[], d, Err(err_invalid_key()));
    }
    let flag = b[0];
    let b = &b[1..];
    let decoded = match flag {
        uintFlag => decode_uint(b),
        // Datetime can be encoded as Uvarint
        uvarintFlag => decode_uvarint(b),
        // null value should also be decoded out.
        NilFlag => return (b, d, Ok(())),
        _ => return (b, d, Err(err_invalid_flag(flag))),
    };
    let (b, v) = match decoded {
        Ok(x) => x,
        Err(err) => return (&[], d, Err(err)),
    };
    let mut t = new_time(ZeroCoreTime, tp, 0);
    if let Err(err) = t.from_packed_uint(v) {
        return (b, d, Err(err));
    }
    if tp == mysql::TypeTimestamp
        && !t.is_zero()
        && let Some(loc) = loc
        && let Err(err) = t.convert_time_zone(&utc(), loc)
    {
        return (b, d, Err(err));
    }
    d.set_mysql_time(t);
    (b, d, Ok(()))
}

/// DecodeAsFloat32 decodes value for mysql.TypeFloat
// go: DecodeAsFloat32 L1 [M-ERRVAL]
pub fn decode_as_float32(b: &[u8], tp: u8) -> (&[u8], Datum, Result<(), Error>) {
    let mut d = Datum::default();
    if b.is_empty() || tp != mysql::TypeFloat {
        return (&[], d, Err(err_invalid_key()));
    }
    let flag = b[0];
    let b = &b[1..];
    if flag != floatFlag {
        return (
            b,
            d,
            Err(errors::errorf(
                "invalid encoded key flag %v for DecodeAsFloat32",
                &[Arg::from(flag)],
            )),
        );
    }
    match decode_float(b) {
        Ok((b, v)) => {
            d.set_float32_from_f64(v);
            (b, d, Ok(()))
        }
        Err(err) => (&[], d, Err(err)),
    }
}

/// CutOne cuts the first encoded value from b.
/// It will return the first encoded item and the remains as byte slice.
// go: CutOne L1
pub fn cut_one(b: &[u8]) -> Result<(&[u8], &[u8]), Error> {
    let l = peek(b)? as usize;
    Ok((&b[..l], &b[l..]))
}

/// CutColumnID cuts the column ID from b.
/// It will return the remains as byte slice and column ID
// go: CutColumnID L1
pub fn cut_column_id(b: &[u8]) -> Result<(&[u8], i64), Error> {
    if b.is_empty() {
        return Err(err_invalid_key());
    }
    // skip the flag
    decode_varint(&b[1..])
}

/// SetRawValues set raw datum values from a row data.
// go: SetRawValues L1
pub fn set_raw_values(mut data: &[u8], values: &mut [Datum]) -> Result<(), Error> {
    for v in values {
        let l = peek(data)? as usize;
        v.set_raw(data[..l].to_vec());
        data = &data[l..];
    }
    Ok(())
}

/// peek peeks the first encoded value from b and returns its length.
// go: peek L1
fn peek(b: &[u8]) -> Result<isize, Error> {
    let origin_length = b.len() as isize;
    if b.is_empty() {
        return Err(err_invalid_key());
    }
    let flag = b[0];
    let mut length: isize = 1;
    let b = &b[1..];
    let l = match flag {
        NilFlag => 0,
        // Those types are stored in 8 bytes.
        intFlag | uintFlag | floatFlag | durationFlag => 8,
        bytesFlag => peek_bytes(b)?,
        compactBytesFlag => peek_compact_bytes(b)?,
        decimalFlag => decimal_peak(b)?,
        varintFlag => peek_varint(b)?,
        uvarintFlag => peek_uvarint(b)?,
        jsonFlag => peek_bytes_as_json(b)?,
        vectorFloat32Flag => peek_bytes_as_vector_float32(b)? as isize,
        _ => return Err(err_invalid_flag(flag)),
    };
    length += l;
    if length <= 0 {
        return Err(err_invalid_key());
    } else if length > origin_length {
        return Err(errors::errorf(
            "invalid encoded key, expected length: %d, actual length: %d",
            &[Arg::from(length), Arg::from(origin_length)],
        ));
    }
    Ok(length)
}

// go: peekBytes L1
fn peek_bytes(b: &[u8]) -> Result<isize, Error> {
    let mut offset = 0;
    loop {
        if b.len() < offset + encGroupSize + 1 {
            return Err(err_insufficient());
        }
        // The byte slice is encoded into many groups.
        // For each group, there are 8 bytes for data and 1 byte for marker.
        let marker = b[offset + encGroupSize];
        let pad_count = encMarker.wrapping_sub(marker);
        offset += encGroupSize + 1;
        // When padCount is not zero, it means we get the end of the byte slice.
        if pad_count != 0 {
            break;
        }
    }
    Ok(offset as isize)
}

// go: peekCompactBytes L1
fn peek_compact_bytes(b: &[u8]) -> Result<isize, Error> {
    // Get length.
    let (v, n) = gobinary::varint(b);
    let vi = v as isize;
    if n < 0 {
        return Err(errors::new("value larger than 64 bits"));
    } else if n == 0 {
        return Err(err_insufficient());
    }
    if (b.len() as isize) < vi.wrapping_add(n) {
        return Err(errors::errorf(
            "insufficient bytes to decode value, expected length: %v",
            &[Arg::from(n)],
        ));
    }
    Ok(n.wrapping_add(vi))
}

// go: peekVarint L1
fn peek_varint(b: &[u8]) -> Result<isize, Error> {
    let (_, n) = gobinary::varint(b);
    if n < 0 {
        return Err(errors::new("value larger than 64 bits"));
    }
    Ok(n)
}

// go: peekUvarint L1
fn peek_uvarint(b: &[u8]) -> Result<isize, Error> {
    let (_, n) = gobinary::uvarint(b);
    if n < 0 {
        return Err(errors::new("value larger than 64 bits"));
    }
    Ok(n)
}

/// Decoder is used to decode value to chunk.
pub struct Decoder<'c> {
    chk: &'c mut Chunk,
    timezone: Arc<Location>,

    /// buf is only used for DecodeBytes to avoid the cost of makeslice. `None` is Go's nil.
    buf: Option<Vec<u8>>,
}

/// NewDecoder creates a Decoder.
// go: NewDecoder L1
pub fn new_decoder(chk: &mut Chunk, timezone: Arc<Location>) -> Decoder<'_> {
    Decoder {
        chk,
        timezone,
        buf: None,
    }
}

impl Decoder<'_> {
    /// The chunk values are decoded into.
    // glue: access to Decoder.chk
    pub fn chunk(&mut self) -> &mut Chunk {
        self.chk
    }

    /// DecodeOne decodes one value to chunk and returns the remained bytes.
    // go: Decoder.DecodeOne L1
    pub fn decode_one<'b>(
        &mut self,
        b: &'b [u8],
        col_idx: usize,
        ft: &FieldType,
    ) -> Result<&'b [u8], Error> {
        if b.is_empty() {
            return Err(err_invalid_key());
        }
        let chk = &mut *self.chk;
        let flag = b[0];
        let b = &b[1..];
        let b = match flag {
            intFlag => {
                let (b, v) = decode_int(b)?;
                append_int_to_chunk(v, chk, col_idx, ft);
                b
            }
            uintFlag => {
                let (b, v) = decode_uint(b)?;
                append_uint_to_chunk(v, chk, col_idx, ft, &self.timezone)?;
                b
            }
            varintFlag => {
                let (b, v) = decode_varint(b)?;
                append_int_to_chunk(v, chk, col_idx, ft);
                b
            }
            uvarintFlag => {
                let (b, v) = decode_uvarint(b)?;
                append_uint_to_chunk(v, chk, col_idx, ft, &self.timezone)?;
                b
            }
            floatFlag => {
                let (b, v) = decode_float(b)?;
                append_float_to_chunk(v, chk, col_idx, ft);
                b
            }
            bytesFlag => match decode_bytes(b, self.buf.take()) {
                Ok((b, buf)) => {
                    chk.append_bytes(col_idx, &buf);
                    self.buf = Some(buf);
                    b
                }
                // Go assigns the nil buffer DecodeBytes returns with its error.
                Err(err) => return Err(err),
            },
            compactBytesFlag => {
                let (b, v) = decode_compact_bytes(b)?;
                chk.append_bytes(col_idx, v);
                b
            }
            decimalFlag => {
                let (b, dec, _, frac, err) = decode_decimal(b);
                err?;
                let mut dec = dec.expect("DecodeDecimal returns a decimal without an error");
                if ft.get_decimal() != tidb_parser::types::UnspecifiedLength
                    && frac > ft.get_decimal()
                {
                    let mut to = MyDecimal::default();
                    dec.round(&mut to, ft.get_decimal(), MODE_HALF_UP)?;
                    dec = to;
                }
                chk.append_my_decimal(col_idx, &dec);
                b
            }
            durationFlag => {
                let (b, r) = decode_int(b)?;
                let v = Duration {
                    duration: r,
                    fsp: ft.get_decimal(),
                };
                chk.append_duration(col_idx, v);
                b
            }
            jsonFlag => {
                let size = peek_bytes_as_json(b)? as usize;
                chk.append_json(col_idx, &BinaryJSON::borrowed(b[0], &b[1..size]));
                &b[size..]
            }
            vectorFloat32Flag => {
                let (v, remaining) = zero_copy_deserialize_vector_float32(b)?;
                chk.append_vector_float32(col_idx, &v);
                remaining
            }
            NilFlag => {
                chk.append_null(col_idx);
                b
            }
            _ => return Err(err_invalid_flag(flag)),
        };
        Ok(b)
    }
}

// go: appendIntToChunk L1
fn append_int_to_chunk(val: i64, chk: &mut Chunk, col_idx: usize, ft: &FieldType) {
    match ft.get_type() {
        mysql::TypeDuration => {
            let v = Duration {
                duration: val,
                fsp: ft.get_decimal(),
            };
            chk.append_duration(col_idx, v);
        }
        _ => chk.append_int64(col_idx, val),
    }
}

// go: appendUintToChunk L1
fn append_uint_to_chunk(
    val: u64,
    chk: &mut Chunk,
    col_idx: usize,
    ft: &FieldType,
    loc: &Arc<Location>,
) -> Result<(), Error> {
    match ft.get_type() {
        mysql::TypeDate | mysql::TypeDatetime | mysql::TypeTimestamp => {
            let mut t = new_time(ZeroCoreTime, ft.get_type(), ft.get_decimal());
            t.from_packed_uint(val)?;
            if ft.get_type() == mysql::TypeTimestamp && !t.is_zero() {
                t.convert_time_zone(&utc(), loc)?;
            }
            chk.append_time(col_idx, t);
        }
        mysql::TypeEnum => {
            // ignore error deliberately, to read empty enum value.
            let enum_ = parse_enum_value(ft.get_elems().unwrap_or(&[]), val).unwrap_or_default();
            chk.append_enum(col_idx, &enum_);
        }
        mysql::TypeSet => {
            let set = parse_set_value(ft.get_elems().unwrap_or(&[]), val)?;
            chk.append_set(col_idx, &set);
        }
        mysql::TypeBit => {
            let byte_size = (ft.get_flen() + 7) >> 3;
            chk.append_bytes(col_idx, &new_binary_literal_from_uint(val, byte_size).0);
        }
        _ => chk.append_uint64(col_idx, val),
    }
    Ok(())
}

// go: appendFloatToChunk L1
fn append_float_to_chunk(val: f64, chk: &mut Chunk, col_idx: usize, ft: &FieldType) {
    if ft.get_type() == mysql::TypeFloat {
        chk.append_float32(col_idx, val as f32);
    } else {
        chk.append_float64(col_idx, val);
    }
}

/// HashGroupKey encodes each row of this column and append encoded data into buf.
/// Only use in the aggregate executor.
///
/// On an unsupported eval type Go returns a nil `buf` with the error; the port leaves `buf`
/// as it is (the caller discards it with the error).
// go: HashGroupKey L1 [M-ERRVAL]
pub fn hash_group_key(
    loc: &Arc<Location>,
    n: usize,
    col: &Column,
    buf: &mut [Vec<u8>],
    ft: &FieldType,
) -> Result<(), Error> {
    let et = ft.eval_type();
    match et {
        ETInt => {
            let i64s = col.int64s();
            for i in 0..n {
                if col.is_null(i) {
                    buf[i].push(NilFlag);
                } else {
                    encode_signed_int(&mut buf[i], i64s[i].get(), false);
                }
            }
        }
        ETReal => {
            let f64s = col.float64s();
            for i in 0..n {
                if col.is_null(i) {
                    buf[i].push(NilFlag);
                } else {
                    buf[i].push(floatFlag);
                    encode_float(&mut buf[i], f64s[i].get());
                }
            }
        }
        ETDecimal => {
            for i in 0..n {
                if col.is_null(i) {
                    buf[i].push(NilFlag);
                } else {
                    buf[i].push(decimalFlag);
                    encode_decimal(
                        &mut buf[i],
                        &col.get_decimal(i),
                        ft.get_flen(),
                        ft.get_decimal(),
                    )?;
                }
            }
        }
        ETDatetime | ETTimestamp => {
            for i in 0..n {
                if col.is_null(i) {
                    buf[i].push(NilFlag);
                } else {
                    buf[i].push(uintFlag);
                    encode_mysql_time(loc, col.get_time(i), mysql::TypeUnspecified, &mut buf[i])?;
                }
            }
        }
        ETDuration => {
            let ds = col.go_durations();
            for i in 0..n {
                if col.is_null(i) {
                    buf[i].push(NilFlag);
                } else {
                    buf[i].push(durationFlag);
                    encode_int(&mut buf[i], ds[i].get());
                }
            }
        }
        ETJson => {
            for i in 0..n {
                if col.is_null(i) {
                    buf[i].push(NilFlag);
                } else {
                    buf[i].push(jsonFlag);
                    buf[i] = col.get_json(i).hash_value(std::mem::take(&mut buf[i]));
                }
            }
        }
        ETString => {
            for i in 0..n {
                if col.is_null(i) {
                    buf[i].push(NilFlag);
                } else {
                    let key = convert_by_collation(col.get_bytes(i), ft);
                    encode_bytes_flag(&mut buf[i], &key, false);
                }
            }
        }
        ETVectorFloat32 => {
            for i in 0..n {
                if col.is_null(i) {
                    buf[i].push(NilFlag);
                } else {
                    buf[i] = col
                        .get_vector_float32(i)
                        .serialize_to(std::mem::take(&mut buf[i]));
                }
            }
        }
        _ => {
            return Err(errors::errorf(
                "unsupported type %s during evaluation",
                &[Arg::Stringer(
                    et.to_string().as_bytes().to_vec(),
                    "types.EvalType",
                )],
            ));
        }
    }
    Ok(())
}

/// ConvertByCollation converts these bytes according to its collation.
// go: ConvertByCollation L1
pub fn convert_by_collation(raw: &[u8], tp: &FieldType) -> Vec<u8> {
    let collator = collate::get_collator(tp.get_collate());
    collator.key(raw)
}

/// ConvertByCollationStr converts this string according to its collation.
// go: ConvertByCollationStr L1
pub fn convert_by_collation_str(str: &[u8], tp: &FieldType) -> Vec<u8> {
    let collator = collate::get_collator(tp.get_collate());
    collator.key(str)
}

/// Hash64 is for datum hash64 calculation.
// go: Hash64 L2 (IHasher has no Cache/SetCache: the encoding is built in a fresh vector)
pub fn hash64(h: &mut dyn IHasher, d: &Datum) {
    let mut b = Vec::new();
    hash_code(&mut b, d);
    h.hash_bytes(&b);
}

/// Sets `types.Hash64ForDatum` (Go's package `init`). Idempotent; the program calls it at
/// start-up, before any `Datum::hash64`.
// go: init L2 (no package initialisers in Rust: called explicitly)
pub fn init() {
    let _ = Hash64ForDatum.set(hash64);
}

/// HashCode encodes a Datum into a unique byte slice.
/// It is mostly the same as EncodeValue, but it doesn't contain truncation or verification logic in order to make the encoding lossless.
// go: HashCode L1
pub fn hash_code(b: &mut Vec<u8>, d: &Datum) {
    match d.kind() {
        KindInt64 => encode_signed_int(b, d.get_int64(), false),
        KindUint64 => encode_unsigned_int(b, d.get_uint64(), false),
        KindFloat32 | KindFloat64 => {
            b.push(floatFlag);
            encode_float(b, d.get_float64());
        }
        KindString => encode_string(b, d, false),
        KindBytes => encode_bytes_flag(b, d.get_bytes(), false),
        KindMysqlTime => {
            b.push(uintFlag);
            let t = d.get_mysql_time().core_time();
            encode_unsigned_int(b, t.0, true);
        }
        KindMysqlDuration => {
            // duration may have negative value, so we cannot use String to encode directly.
            b.push(durationFlag);
            encode_int(b, d.get_mysql_duration().duration);
        }
        KindMysqlDecimal => {
            b.push(decimalFlag);
            let dec_str = d.get_mysql_decimal().to_string();
            encode_bytes_flag(b, &dec_str, false);
        }
        KindMysqlEnum => encode_unsigned_int(b, d.get_mysql_enum().value, false),
        KindMysqlSet => encode_unsigned_int(b, d.get_mysql_set().value, false),
        KindMysqlBit | KindBinaryLiteral => {
            let val = d.get_binary_literal();
            encode_bytes_flag(b, &val.0, false);
        }
        KindMysqlJSON => {
            b.push(jsonFlag);
            let j = d.get_mysql_json();
            b.push(j.type_code);
            b.extend_from_slice(&j.value);
        }
        KindVectorFloat32 => {
            b.push(vectorFloat32Flag);
            let v = d.get_vector_float32();
            *b = v.serialize_to(std::mem::take(b));
        }
        KindNull => b.push(NilFlag),
        KindMinNotNull => b.push(bytesFlag),
        KindMaxValue => b.push(maxFlag),
        k => {
            log::warn!(
                "trying to calculate HashCode of an unexpected type of Datum; Datum Kind={k}; stack={}",
                std::backtrace::Backtrace::capture()
            );
        }
    }
}
