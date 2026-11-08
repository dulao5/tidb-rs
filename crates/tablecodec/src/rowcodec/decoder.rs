//! go-file: pkg/util/rowcodec/decoder.go
//!
//! The row format v2 decoders: to a datum map, to a chunk, and to old-format datum bytes.
//!
//! Mapping:
//! - Go's decoders embed a `row` they reuse between calls; the port builds a [`Row`] view per
//!   call (no state is carried between calls in Go either: `fromBytes` resets every field).
//! - `*time.Location` may be nil in Go: the decoders keep an `Option<Arc<Location>>`. Go's
//!   `ConvertTimeZone(time.UTC, nil)` panics ("time: missing Location in call to Time.In");
//!   the datum-map decoder panics the same way. The chunk decoder passes its location to
//!   `codec.NewDecoder` for handle columns, where a nil location would only matter for a
//!   non-zero TIMESTAMP handle column; tidb-rs callers always pass the session location, the
//!   port uses UTC there (L2, spec §4).
//! - `defDatum` / `defBytes` callbacks are boxed closures.
//! - Go's `map[int64]int` lookups return 0 for a missing key: kept (`output_offset`).

// Flags keep their Go names (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use std::collections::HashMap;
use std::sync::Arc;

use tidb_base::Error;
use tidb_base::errors;
use tidb_base::gofmt::Arg;
use tidb_base::gostd::time::{Location, utc};
use tidb_base::mysql;
use tidb_chunk::Chunk;
use tidb_codec::{decode_decimal, decode_float, encode_compact_bytes, encode_int, encode_uint};
use tidb_codec::{encode_uvarint, encode_varint, new_decoder};
use tidb_kv::key::Handle;
use tidb_model::{ExtraCommitTSID, ExtraHandleID, ExtraRowChecksumID};
use tidb_parser::types::FieldType;
use tidb_types::MODE_HALF_UP;
use tidb_types::MyDecimal;
use tidb_types::binary_literal::new_binary_literal_from_uint;
use tidb_types::datum::Datum;
use tidb_types::enum_type::parse_enum_value;
use tidb_types::etc::need_restored_data;
use tidb_types::json::BinaryJSON;
use tidb_types::set_type::parse_set_value;
use tidb_types::time::{Duration, Time};
use tidb_types::vector::zero_copy_deserialize_vector_float32;

use super::common::{
    BytesFlag, CompactBytesFlag, DecimalFlag, FloatFlag, IntFlag, JSONFlag, NilFlag, UintFlag,
    VarintFlag, VaruintFlag, VectorFloat32Flag, decode_int, decode_uint,
};
use super::row::Row;

/// `t.ConvertTimeZone(time.UTC, loc)` with Go's nil-location panic.
// glue: Go *time.Location that may be nil
fn convert_from_utc(t: &mut Time, loc: Option<&Arc<Location>>) -> Result<(), Error> {
    match loc {
        Some(loc) => t.convert_time_zone(&utc(), loc),
        None => panic!("time: missing Location in call to Time.In"),
    }
}

/// decoder contains base util for decode row.
#[derive(Clone, Debug, Default)]
pub struct Decoder {
    pub columns: Vec<ColInfo>,
    pub handle_col_ids: Vec<i64>,
    pub loc: Option<Arc<Location>>,
}

/// NewDecoder creates a decoder.
// go: NewDecoder L1
pub fn new_decoder_base(
    columns: Vec<ColInfo>,
    handle_col_ids: Vec<i64>,
    loc: Option<Arc<Location>>,
) -> Decoder {
    Decoder {
        columns,
        handle_col_ids,
        loc,
    }
}

/// ColInfo is used as column meta info for row decoder.
#[derive(Clone, Debug, Default)]
pub struct ColInfo {
    pub id: i64,
    pub is_pk_handle: bool,
    pub virtual_gen_col: bool,
    /// Go `*types.FieldType`, shared.
    pub ft: Arc<FieldType>,
}

/// DatumMapDecoder decodes the row to datum map.
#[derive(Clone, Debug, Default)]
pub struct DatumMapDecoder {
    decoder: Decoder,
}

/// NewDatumMapDecoder creates a DatumMapDecoder.
// go: NewDatumMapDecoder L1
pub fn new_datum_map_decoder(columns: Vec<ColInfo>, loc: Option<Arc<Location>>) -> DatumMapDecoder {
    DatumMapDecoder {
        decoder: Decoder {
            columns,
            handle_col_ids: Vec::new(),
            loc,
        },
    }
}

impl DatumMapDecoder {
    /// DecodeToDatumMap decodes byte slices to datum map.
    // go: DatumMapDecoder.DecodeToDatumMap L1
    pub fn decode_to_datum_map(
        &self,
        row_data: &[u8],
        row: Option<HashMap<i64, Datum>>,
    ) -> Result<HashMap<i64, Datum>, Error> {
        let mut row = row.unwrap_or_else(|| HashMap::with_capacity(self.decoder.columns.len()));
        let r = Row::from_bytes(row_data)?;
        for col in &self.decoder.columns {
            let (idx, is_nil, not_found) = r.find_col_id(col.id);
            if !not_found && !is_nil {
                let col_data = r.get_data(idx);
                let d = self.decode_col_datum(col, col_data)?;
                row.insert(col.id, d);
                continue;
            }

            if is_nil {
                let mut d = Datum::default();
                d.set_null();
                row.insert(col.id, d);
                continue;
            }
        }
        Ok(row)
    }

    // go: DatumMapDecoder.decodeColDatum L1
    fn decode_col_datum(&self, col: &ColInfo, col_data: &[u8]) -> Result<Datum, Error> {
        let mut d = Datum::default();
        match col.ft.get_type() {
            mysql::TypeLonglong
            | mysql::TypeLong
            | mysql::TypeInt24
            | mysql::TypeShort
            | mysql::TypeTiny => {
                if mysql::has_unsigned_flag(col.ft.get_flag()) {
                    d.set_uint64(decode_uint(col_data));
                } else {
                    d.set_int64(decode_int(col_data));
                }
            }
            mysql::TypeYear => d.set_int64(decode_int(col_data)),
            mysql::TypeFloat => {
                let (_, f_val) = decode_float(col_data)?;
                d.set_float32(f_val as f32);
            }
            mysql::TypeDouble => {
                let (_, f_val) = decode_float(col_data)?;
                d.set_float64(f_val);
            }
            mysql::TypeVarString
            | mysql::TypeVarchar
            | mysql::TypeString
            | mysql::TypeBlob
            | mysql::TypeTinyBlob
            | mysql::TypeMediumBlob
            | mysql::TypeLongBlob => {
                d.set_string(col_data.to_vec(), col.ft.get_collate().to_owned());
            }
            mysql::TypeNewDecimal => {
                let (_, dec, precision, frac, err) = decode_decimal(col_data);
                err?;
                d.set_mysql_decimal(dec.unwrap_or_default());
                d.set_length(precision);
                d.set_frac(frac);
            }
            mysql::TypeDate | mysql::TypeDatetime | mysql::TypeTimestamp => {
                let mut t = Time::default();
                t.set_type(col.ft.get_type());
                t.set_fsp(col.ft.get_decimal());
                t.from_packed_uint(decode_uint(col_data))?;
                if col.ft.get_type() == mysql::TypeTimestamp && !t.is_zero() {
                    convert_from_utc(&mut t, self.decoder.loc.as_ref())?;
                }
                d.set_mysql_time(t);
            }
            mysql::TypeDuration => {
                let dur = Duration {
                    duration: decode_int(col_data),
                    fsp: col.ft.get_decimal(),
                };
                d.set_mysql_duration(dur);
            }
            mysql::TypeEnum => {
                // ignore error deliberately, to read empty enum value.
                let enum_ = parse_enum_value(col.ft.get_elems().unwrap_or(&[]), decode_uint(col_data))
                    .unwrap_or_default();
                d.set_mysql_enum(enum_, col.ft.get_collate().to_owned());
            }
            mysql::TypeSet => {
                let set = parse_set_value(col.ft.get_elems().unwrap_or(&[]), decode_uint(col_data))?;
                d.set_mysql_set(set, col.ft.get_collate().to_owned());
            }
            mysql::TypeBit => {
                let byte_size = (col.ft.get_flen() + 7) >> 3;
                d.set_mysql_bit(new_binary_literal_from_uint(decode_uint(col_data), byte_size));
            }
            mysql::TypeJSON => {
                let j = BinaryJSON::borrowed(col_data[0], &col_data[1..]);
                d.set_mysql_json(j);
            }
            mysql::TypeTiDBVectorFloat32 => {
                let (v, _) = zero_copy_deserialize_vector_float32(col_data)?;
                d.set_vector_float32(v);
            }
            tp => {
                return Err(errors::errorf("unknown type %d", &[Arg::from(tp)]));
            }
        }
        Ok(d)
    }
}

/// The `defDatum` callback of a [`ChunkDecoder`].
pub type DefDatumFn = Box<dyn Fn(usize, &mut Chunk) -> Result<(), Error> + Send + Sync>;

/// ChunkDecoder decodes the row to chunk.Chunk.
pub struct ChunkDecoder {
    decoder: Decoder,
    def_datum: Option<DefDatumFn>,
}

/// NewChunkDecoder creates a NewChunkDecoder.
// go: NewChunkDecoder L1
pub fn new_chunk_decoder(
    columns: Vec<ColInfo>,
    handle_col_ids: Vec<i64>,
    def_datum: Option<DefDatumFn>,
    loc: Option<Arc<Location>>,
) -> ChunkDecoder {
    ChunkDecoder {
        decoder: Decoder {
            columns,
            handle_col_ids,
            loc,
        },
        def_datum,
    }
}

impl ChunkDecoder {
    /// DecodeToChunk decodes a row to chunk.
    // go: ChunkDecoder.DecodeToChunk L1
    pub fn decode_to_chunk(
        &self,
        row_data: &[u8],
        commit_ts: u64,
        handle: Option<&Handle>,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let r = Row::from_bytes(row_data)?;

        for (col_idx, col) in self.decoder.columns.iter().enumerate() {
            if col.id == ExtraCommitTSID as i64 {
                // intest.Assert(commitTS > 0, ...) is a test-build assertion only.
                if commit_ts > 0 {
                    chk.append_uint64(col_idx, commit_ts);
                } else {
                    chk.append_null(col_idx);
                }
                continue;
            }
            // fill the virtual column value after row calculation
            if col.virtual_gen_col {
                chk.append_null(col_idx);
                continue;
            }
            if col.id == ExtraRowChecksumID as i64 {
                chk.append_null(col_idx);
                continue;
            }

            let (idx, is_nil, not_found) = r.find_col_id(col.id);
            if !not_found && !is_nil {
                let col_data = r.get_data(idx);
                self.decode_col_to_chunk(col_idx, col, col_data, chk)?;
                continue;
            }

            // Only try to decode handle when there is no corresponding column in the value.
            // This is because the information in handle may be incomplete in some cases.
            // For example, prefixed clustered index like 'primary key(col1(1))' only store the leftmost 1 char in the handle.
            if self.try_append_handle_column(col_idx, col, handle, chk) {
                continue;
            }

            if is_nil {
                chk.append_null(col_idx);
                continue;
            }

            let Some(def_datum) = &self.def_datum else {
                chk.append_null(col_idx);
                continue;
            };

            def_datum(col_idx, chk)?;
        }
        Ok(())
    }

    // go: ChunkDecoder.tryAppendHandleColumn L1
    fn try_append_handle_column(
        &self,
        col_idx: usize,
        col: &ColInfo,
        handle: Option<&Handle>,
        chk: &mut Chunk,
    ) -> bool {
        let Some(handle) = handle else {
            return false;
        };
        if handle.is_int() && col.id == self.decoder.handle_col_ids[0] {
            chk.append_int64(col_idx, handle.int_value());
            return true;
        }
        for (i, &id) in self.decoder.handle_col_ids.iter().enumerate() {
            if col.id == id {
                if need_restored_data(&col.ft) {
                    return false;
                }
                let loc = self.decoder.loc.clone().unwrap_or_else(utc);
                let mut coder = new_decoder(chk, loc);
                let encoded_col = handle.encoded_col(i as isize);
                return coder.decode_one(&encoded_col, col_idx, &col.ft).is_ok();
            }
        }
        false
    }

    // go: ChunkDecoder.decodeColToChunk L1
    fn decode_col_to_chunk(
        &self,
        col_idx: usize,
        col: &ColInfo,
        col_data: &[u8],
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        match col.ft.get_type() {
            mysql::TypeLonglong
            | mysql::TypeLong
            | mysql::TypeInt24
            | mysql::TypeShort
            | mysql::TypeTiny => {
                if mysql::has_unsigned_flag(col.ft.get_flag()) {
                    chk.append_uint64(col_idx, decode_uint(col_data));
                } else {
                    chk.append_int64(col_idx, decode_int(col_data));
                }
            }
            mysql::TypeYear => chk.append_int64(col_idx, decode_int(col_data)),
            mysql::TypeFloat => {
                let (_, f_val) = decode_float(col_data)?;
                chk.append_float32(col_idx, f_val as f32);
            }
            mysql::TypeDouble => {
                let (_, f_val) = decode_float(col_data)?;
                chk.append_float64(col_idx, f_val);
            }
            mysql::TypeVarString
            | mysql::TypeVarchar
            | mysql::TypeString
            | mysql::TypeBlob
            | mysql::TypeTinyBlob
            | mysql::TypeMediumBlob
            | mysql::TypeLongBlob => {
                chk.append_bytes(col_idx, col_data);
            }
            mysql::TypeNewDecimal => {
                let (_, dec, _, frac, err) = decode_decimal(col_data);
                err?;
                let mut dec = dec.unwrap_or_default();
                if col.ft.get_decimal() != tidb_parser::types::UnspecifiedLength
                    && frac > col.ft.get_decimal()
                {
                    let mut to = MyDecimal::default();
                    dec.round(&mut to, col.ft.get_decimal(), MODE_HALF_UP)?;
                    dec = to;
                }
                chk.append_my_decimal(col_idx, &dec);
            }
            mysql::TypeDate | mysql::TypeDatetime | mysql::TypeTimestamp => {
                let mut t = Time::default();
                t.set_type(col.ft.get_type());
                t.set_fsp(col.ft.get_decimal());
                t.from_packed_uint(decode_uint(col_data))?;
                if col.ft.get_type() == mysql::TypeTimestamp
                    && let Some(loc) = &self.decoder.loc
                    && !t.is_zero()
                {
                    t.convert_time_zone(&utc(), loc)?;
                }
                chk.append_time(col_idx, t);
            }
            mysql::TypeDuration => {
                let dur = Duration {
                    duration: decode_int(col_data),
                    fsp: col.ft.get_decimal(),
                };
                chk.append_duration(col_idx, dur);
            }
            mysql::TypeEnum => {
                // ignore error deliberately, to read empty enum value.
                let enum_ = parse_enum_value(col.ft.get_elems().unwrap_or(&[]), decode_uint(col_data))
                    .unwrap_or_default();
                chk.append_enum(col_idx, &enum_);
            }
            mysql::TypeSet => {
                let set = parse_set_value(col.ft.get_elems().unwrap_or(&[]), decode_uint(col_data))?;
                chk.append_set(col_idx, &set);
            }
            mysql::TypeBit => {
                let byte_size = (col.ft.get_flen() + 7) >> 3;
                chk.append_bytes(
                    col_idx,
                    &new_binary_literal_from_uint(decode_uint(col_data), byte_size).0,
                );
            }
            mysql::TypeJSON => {
                let j = BinaryJSON::borrowed(col_data[0], &col_data[1..]);
                chk.append_json(col_idx, &j);
            }
            mysql::TypeTiDBVectorFloat32 => {
                let (v, _) = zero_copy_deserialize_vector_float32(col_data)?;
                chk.append_vector_float32(col_idx, &v);
            }
            tp => {
                return Err(errors::errorf("unknown type %d", &[Arg::from(tp)]));
            }
        }
        Ok(())
    }
}

/// The `defBytes` callback of a [`BytesDecoder`].
pub type DefBytesFn = Box<dyn Fn(usize) -> Result<Vec<u8>, Error> + Send + Sync>;

/// BytesDecoder decodes the row to old datums bytes.
pub struct BytesDecoder {
    decoder: Decoder,
    def_bytes: Option<DefBytesFn>,
}

/// NewByteDecoder creates a BytesDecoder.
/// defBytes: provided default value bytes in old datum format(flag+colData).
// go: NewByteDecoder L1
pub fn new_byte_decoder(
    columns: Vec<ColInfo>,
    handle_col_ids: Vec<i64>,
    def_bytes: Option<DefBytesFn>,
    loc: Option<Arc<Location>>,
) -> BytesDecoder {
    BytesDecoder {
        decoder: Decoder {
            columns,
            handle_col_ids,
            loc,
        },
        def_bytes,
    }
}

impl BytesDecoder {
    /// The column infos (Go `decoder.columns`).
    // glue: Go field access decoder.columns
    pub fn columns(&self) -> &[ColInfo] {
        &self.decoder.columns
    }

    /// The result entries are Go's `[][]byte`: an empty vector is Go's nil (the decoder never
    /// produces an empty non-nil entry).
    // go: BytesDecoder.decodeToBytesInternal L1
    fn decode_to_bytes_internal(
        &self,
        output_offset: &HashMap<i64, usize>,
        handle: Option<&Handle>,
        value: &[u8],
        cache_bytes: &[u8],
    ) -> Result<Vec<Vec<u8>>, Error> {
        let r = Row::from_bytes(value)?;
        let mut values: Vec<Vec<u8>> = vec![Vec::new(); output_offset.len()];
        for (i, col) in self.decoder.columns.iter().enumerate() {
            let tp = field_type2_flag(
                col.ft.array_type().get_type(),
                col.ft.get_flag() & mysql::UnsignedFlag == 0,
            );
            let col_id = col.id;
            let offset = output_offset.get(&col_id).copied().unwrap_or(0);
            let (idx, is_nil, not_found) = r.find_col_id(col_id);
            if !not_found && !is_nil {
                let val = r.get_data(idx);
                values[offset] = Self::encode_old_datum(tp, val);
                continue;
            }

            // Only try to decode handle when there is no corresponding column in the value.
            // This is because the information in handle may be incomplete in some cases.
            // For example, prefixed clustered index like 'primary key(col1(1))' only store the leftmost 1 char in the handle.
            if self.try_decode_handle(&mut values, offset, col, handle, cache_bytes) {
                continue;
            }

            if is_nil {
                values[offset] = vec![NilFlag];
                continue;
            }

            if let Some(def_bytes) = &self.def_bytes {
                let def_val = def_bytes(i)?;
                if !def_val.is_empty() {
                    values[offset] = def_val;
                    continue;
                }
            }

            values[offset] = vec![NilFlag];
        }
        Ok(values)
    }

    // go: BytesDecoder.tryDecodeHandle L1
    fn try_decode_handle(
        &self,
        values: &mut [Vec<u8>],
        offset: usize,
        col: &ColInfo,
        handle: Option<&Handle>,
        cache_bytes: &[u8],
    ) -> bool {
        let Some(handle) = handle else {
            return false;
        };
        if need_restored_data(&col.ft) {
            return false;
        }
        if col.is_pk_handle || col.id == ExtraHandleID as i64 {
            let mut handle_data = cache_bytes.to_vec();
            if mysql::has_unsigned_flag(col.ft.get_flag()) {
                handle_data.push(UintFlag);
                encode_uint(&mut handle_data, handle.int_value() as u64);
            } else {
                handle_data.push(IntFlag);
                encode_int(&mut handle_data, handle.int_value());
            }
            values[offset] = handle_data;
            return true;
        }
        let mut handle_data: Vec<u8> = Vec::new();
        for (i, &hid) in self.decoder.handle_col_ids.iter().enumerate() {
            if col.id == hid {
                handle_data.extend_from_slice(&handle.encoded_col(i as isize));
            }
        }
        if !handle_data.is_empty() {
            values[offset] = handle_data;
            return true;
        }
        false
    }

    /// DecodeToBytesNoHandle decodes raw byte slice to row data without handle.
    // go: BytesDecoder.DecodeToBytesNoHandle L1
    pub fn decode_to_bytes_no_handle(
        &self,
        output_offset: &HashMap<i64, usize>,
        value: &[u8],
    ) -> Result<Vec<Vec<u8>>, Error> {
        self.decode_to_bytes_internal(output_offset, None, value, &[])
    }

    /// DecodeToBytes decodes raw byte slice to row data.
    // go: BytesDecoder.DecodeToBytes L1
    pub fn decode_to_bytes(
        &self,
        output_offset: &HashMap<i64, usize>,
        handle: Option<&Handle>,
        value: &[u8],
        cache_bytes: &[u8],
    ) -> Result<Vec<Vec<u8>>, Error> {
        self.decode_to_bytes_internal(output_offset, handle, value, cache_bytes)
    }

    // go: BytesDecoder.encodeOldDatum L1
    fn encode_old_datum(tp: u8, val: &[u8]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(1 + tidb_codec::gobinary::MaxVarintLen64 + val.len());
        match tp {
            BytesFlag => {
                buf.push(CompactBytesFlag);
                encode_compact_bytes(&mut buf, val);
            }
            IntFlag => {
                buf.push(VarintFlag);
                encode_varint(&mut buf, decode_int(val));
            }
            UintFlag => {
                buf.push(VaruintFlag);
                encode_uvarint(&mut buf, decode_uint(val));
            }
            _ => {
                buf.push(tp);
                buf.extend_from_slice(val);
            }
        }
        buf
    }
}

/// fieldType2Flag transforms field type into kv type flag.
// go: fieldType2Flag L1 [M-PANIC]
fn field_type2_flag(tp: u8, signed: bool) -> u8 {
    match tp {
        mysql::TypeTiny
        | mysql::TypeShort
        | mysql::TypeInt24
        | mysql::TypeLong
        | mysql::TypeLonglong => {
            if signed {
                IntFlag
            } else {
                UintFlag
            }
        }
        mysql::TypeFloat | mysql::TypeDouble => FloatFlag,
        mysql::TypeBlob
        | mysql::TypeTinyBlob
        | mysql::TypeMediumBlob
        | mysql::TypeLongBlob
        | mysql::TypeString
        | mysql::TypeVarchar
        | mysql::TypeVarString => BytesFlag,
        mysql::TypeDatetime | mysql::TypeDate | mysql::TypeTimestamp => UintFlag,
        mysql::TypeDuration => IntFlag,
        mysql::TypeNewDecimal => DecimalFlag,
        mysql::TypeYear => IntFlag,
        mysql::TypeEnum | mysql::TypeBit | mysql::TypeSet => UintFlag,
        mysql::TypeJSON => JSONFlag,
        mysql::TypeTiDBVectorFloat32 => VectorFloat32Flag,
        mysql::TypeNull => NilFlag,
        _ => panic!("unknown field type {tp}"),
    }
}
