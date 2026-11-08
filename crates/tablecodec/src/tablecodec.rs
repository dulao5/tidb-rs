//! go-file: pkg/tablecodec/tablecodec.go
//!
//! Table, record and index key layouts; row v1 decoding (`DecodeRowWithMap`); `Unflatten`;
//! index key cutting and index value decoding (old encoding, version 0, clustered version 1).
//!
//! Mapping:
//! - `kv.Key` results are [`Key`]; key arguments are `&[u8]`. Go's error messages format a
//!   `kv.Key` with `%q`, which calls its `String` method (hex) and quotes it: the port passes
//!   the hex string as a `fmt.Stringer` argument.
//! - `*time.Location` that Go callers may pass as nil is `Option<&Arc<Location>>`; where Go
//!   would call `Time.In(nil)` the port panics with Go's message.
//! - Index decoding returns `[][]byte` whose entries alias the key in Go: here
//!   `Cow<[u8]>`, borrowed from the key where Go aliases it, owned where Go computes bytes.
//!   An empty entry is Go's nil (the decoders never produce an empty non-nil entry).
//! - `[]byte` arguments Go distinguishes as nil vs empty (`DecodeRowWithMap*`'s and
//!   `CutRowNew`'s row): an empty slice is Go's non-nil empty slice (every Go caller passes
//!   a KV value, never nil).
//! - `map[int64]*types.FieldType` is `HashMap<i64, Arc<FieldType>>` (Go shares the field
//!   types by pointer); `map[int64]types.Datum` is `HashMap<i64, Datum>`.
//! - Not ported (tracker block X, spec §2): the row and index value encoders (`EncodeRow`,
//!   `EncodeOldRow`, `GenIndexValue*`, `EncodeHandleInUniqueIndexValue`), the temp-index value
//!   codec, `IsUntouchedIndexKValue`, `IndexKVIsUnique`, `TempIndexKey2IndexKey` (write path,
//!   done by Go TiDB), helpers used only by DDL, unistore or TiCDC, and functions with no
//!   caller in v8.5.8.

// Go identifiers for the key layout constants (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::LazyLock;

use tidb_base::Error;
use tidb_base::dbterror::ClassXEval;
use tidb_base::errno;
use tidb_base::errors;
use tidb_base::gofmt::Arg;
use tidb_base::gostd::time::{Location, utc};
use tidb_base::gostd::{decode_rune, rune_count};
use tidb_base::mysql;
use tidb_base::terror;
use tidb_codec::{
    IntHandleFlag, NilFlag, cut_column_id, cut_one, decode_bytes, decode_cmp_uint_to_int,
    decode_int, decode_one, decode_uint, encode_bytes, encode_int, encode_int_to_cmp_uint,
    encode_key, encode_value,
};
use tidb_kv::key::new_partition_handle;
use tidb_kv::key::{CommonHandle, Handle, IntHandle, Key, KeyRange, new_common_handle};
use tidb_kv::kv::KeyRanges;
use tidb_kv::kv::UnCommitIndexKVFlag;
use tidb_model::{ColumnInfo, GlobalIndexVersionV1, IndexColumn, IndexInfo, TableInfo};
use tidb_parser::gocharset::{CharsetASCII, CharsetBin};
use tidb_parser::types::{FieldType, UnspecifiedLength};
use tidb_types::binary_literal::new_binary_literal_from_uint;
use tidb_types::collate::is_bin_collation;
use tidb_types::context::StrictContext;
use tidb_types::core_time::ZeroCoreTime;
use tidb_types::datum::{
    Datum, KindBinaryLiteral, KindBytes, KindInt64, KindMysqlBit, KindMysqlDuration, KindMysqlEnum,
    KindMysqlSet, KindMysqlTime, KindString, new_int_datum, new_uint_datum,
};
use tidb_types::enum_type::parse_enum_value;
use tidb_types::etc::need_restored_data;
use tidb_types::field_type::new_field_type;
use tidb_types::set_type::parse_set_value;
use tidb_types::time::{Duration, new_time};

use crate::rowcodec::{self, ColInfo};

static errInvalidKey: LazyLock<Error> = LazyLock::new(|| ClassXEval.new_std(errno::ErrInvalidKey));
static errInvalidRecordKey: LazyLock<Error> =
    LazyLock::new(|| ClassXEval.new_std(errno::ErrInvalidRecordKey));
static errInvalidIndexKey: LazyLock<Error> =
    LazyLock::new(|| ClassXEval.new_std(errno::ErrInvalidIndexKey));

const tablePrefix: &[u8] = b"t";
const recordPrefixSep: &[u8] = b"_r";
const indexPrefixSep: &[u8] = b"_i";
const metaPrefix: &[u8] = b"m";

const idLen: usize = 8;
const prefixLen: usize = 1 + idLen /*tableID*/ + 2;
/// RecordRowKeyLen is public for calculating average row size.
pub const RecordRowKeyLen: usize = prefixLen + idLen /*handle*/;
const tablePrefixLength: usize = 1;
const recordPrefixSepLength: usize = 2;
const metaPrefixLength: usize = 1;
/// MaxOldEncodeValueLen is the maximum len of the old encoding of index value.
pub const MaxOldEncodeValueLen: usize = 9;

/// CommonHandleFlag is the flag used to decode the common handle in an unique index value.
pub const CommonHandleFlag: u8 = 127;
/// PartitionIDFlag is the flag used to decode the partition ID.
/// Used in both global index values and global index keys (for V1+ non-unique indexes).
/// In keys: PartitionIDFlag + partition_id (8 bytes) + inner_handle_encoded (IntHandle)
/// In values: PartitionIDFlag + partition_id (8 bytes)
pub const PartitionIDFlag: u8 = 126;
/// IndexVersionFlag is the flag used to decode the index's version info.
pub const IndexVersionFlag: u8 = 125;
/// RestoreDataFlag is the flag that RestoreData begin with.
/// See rowcodec.Encoder.Encode and rowcodec.row.toBytes
pub const RestoreDataFlag: u8 = rowcodec::CodecVer;

/// TableSplitKeyLen is the length of key 't{table_id}' which is used for table split.
pub const TableSplitKeyLen: usize = 1 + idLen;

/// `structure.HashData` (pkg/structure, block D06.2): the type flag of hash data keys
/// (`structure.TypeFlag` is a byte).
const structureHashData: u8 = b'h';

/// A `kv.Key` argument of a format: Go's `%q` calls `Key.String` (hex).
// glue: kv.Key as a fmt.Stringer argument
fn key_arg(k: &[u8]) -> Arg {
    Arg::Stringer(Key(k.to_vec()).string().into_bytes(), "kv.Key")
}

/// help kv package to refer the tablecodec package to resolve the kv.Key functions.
///
/// Go sets `kv.DecodeTableIDFunc` in its package initialiser; tidb-rs has no package
/// initialisers, the program calls [`init`] once at start-up (L2, spec §4; wired with the
/// other initialisers in D00.1 / the server).
// go: init L2 (Rust has no package initialisers; called at start-up)
pub fn init() -> fn(&[u8]) -> i64 {
    // kv.DecodeTableIDFunc has no Rust counterpart yet (D07.0 ports Key without it): the
    // function is returned for the caller that installs it.
    |key: &[u8]| -> i64 {
        //preCheck, avoid the noise error log.
        if has_table_prefix(key) && key.len() >= TableSplitKeyLen {
            return decode_table_id(key);
        }
        0
    }
}

/// TablePrefix returns table's prefix 't'.
// go: TablePrefix L1
pub fn table_prefix() -> &'static [u8] {
    tablePrefix
}

/// EncodeRowKey encodes the table id and record handle into a kv.Key
// go: EncodeRowKey L1
pub fn encode_row_key(table_id: i64, encoded_handle: &[u8]) -> Key {
    let mut buf = Vec::with_capacity(prefixLen + encoded_handle.len());
    append_table_record_prefix(&mut buf, table_id);
    buf.extend_from_slice(encoded_handle);
    Key(buf)
}

/// EncodeRowKeyWithHandle encodes the table id, row handle into a kv.Key
// go: EncodeRowKeyWithHandle L1
pub fn encode_row_key_with_handle(table_id: i64, handle: &Handle) -> Key {
    encode_row_key(table_id, &handle.encoded())
}

/// EncodeRecordKey encodes the recordPrefix, row handle into a kv.Key.
// go: EncodeRecordKey L1
pub fn encode_record_key(record_prefix: &[u8], h: &Handle) -> Key {
    let mut buf = Vec::with_capacity(record_prefix.len() + h.len() as usize);
    let prefix;
    let record_prefix = if let Handle::Partition(ph) = h {
        prefix = gen_table_record_prefix(ph.partition_id);
        &prefix.0[..]
    } else {
        record_prefix
    };
    buf.extend_from_slice(record_prefix);
    buf.extend_from_slice(&h.encoded());
    Key(buf)
}

// go: hasTablePrefix L1
fn has_table_prefix(key: &[u8]) -> bool {
    key[0] == tablePrefix[0]
}

// go: hasRecordPrefixSep L1
fn has_record_prefix_sep(key: &[u8]) -> bool {
    key[0] == recordPrefixSep[0] && key[1] == recordPrefixSep[1]
}

/// DecodeRecordKey decodes the key and gets the tableID, handle.
// go: DecodeRecordKey L1
pub fn decode_record_key(key: &[u8]) -> Result<(i64, Handle), Error> {
    if key.len() <= prefixLen {
        return Err(
            errInvalidRecordKey.gen_with_stack("invalid record key - %q", vec![key_arg(key)])
        );
    }

    let k = key;
    if !has_table_prefix(key) {
        return Err(errInvalidRecordKey.gen_with_stack("invalid record key - %q", vec![key_arg(k)]));
    }

    let key = &key[tablePrefixLength..];
    let (key, table_id) = decode_int(key)?;

    if !has_record_prefix_sep(key) {
        return Err(errInvalidRecordKey.gen_with_stack("invalid record key - %q", vec![key_arg(k)]));
    }

    let key = &key[recordPrefixSepLength..];
    if key.len() == 8 {
        let (_, int_handle) = decode_int(key)?;
        return Ok((table_id, Handle::Int(IntHandle(int_handle))));
    }
    match new_common_handle(key.to_vec()) {
        Ok(h) => Ok((table_id, Handle::Common(Arc::new(h)))),
        Err(err) => Err(errInvalidRecordKey.gen_with_stack(
            "invalid record key - %q %v",
            vec![key_arg(k), Arg::from(&err)],
        )),
    }
}

/// DecodeIndexKey decodes the key and gets the tableID, indexID, indexValues.
///
/// The index values are Go strings (bytes); an empty vector is Go's nil.
// go: DecodeIndexKey L1
pub fn decode_index_key(key: &[u8]) -> Result<(i64, i64, Vec<Vec<u8>>), Error> {
    let k = key;

    let (table_id, index_id, is_record, err) = decode_key_head(key);
    err?;
    if is_record {
        return Err(errInvalidIndexKey.gen_with_stack("invalid index key - %q", vec![key_arg(k)]));
    }
    let index_key = &key[prefixLen + idLen..];
    match decode_values_bytes_to_strings(index_key) {
        Ok(index_values) => Ok((table_id, index_id, index_values)),
        Err(err) => Err(errInvalidIndexKey.gen_with_stack(
            "invalid index key - %q %v",
            vec![key_arg(k), Arg::from(&err)],
        )),
    }
}

/// DecodeValuesBytesToStrings decode the raw bytes to strings for each columns.
/// FIXME: Without the schema information, we can only decode the raw kind of
/// the column. For instance, MysqlTime is internally saved as uint64.
///
/// Go returns `nil, e` when `ToString` fails, where `e` is the (nil) decoding error: the
/// result is a nil slice and no error (kept; spec §5).
// go: DecodeValuesBytesToStrings L1
pub fn decode_values_bytes_to_strings(mut b: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
    let mut datum_values: Vec<Vec<u8>> = Vec::new();
    while !b.is_empty() {
        let (remain, d, e) = decode_one(b);
        e?;
        let str = match d.to_string() {
            Ok(s) => s,
            Err(_) => return Ok(Vec::new()),
        };
        datum_values.push(str);
        b = remain;
    }
    Ok(datum_values)
}

/// DecodeMetaKey decodes the key and get the meta key and meta field.
// go: DecodeMetaKey L1
pub fn decode_meta_key(ek: &[u8]) -> Result<(Vec<u8>, Vec<u8>), Error> {
    if !ek.starts_with(metaPrefix) {
        return Err(errors::new("invalid encoded hash data key prefix"));
    }
    let ek = &ek[metaPrefixLength..];
    let (ek, key) = decode_bytes(ek, None)?;
    let (ek, tp) = decode_uint(ek)?;
    // Go compares structure.TypeFlag(tp): the uint64 truncated to a byte.
    if tp as u8 != structureHashData {
        return Err(errors::errorf(
            "invalid encoded hash data key flag %c",
            &[Arg::from(tp as u8)],
        ));
    }
    let (_, field) = decode_bytes(ek, None)?;
    Ok((key, field))
}

/// DecodeKeyHead decodes the key's head and gets the tableID, indexID. isRecordKey is true when is a record key.
///
/// Returns `(tableID, indexID, isRecordKey, err)` with Go's named results (M-ERRVAL): on
/// some errors `tableID` is already set.
// go: DecodeKeyHead L1 [M-ERRVAL]
pub fn decode_key_head(key: &[u8]) -> (i64, i64, bool, Result<(), Error>) {
    let mut table_id = 0i64;
    let mut index_id = 0i64;
    let k = key;
    if !key.starts_with(tablePrefix) {
        let err = errInvalidKey.gen_with_stack("invalid key - %q", vec![key_arg(k)]);
        return (table_id, index_id, false, Err(err));
    }

    let key = &key[tablePrefix.len()..];
    let key = match decode_int(key) {
        Ok((key, v)) => {
            table_id = v;
            key
        }
        Err(err) => return (table_id, index_id, false, Err(err)),
    };

    if key.starts_with(recordPrefixSep) {
        return (table_id, index_id, true, Ok(()));
    }
    if !key.starts_with(indexPrefixSep) {
        let err = errInvalidKey.gen_with_stack("invalid key - %q", vec![key_arg(k)]);
        return (table_id, index_id, false, Err(err));
    }

    let key = &key[indexPrefixSep.len()..];

    match decode_int(key) {
        Ok((_, v)) => index_id = v,
        Err(err) => return (table_id, index_id, false, Err(err)),
    }
    (table_id, index_id, false, Ok(()))
}

/// `tikv.DecodeKey(key, kvrpcpb.APIVersion_V2)`: a keyspace-prefixed key (`x`/`r` + 3
/// bytes) without its prefix.
// go-dep: github.com/tikv/client-go/v2/internal/apicodec.DecodeKey L1 (APIVersion_V2 only)
fn decode_key_api_v2(encoded: &[u8]) -> Result<&[u8], Error> {
    const keyspacePrefixLen: usize = 4;
    const rawModePrefix: u8 = b'r';
    const txnModePrefix: u8 = b'x';
    if encoded.len() < keyspacePrefixLen
        || (encoded[0] != rawModePrefix && encoded[0] != txnModePrefix)
    {
        return Err(errors::errorf(
            "invalid API V2 key %s",
            &[Arg::Bytes(encoded.to_vec())],
        ));
    }
    Ok(&encoded[keyspacePrefixLen..])
}

/// DecodeTableID decodes the table ID of the key, if the key is not table key, returns 0.
// go: DecodeTableID L1
pub fn decode_table_id(key: &[u8]) -> i64 {
    let mut key = key;
    if !key.starts_with(tablePrefix) {
        // If the key is in API V2, then ignore the prefix
        match decode_key_api_v2(key) {
            Ok(k) => key = k,
            Err(err) => {
                terror::log(Some(&err));
                return 0;
            }
        }
        if !key.starts_with(tablePrefix) {
            return 0;
        }
    }
    let key = &key[tablePrefix.len()..];
    match decode_int(key) {
        Ok((_, table_id)) => table_id,
        Err(err) => {
            // TODO: return error.
            terror::log(Some(&err));
            0
        }
    }
}

/// DecodeRowKey decodes the key and gets the handle.
///
/// Go returns `kv.IntHandle(0)` with its error; the port returns the error alone.
// go: DecodeRowKey L1
pub fn decode_row_key(key: &[u8]) -> Result<Handle, Error> {
    if key.len() < RecordRowKeyLen
        || !has_table_prefix(key)
        || !has_record_prefix_sep(&key[prefixLen - 2..])
    {
        return Err(errInvalidKey.gen_with_stack("invalid key - %q", vec![key_arg(key)]));
    }
    if key.len() == RecordRowKeyLen {
        let u = u64::from_be_bytes(key[prefixLen..prefixLen + 8].try_into().unwrap());
        return Ok(Handle::Int(IntHandle(decode_cmp_uint_to_int(u))));
    }
    Ok(Handle::Common(Arc::new(new_common_handle(
        key[prefixLen..].to_vec(),
    )?)))
}

/// EncodeValue encodes a go value to bytes.
/// This function may return both a valid encoded bytes and an error (actually `"pingcap/errors".ErrorGroup`). If the caller
/// expects to handle these errors according to `SQL_MODE` or other configuration, please refer to `pkg/errctx`.
///
/// Returns `(bytes, err)` as Go (M-ERRVAL); on a `flatten` error the bytes are empty (Go nil).
// go: EncodeValue L1 [M-ERRVAL]
pub fn encode_value_datum(
    loc: Option<&Arc<Location>>,
    mut b: Vec<u8>,
    raw: &Datum,
) -> (Vec<u8>, Result<(), Error>) {
    let v = match flatten(loc, raw) {
        Ok(v) => v,
        Err(err) => return (Vec::new(), Err(err)),
    };
    // flatten removed the time kinds, the only ones codec.EncodeValue reads loc for.
    let loc = loc.cloned().unwrap_or_else(utc);
    let err = encode_value(&loc, &mut b, std::slice::from_ref(&v));
    (b, err)
}

// go: flatten L1
fn flatten(loc: Option<&Arc<Location>>, data: &Datum) -> Result<Datum, Error> {
    let mut ret = Datum::default();
    match data.kind() {
        KindMysqlTime => {
            // for mysql datetime, timestamp and date type
            let mut t = data.get_mysql_time();
            if t.tp() == mysql::TypeTimestamp
                && let Some(loc) = loc
                && !Arc::ptr_eq(loc, &utc())
            {
                t.convert_time_zone(loc, &utc())?;
            }
            let (v, err) = match t.to_packed_uint() {
                Ok(v) => (v, Ok(())),
                Err(err) => (0, Err(err)),
            };
            ret.set_uint64(v);
            err.map(|_| ret)
        }
        KindMysqlDuration => {
            // for mysql time type
            ret.set_int64(data.get_mysql_duration().duration);
            Ok(ret)
        }
        KindMysqlEnum => {
            ret.set_uint64(data.get_mysql_enum().value);
            Ok(ret)
        }
        KindMysqlSet => {
            ret.set_uint64(data.get_mysql_set().value);
            Ok(ret)
        }
        KindBinaryLiteral | KindMysqlBit => {
            // We don't need to handle errors here since the literal is ensured to be able to store in uint64 in convertToMysqlBit.
            let (val, err) = data.get_binary_literal().to_int(&StrictContext);
            err?;
            ret.set_uint64(val);
            Ok(ret)
        }
        _ => Ok(data.clone()),
    }
}

/// DecodeColumnValue decodes data to a Datum according to the column info.
// go: DecodeColumnValue L1
pub fn decode_column_value(
    data: &[u8],
    ft: &FieldType,
    loc: Option<&Arc<Location>>,
) -> Result<Datum, Error> {
    let (_, d, err) = decode_one(data);
    err?;
    let (col_datum, err) = unflatten(d, ft, loc);
    err?;
    Ok(col_datum)
}

/// DecodeColumnValueWithDatum decodes data to an existing Datum according to the column info.
///
/// Like Go, `result` holds what `DecodeOne` / `Unflatten` produced even when they fail.
// go: DecodeColumnValueWithDatum L1 [M-ERRVAL]
pub fn decode_column_value_with_datum(
    data: &[u8],
    ft: &FieldType,
    loc: Option<&Arc<Location>>,
    result: &mut Datum,
) -> Result<(), Error> {
    let (_, d, err) = decode_one(data);
    *result = d;
    err?;
    let (d, err) = unflatten(std::mem::take(result), ft, loc);
    *result = d;
    err
}

/// DecodeRowWithMapNew decode a row to datum map.
// go: DecodeRowWithMapNew L1
pub fn decode_row_with_map_new(
    b: &[u8],
    cols: &HashMap<i64, Arc<FieldType>>,
    loc: Option<&Arc<Location>>,
    row: Option<HashMap<i64, Datum>>,
) -> Result<HashMap<i64, Datum>, Error> {
    let row = row.unwrap_or_else(|| HashMap::with_capacity(cols.len()));
    if b.len() == 1 && b[0] == NilFlag {
        return Ok(row);
    }

    let req_cols: Vec<ColInfo> = cols
        .iter()
        .map(|(&id, tp)| ColInfo {
            id,
            ft: Arc::clone(tp),
            ..ColInfo::default()
        })
        .collect();
    let rd = rowcodec::new_datum_map_decoder(req_cols, loc.cloned());
    rd.decode_to_datum_map(b, Some(row))
}

/// DecodeRowWithMap decodes a byte slice into datums with an existing row map.
/// Row layout: colID1, value1, colID2, value2, .....
// go: DecodeRowWithMap L1
pub fn decode_row_with_map(
    mut b: &[u8],
    cols: &HashMap<i64, Arc<FieldType>>,
    loc: Option<&Arc<Location>>,
    row: Option<HashMap<i64, Datum>>,
) -> Result<HashMap<i64, Datum>, Error> {
    let mut row = row.unwrap_or_else(|| HashMap::with_capacity(cols.len()));
    if b.len() == 1 && b[0] == NilFlag {
        return Ok(row);
    }
    let mut cnt = 0usize;
    while !b.is_empty() {
        // Get col id.
        let (data, rest) = cut_one(b)?;
        b = rest;
        let (_, cid, err) = decode_one(data);
        err?;
        // Get col value.
        let (data, rest) = cut_one(b)?;
        b = rest;
        let id = cid.get_int64();
        if let Some(ft) = cols.get(&id) {
            let (_, v, err) = decode_one(data);
            err?;
            let (v, err) = unflatten(v, ft, loc);
            err?;
            row.insert(id, v);
            cnt += 1;
            if cnt == cols.len() {
                // Get enough data.
                break;
            }
        }
    }
    Ok(row)
}

/// DecodeRowToDatumMap decodes a byte slice into datums.
/// Row layout: colID1, value1, colID2, value2, .....
/// Default value columns, generated columns and handle columns are unprocessed.
// go: DecodeRowToDatumMap L1
pub fn decode_row_to_datum_map(
    b: &[u8],
    cols: &HashMap<i64, Arc<FieldType>>,
    loc: Option<&Arc<Location>>,
) -> Result<HashMap<i64, Datum>, Error> {
    if !rowcodec::is_new_format(b) {
        return decode_row_with_map(b, cols, loc, None);
    }
    decode_row_with_map_new(b, cols, loc, None)
}

/// DecodeHandleToDatumMap decodes a handle into datum map.
///
/// Returns the map with the error as Go (M-ERRVAL); Go's nil map is an empty map.
// go: DecodeHandleToDatumMap L1 [M-ERRVAL]
pub fn decode_handle_to_datum_map(
    handle: Option<&Handle>,
    handle_col_ids: &[i64],
    cols: &HashMap<i64, Arc<FieldType>>,
    loc: Option<&Arc<Location>>,
    row: Option<HashMap<i64, Datum>>,
) -> (HashMap<i64, Datum>, Result<(), Error>) {
    let Some(handle) = handle else {
        return (row.unwrap_or_default(), Ok(()));
    };
    if handle_col_ids.is_empty() {
        return (row.unwrap_or_default(), Ok(()));
    }
    let mut row = row.unwrap_or_else(|| HashMap::with_capacity(cols.len()));
    for (idx, id) in handle_col_ids.iter().enumerate() {
        let Some(ft) = cols.get(id) else {
            continue;
        };
        if need_restored_data(ft) {
            continue;
        }
        let (d, err) = decode_handle_to_datum(handle, ft, idx);
        if let Err(err) = err {
            return (row, Err(err));
        }
        let (d, err) = unflatten(d, ft, loc);
        if let Err(err) = err {
            return (row, Err(err));
        }
        row.entry(*id).or_insert(d);
    }
    (row, Ok(()))
}

/// decodeHandleToDatum decodes a handle to a specific column datum.
// go: decodeHandleToDatum L1 [M-ERRVAL]
fn decode_handle_to_datum(
    handle: &Handle,
    ft: &FieldType,
    idx: usize,
) -> (Datum, Result<(), Error>) {
    if handle.is_int() {
        let d = if mysql::has_unsigned_flag(ft.get_flag()) {
            tidb_types::datum::new_uint_datum(handle.int_value() as u64)
        } else {
            new_int_datum(handle.int_value())
        };
        return (d, Ok(()));
    }
    // Decode common handle to Datum.
    let (_, d, err) = decode_one(&handle.encoded_col(idx as isize));
    (d, err)
}

/// CutRowNew cuts encoded row into byte slices and return columns' byte slice.
/// Row layout: colID1, value1, colID2, value2, .....
///
/// An empty entry (or an empty result) is Go's nil.
// go: CutRowNew L1
pub fn cut_row_new<'a>(
    mut data: &'a [u8],
    col_ids: &HashMap<i64, usize>,
) -> Result<Vec<&'a [u8]>, Error> {
    if data.len() == 1 && data[0] == NilFlag {
        return Ok(Vec::new());
    }

    let mut cnt = 0usize;
    let mut row: Vec<&[u8]> = vec![&[]; col_ids.len()];
    while !data.is_empty() && cnt < col_ids.len() {
        // Get col id.
        let (rest, cid) = cut_column_id(data)?;
        data = rest;

        // Get col value.
        let (b, rest) = cut_one(data)?;
        data = rest;

        if let Some(&offset) = col_ids.get(&cid) {
            row[offset] = b;
            cnt += 1;
        }
    }
    Ok(row)
}

/// Unflatten converts a raw datum to a column datum.
///
/// Returns the datum with the error as Go does (M-ERRVAL): on error it is the input datum,
/// possibly half converted.
// go: Unflatten L1 [M-ERRVAL]
pub fn unflatten(
    mut datum: Datum,
    ft: &FieldType,
    loc: Option<&Arc<Location>>,
) -> (Datum, Result<(), Error>) {
    if datum.is_null() {
        return (datum, Ok(()));
    }
    match ft.get_type() {
        mysql::TypeFloat => {
            let f = datum.get_float64() as f32;
            datum.set_float32(f);
            return (datum, Ok(()));
        }
        mysql::TypeVarchar
        | mysql::TypeString
        | mysql::TypeVarString
        | mysql::TypeTinyBlob
        | mysql::TypeMediumBlob
        | mysql::TypeBlob
        | mysql::TypeLongBlob => {
            let s = datum.get_string().to_vec();
            datum.set_string(s, ft.get_collate().to_owned());
        }
        mysql::TypeTiny
        | mysql::TypeShort
        | mysql::TypeYear
        | mysql::TypeInt24
        | mysql::TypeLong
        | mysql::TypeLonglong
        | mysql::TypeDouble => {
            return (datum, Ok(()));
        }
        mysql::TypeDate | mysql::TypeDatetime | mysql::TypeTimestamp => {
            let mut t = new_time(ZeroCoreTime, ft.get_type(), ft.get_decimal());
            if let Err(err) = t.from_packed_uint(datum.get_uint64()) {
                return (datum, Err(err));
            }
            if ft.get_type() == mysql::TypeTimestamp
                && !t.is_zero()
                && let Err(err) = crate::rowcodec::decoder::convert_from_utc(&mut t, loc)
            {
                return (datum, Err(err));
            }
            datum.set_uint64(0);
            datum.set_mysql_time(t);
            return (datum, Ok(()));
        }
        mysql::TypeDuration => {
            // duration should read fsp from column meta data
            let dur = Duration {
                duration: datum.get_int64(),
                fsp: ft.get_decimal(),
            };
            datum.set_mysql_duration(dur);
            return (datum, Ok(()));
        }
        mysql::TypeEnum => {
            // ignore error deliberately, to read empty enum value.
            let enum_ = parse_enum_value(ft.get_elems().unwrap_or(&[]), datum.get_uint64())
                .unwrap_or_default();
            datum.set_mysql_enum(enum_, ft.get_collate().to_owned());
            return (datum, Ok(()));
        }
        mysql::TypeSet => {
            let set = match parse_set_value(ft.get_elems().unwrap_or(&[]), datum.get_uint64()) {
                Ok(set) => set,
                Err(err) => return (datum, Err(err)),
            };
            datum.set_mysql_set(set, ft.get_collate().to_owned());
            return (datum, Ok(()));
        }
        mysql::TypeBit => {
            let val = datum.get_uint64();
            let byte_size = (ft.get_flen() + 7) >> 3;
            datum.set_uint64(0);
            datum.set_mysql_bit(new_binary_literal_from_uint(val, byte_size));
        }
        _ => {}
    }
    (datum, Ok(()))
}

/// EncodeIndexSeekKey encodes an index value to kv.Key.
// go: EncodeIndexSeekKey L1
pub fn encode_index_seek_key(table_id: i64, idx_id: i64, encoded_value: &[u8]) -> Key {
    let mut key = Vec::with_capacity(RecordRowKeyLen + encoded_value.len());
    append_table_index_prefix(&mut key, table_id);
    encode_int(&mut key, idx_id);
    key.extend_from_slice(encoded_value);
    Key(key)
}

/// CutIndexKeyTo cuts encoded index key into colIDs to bytes slices.
/// The caller should prepare the memory of the result values.
// go: CutIndexKeyTo L1
pub fn cut_index_key_to<'a>(
    key: &'a [u8],
    values: &mut [Cow<'a, [u8]>],
) -> Result<&'a [u8], Error> {
    let mut b = &key[prefixLen + idLen..];
    for v in values.iter_mut() {
        let (val, rest) = cut_one(b)?;
        b = rest;
        *v = Cow::Borrowed(val);
    }
    Ok(b)
}

/// The index column values, the rest of an index key, and the error (M-ERRVAL).
pub type CutIndexKeyResult<'a> = (Vec<Cow<'a, [u8]>>, &'a [u8], Result<(), Error>);

/// CutIndexKeyNew cuts encoded index key into colIDs to bytes slices.
/// The returned value b is the remaining bytes of the key which would be empty if it is unique index or handle data
/// if it is non-unique index.
///
/// Like Go, on error the values cut so far are returned with it (and an empty rest).
// go: CutIndexKeyNew L1 [M-ERRVAL]
pub fn cut_index_key_new(key: &[u8], length: usize) -> CutIndexKeyResult<'_> {
    let mut values = vec![Cow::Borrowed(&[][..]); length];
    match cut_index_key_to(key, &mut values) {
        Ok(b) => (values, b, Ok(())),
        Err(err) => (values, &[], Err(err)),
    }
}

/// HandleStatus is the handle status in index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandleStatus {
    /// HandleDefault means decode handle value as int64 or bytes when DecodeIndexKV.
    HandleDefault,
    /// HandleIsUnsigned means decode handle value as uint64 when DecodeIndexKV.
    HandleIsUnsigned,
    /// HandleNotNeeded means no need to decode handle value when DecodeIndexKV.
    HandleNotNeeded,
}

/// reEncodeHandle encodes the handle as a Datum so it can be properly decoded later.
/// If it is common handle, it returns the encoded column values.
/// If it is int handle, it is encoded as int Datum or uint Datum decided by the unsigned.
// go: reEncodeHandle L1
fn re_encode_handle(handle: &Handle, unsigned: bool) -> Result<Vec<Cow<'static, [u8]>>, Error> {
    let handle_col_len = if handle.is_int() {
        1
    } else {
        handle.num_cols() as usize
    };
    let result = Vec::with_capacity(handle_col_len);
    re_encode_handle_to(handle, unsigned, &[], result)
}

// go: reEncodeHandleTo L1
fn re_encode_handle_to<'a>(
    handle: &Handle,
    unsigned: bool,
    buf: &[u8],
    mut result: Vec<Cow<'a, [u8]>>,
) -> Result<Vec<Cow<'a, [u8]>>, Error> {
    if !handle.is_int() {
        let handle_col_len = handle.num_cols();
        for i in 0..handle_col_len {
            result.push(Cow::Owned(handle.encoded_col(i).into_owned()));
        }
        return Ok(result);
    }
    let mut handle_datum = new_int_datum(handle.int_value());
    if unsigned {
        let u = handle_datum.get_uint64();
        handle_datum.set_uint64(u);
    }
    let mut int_handle_bytes = buf.to_vec();
    let err = encode_value(
        &utc(),
        &mut int_handle_bytes,
        std::slice::from_ref(&handle_datum),
    );
    result.push(Cow::Owned(int_handle_bytes));
    err.map(|_| result)
}

/// reEncodeHandleConsiderNewCollation encodes the handle as a Datum so it can be properly decoded later.
// go: reEncodeHandleConsiderNewCollation L1
fn re_encode_handle_consider_new_collation(
    handle: &Handle,
    columns: &[ColInfo],
    restore_data: &[u8],
) -> Result<Vec<Cow<'static, [u8]>>, Error> {
    let handle_col_len = handle.num_cols();
    let mut c_handle_bytes: Vec<Cow<'static, [u8]>> = Vec::with_capacity(handle_col_len as usize);
    for i in 0..handle_col_len {
        c_handle_bytes.push(Cow::Owned(handle.encoded_col(i).into_owned()));
    }
    if restore_data.is_empty() {
        return Ok(c_handle_bytes);
    }
    // Remove some extra columns(ID < 0), such like `model.ExtraPhysTblID`.
    // They are not belong to common handle and no need to restore data.
    let mut idx = columns.len();
    while idx > 0 && columns[idx - 1].id < 0 {
        idx -= 1;
    }
    decode_restored_values_v5(&columns[..idx], c_handle_bytes, restore_data)
}

// go: decodeRestoredValues L1
fn decode_restored_values(
    columns: &[ColInfo],
    restored_val: &[u8],
) -> Result<Vec<Cow<'static, [u8]>>, Error> {
    let mut col_ids: HashMap<i64, usize> = HashMap::with_capacity(columns.len());
    for (i, col) in columns.iter().enumerate() {
        col_ids.insert(col.id, i);
    }
    // We don't need to decode handle here, and colIDs >= 0 always.
    let rd = rowcodec::new_byte_decoder(columns.to_vec(), vec![-1], None, None);
    let result_values = rd.decode_to_bytes_no_handle(&col_ids, restored_val)?;
    Ok(result_values.into_iter().map(Cow::Owned).collect())
}

/// decodeRestoredValuesV5 decodes index values whose format is introduced in TiDB 5.0.
/// Unlike the format in TiDB 4.0, the new format is optimized for storage space:
/// 1. If the index is a composed index, only the non-binary string column's value need to write to value, not all.
/// 2. If a string column's collation is _bin, then we only write the number of the truncated spaces to value.
/// 3. If a string column is char, not varchar, then we use the sortKey directly.
// go: decodeRestoredValuesV5 L1
fn decode_restored_values_v5<'a>(
    columns: &[ColInfo],
    results: Vec<Cow<'a, [u8]>>,
    restored_val: &[u8],
) -> Result<Vec<Cow<'a, [u8]>>, Error> {
    let col_id_offsets = build_column_id_offsets(columns);
    let col_infos_need_restore = build_restored_column(columns);
    let rd = rowcodec::new_byte_decoder(col_infos_need_restore, Vec::new(), None, None);
    let new_results = rd.decode_to_bytes_no_handle(&col_id_offsets, restored_val)?;
    let mut out: Vec<Cow<'a, [u8]>> = Vec::with_capacity(new_results.len());
    for (i, new_result) in new_results.into_iter().enumerate() {
        let no_restore_data = new_result.is_empty();
        if no_restore_data {
            out.push(results[i].clone());
            continue;
        }
        if is_bin_collation(columns[i].ft.get_collate()) {
            let no_padding_datum = decode_column_value(&results[i], &columns[i].ft, None)?;
            let padding_count_datum =
                decode_column_value(&new_result, &new_field_type(mysql::TypeLonglong), None)?;
            let (no_padding_str, padding_count) = (
                no_padding_datum.get_string(),
                padding_count_datum.get_int64() as isize,
            );
            // Skip if padding count is 0.
            if padding_count == 0 {
                out.push(results[i].clone());
                continue;
            }
            let mut new_datum = no_padding_datum.clone();
            let mut s = no_padding_str.to_vec();
            // strings.Repeat panics on a negative count.
            assert!(padding_count >= 0, "strings: negative Repeat count");
            // A corrupted padding count asks for up to 2^63 spaces: Go dies with "fatal error:
            // out of memory" (not recoverable); Rust would abort. The port panics instead
            // when the allocation fails (L2, spec §4; upstream-pitfall candidate spec §5).
            if s.try_reserve_exact(padding_count as usize).is_err() {
                panic!("runtime: out of memory: cannot allocate {padding_count} bytes");
            }
            s.extend(std::iter::repeat_n(b' ', padding_count as usize));
            let collation = new_datum.collation().to_owned();
            new_datum.set_string(s, collation);
            let mut nr = vec![rowcodec::BytesFlag];
            encode_bytes(&mut nr, new_datum.get_bytes());
            out.push(Cow::Owned(nr));
            continue;
        }
        out.push(Cow::Owned(new_result));
    }
    Ok(out)
}

// go: buildColumnIDOffsets L1
fn build_column_id_offsets(all_cols: &[ColInfo]) -> HashMap<i64, usize> {
    let mut col_id_offsets = HashMap::with_capacity(all_cols.len());
    for (i, col) in all_cols.iter().enumerate() {
        col_id_offsets.insert(col.id, i);
    }
    col_id_offsets
}

// go: buildRestoredColumn L1
fn build_restored_column(all_cols: &[ColInfo]) -> Vec<ColInfo> {
    let mut restored_columns = Vec::with_capacity(all_cols.len());
    for col in all_cols {
        if !need_restored_data(&col.ft) {
            continue;
        }
        let ft = if is_bin_collation(col.ft.get_collate()) {
            // Change the fieldType from string to uint since we store the number of the truncated spaces.
            // NOTE: the corresponding datum is generated as `types.NewUintDatum(paddingSize)`, and the raw data is
            // encoded via `encodeUint`. Thus we should mark the field type as unsigened here so that the BytesDecoder
            // can decode it correctly later. Otherwise there might be issues like #47115.
            let mut ft = new_field_type(mysql::TypeLonglong);
            ft.add_flag(mysql::UnsignedFlag);
            Arc::new(ft)
        } else {
            Arc::clone(&col.ft)
        };
        restored_columns.push(ColInfo {
            id: col.id,
            ft,
            ..ColInfo::default()
        });
    }
    restored_columns
}

// go: decodeIndexKvOldCollation L1
fn decode_index_kv_old_collation<'a>(
    key: &'a [u8],
    value: &[u8],
    hd_status: HandleStatus,
    buf: &[u8],
    mut result_values: Vec<Cow<'a, [u8]>>,
) -> Result<Vec<Cow<'a, [u8]>>, Error> {
    let b = cut_index_key_to(key, &mut result_values)?;
    if hd_status == HandleStatus::HandleNotNeeded {
        return Ok(result_values);
    }
    let handle;
    if !b.is_empty() {
        // non-unique index
        handle = decode_handle_in_index_key(b)?;
        result_values = re_encode_handle_to(
            &handle,
            hd_status == HandleStatus::HandleIsUnsigned,
            buf,
            result_values,
        )?;
    } else {
        // In unique int handle index.
        handle = decode_int_handle_in_index_value(value);
        result_values = re_encode_handle_to(
            &handle,
            hd_status == HandleStatus::HandleIsUnsigned,
            buf,
            result_values,
        )?;
    }
    Ok(result_values)
}

// go: getIndexVersion L1
fn get_index_version(value: &[u8]) -> isize {
    if value.len() <= MaxOldEncodeValueLen {
        return 0;
    }
    let tail_len = value[0] as isize;
    if (tail_len == 0 || tail_len == 1) && value[1] == IndexVersionFlag {
        return value[2] as isize;
    }
    0
}

/// DecodeIndexKVEx looks like DecodeIndexKV, the difference is that it tries to reduce allocations.
// go: DecodeIndexKVEx L1
pub fn decode_index_kv_ex<'a>(
    key: &'a [u8],
    value: &[u8],
    cols_len: usize,
    hd_status: HandleStatus,
    columns: &[ColInfo],
    buf: &[u8],
    pre_alloc: Vec<Cow<'a, [u8]>>,
) -> Result<Vec<Cow<'a, [u8]>>, Error> {
    if value.len() <= MaxOldEncodeValueLen {
        return decode_index_kv_old_collation(key, value, hd_status, buf, pre_alloc);
    }
    if get_index_version(value) == 1 {
        return decode_index_kv_for_clustered_index_version1(
            key, value, cols_len, hd_status, columns,
        );
    }
    decode_index_kv_general(key, value, cols_len, hd_status, columns)
}

/// DecodeIndexKV uses to decode index key values.
///
/// ```text
/// `colsLen` is expected to be index columns count.
/// `columns` is expected to be index columns + handle columns(if hdStatus is not HandleNotNeeded).
/// ```
// go: DecodeIndexKV L1
pub fn decode_index_kv<'a>(
    key: &'a [u8],
    value: &[u8],
    cols_len: usize,
    hd_status: HandleStatus,
    columns: &[ColInfo],
) -> Result<Vec<Cow<'a, [u8]>>, Error> {
    if value.len() <= MaxOldEncodeValueLen {
        let mut pre_alloc = Vec::with_capacity(cols_len + columns.len());
        pre_alloc.resize(cols_len, Cow::Borrowed(&[][..]));
        return decode_index_kv_old_collation(key, value, hd_status, &[], pre_alloc);
    }
    if get_index_version(value) == 1 {
        return decode_index_kv_for_clustered_index_version1(
            key, value, cols_len, hd_status, columns,
        );
    }
    decode_index_kv_general(key, value, cols_len, hd_status, columns)
}

/// DecodeIndexHandle uses to decode the handle from index key/value.
///
/// `None` is Go's nil handle (from [`decode_handle_in_index_value`]).
// go: DecodeIndexHandle L1
pub fn decode_index_handle(
    key: &[u8],
    value: &[u8],
    cols_len: usize,
) -> Result<Option<Handle>, Error> {
    let mut b = &key[prefixLen + idLen..];
    for _ in 0..cols_len {
        let (_, rest) = cut_one(b)?;
        b = rest;
    }
    if !b.is_empty() {
        let mut handle = decode_handle_in_index_key(b)?;
        // If len(value) >= 9, it may contain partition id.
        // We should decode it and return a partition handle.
        if value.len() >= 9 {
            let seg = split_index_value(value);
            if let Some(partition_id) = seg.partition_id.filter(|p| !p.is_empty()) {
                let (_, pid) = decode_int(partition_id)?;
                // For GlobalIndexVersionV1+, the handle from the key may already be a
                // PartitionHandle (partition ID encoded in key). To avoid creating a
                // nested PartitionHandle, extract the inner handle first.
                // For V1: use partition ID from value (authoritative source).
                // TODO: For V2+, use partition ID from key (PartitionHandle) instead.
                if let Handle::Partition(ph) = handle {
                    handle = *ph.handle;
                }
                handle = Handle::Partition(new_partition_handle(pid, handle));
            }
        }
        return Ok(Some(handle));
    } else if value.len() >= 8 {
        return decode_handle_in_index_value(value);
    }
    // Should never execute to here.
    Err(errors::errorf(
        "no handle in index key: %v, value: %v",
        &[Arg::Bytes(key.to_vec()), Arg::Bytes(value.to_vec())],
    ))
}

// go: decodeHandleInIndexKey L1
fn decode_handle_in_index_key(key_suffix: &[u8]) -> Result<Handle, Error> {
    // Check if this is a PartitionHandle (for global non-unique indexes V1+)
    if !key_suffix.is_empty() && key_suffix[0] == PartitionIDFlag {
        // Format: PartitionIDFlag + partition_id (8 bytes) + inner_handle
        let key_suffix = &key_suffix[1..]; // Skip the flag
        let (remain, part_id) = decode_int(key_suffix)?;
        // Decode the inner handle
        let inner_handle = decode_handle_in_index_key(remain)?;
        return Ok(Handle::Partition(new_partition_handle(
            part_id,
            inner_handle,
        )));
    }

    let (remain, d, err) = decode_one(key_suffix);
    err?;
    if remain.is_empty() && d.kind() == KindInt64 {
        return Ok(Handle::Int(IntHandle(d.get_int64())));
    }
    Ok(Handle::Common(Arc::new(new_common_handle(
        key_suffix.to_vec(),
    )?)))
}

/// DecodeHandleInIndexValue decodes handle in unqiue index value.
///
/// Go returns a nil handle for a value without one (a unique index entry with NULLs keeps
/// its handle in the key): `None`. With a partition ID and no handle Go returns
/// `PartitionHandle{nil}`, whose every method panics; the port returns `None` too (L2,
/// spec §4).
// go: DecodeHandleInIndexValue L1
pub fn decode_handle_in_index_value(value: &[u8]) -> Result<Option<Handle>, Error> {
    if value.len() <= MaxOldEncodeValueLen {
        return Ok(Some(decode_int_handle_in_index_value(value)));
    }
    let seg = split_index_value(value);
    let mut handle: Option<Handle> = None;
    if let Some(int_handle) = seg.int_handle.filter(|b| !b.is_empty()) {
        handle = Some(decode_int_handle_in_index_value(int_handle));
    }
    if let Some(common_handle) = seg.common_handle.filter(|b| !b.is_empty()) {
        handle = Some(Handle::Common(Arc::new(new_common_handle(
            common_handle.to_vec(),
        )?)));
    }
    if let Some(partition_id) = seg.partition_id.filter(|b| !b.is_empty()) {
        let (_, pid) = decode_int(partition_id)?;
        handle = handle.map(|inner| Handle::Partition(new_partition_handle(pid, inner)));
    }
    Ok(handle)
}

/// DecodeIntHandleInIndexValue uses to decode index value as int handle id.
// go: DecodeIntHandleInIndexValue L1
pub fn decode_int_handle_in_index_value(data: &[u8]) -> Handle {
    let b: [u8; 8] = data[..8].try_into().unwrap();
    Handle::Int(IntHandle(u64::from_be_bytes(b) as i64))
}

/// EncodeTableIndexPrefix encodes index prefix with tableID and idxID.
// go: EncodeTableIndexPrefix L1
pub fn encode_table_index_prefix(table_id: i64, idx_id: i64) -> Key {
    let mut key = Vec::with_capacity(prefixLen + idLen);
    append_table_index_prefix(&mut key, table_id);
    encode_int(&mut key, idx_id);
    Key(key)
}

/// EncodeTablePrefix encodes the table prefix to generate a key
// go: EncodeTablePrefix L1
pub fn encode_table_prefix(table_id: i64) -> Key {
    let mut key = Vec::with_capacity(tablePrefixLength + idLen);
    key.extend_from_slice(tablePrefix);
    encode_int(&mut key, table_id);
    Key(key)
}

/// appendTableRecordPrefix appends table record prefix  "t[tableID]_r".
// go: appendTableRecordPrefix L1
fn append_table_record_prefix(buf: &mut Vec<u8>, table_id: i64) {
    buf.extend_from_slice(tablePrefix);
    encode_int(buf, table_id);
    buf.extend_from_slice(recordPrefixSep);
}

/// appendTableIndexPrefix appends table index prefix  "t[tableID]_i".
// go: appendTableIndexPrefix L1
fn append_table_index_prefix(buf: &mut Vec<u8>, table_id: i64) {
    buf.extend_from_slice(tablePrefix);
    encode_int(buf, table_id);
    buf.extend_from_slice(indexPrefixSep);
}

/// GenTableRecordPrefix composes record prefix with tableID: "t[tableID]_r".
// go: GenTableRecordPrefix L1
pub fn gen_table_record_prefix(table_id: i64) -> Key {
    let mut buf = Vec::with_capacity(tablePrefix.len() + 8 + recordPrefixSep.len());
    append_table_record_prefix(&mut buf, table_id);
    Key(buf)
}

/// GenTableIndexPrefix composes index prefix with tableID: "t[tableID]_i".
// go: GenTableIndexPrefix L1
pub fn gen_table_index_prefix(table_id: i64) -> Key {
    let mut buf = Vec::with_capacity(tablePrefix.len() + 8 + indexPrefixSep.len());
    append_table_index_prefix(&mut buf, table_id);
    Key(buf)
}

/// IsRecordKey is used to check whether the key is an record key.
// go: IsRecordKey L1
pub fn is_record_key(k: &[u8]) -> bool {
    k.len() > 11 && k[0] == b't' && k[10] == b'r'
}

/// IsIndexKey is used to check whether the key is an index key.
// go: IsIndexKey L1
pub fn is_index_key(k: &[u8]) -> bool {
    k.len() > 11 && k[0] == b't' && k[10] == b'i'
}

/// IsTableKey is used to check whether the key is a table key.
// go: IsTableKey L1
pub fn is_table_key(k: &[u8]) -> bool {
    k.len() == 9 && k[0] == b't'
}

/// GenTablePrefix composes table record and index prefix: "t[tableID]".
// go: GenTablePrefix L1
pub fn gen_table_prefix(table_id: i64) -> Key {
    let mut buf = Vec::with_capacity(tablePrefix.len() + 8);
    buf.extend_from_slice(tablePrefix);
    encode_int(&mut buf, table_id);
    Key(buf)
}

/// GetTableHandleKeyRange returns table handle's key range with tableID.
// go: GetTableHandleKeyRange L1
pub fn get_table_handle_key_range(table_id: i64) -> (Vec<u8>, Vec<u8>) {
    let start_key = encode_row_key_with_handle(table_id, &Handle::Int(IntHandle(i64::MIN)));
    let end_key = encode_row_key_with_handle(table_id, &Handle::Int(IntHandle(i64::MAX)));
    (start_key.0, end_key.0)
}

/// GetTableIndexKeyRange returns table index's key range with tableID and indexID.
// go: GetTableIndexKeyRange L1
pub fn get_table_index_key_range(table_id: i64, index_id: i64) -> (Vec<u8>, Vec<u8>) {
    let start_key = encode_index_seek_key(table_id, index_id, &[]);
    let end_key = encode_index_seek_key(table_id, index_id, &[255]);
    (start_key.0, end_key.0)
}

/// GetIndexKeyBuf reuse or allocate buffer
// go: GetIndexKeyBuf L1
pub fn get_index_key_buf(buf: Option<Vec<u8>>, default_cap: usize) -> Vec<u8> {
    match buf {
        Some(mut buf) => {
            buf.clear();
            buf
        }
        None => Vec::with_capacity(default_cap),
    }
}

/// GenIndexKey generates index key using input physical table id
///
/// `indexed_values` is truncated in place as Go's slice argument is (`TruncateIndexValues`).
/// Go returns `nil, false, err` on error.
// go: GenIndexKey L1
#[allow(clippy::too_many_arguments)] // Go's signature
pub fn gen_index_key(
    loc: &Arc<Location>,
    tbl_info: &TableInfo,
    idx_info: &IndexInfo,
    phy_tbl_id: i64,
    indexed_values: &mut [Datum],
    h: Option<&Handle>,
    buf: Option<Vec<u8>>,
) -> Result<(Vec<u8>, bool), Error> {
    let mut distinct = false;
    if idx_info.unique {
        // See https://dev.mysql.com/doc/refman/5.7/en/create-index.html
        // A UNIQUE index creates a constraint such that all values in the index must be distinct.
        // An error occurs if you try to add a new row with a key value that matches an existing row.
        // For all engines, a UNIQUE index permits multiple NULL values for columns that can contain NULL.
        distinct = true;
        for cv in indexed_values.iter() {
            if cv.is_null() {
                distinct = false;
                break;
            }
        }
    }
    // For string columns, indexes can be created using only the leading part of column values,
    // using col_name(length) syntax to specify an index prefix length.
    truncate_index_values(tbl_info, idx_info, indexed_values);
    let mut key = get_index_key_buf(buf, RecordRowKeyLen + indexed_values.len() * 9 + 9);
    append_table_index_prefix(&mut key, phy_tbl_id);
    encode_int(&mut key, idx_info.id);
    encode_key(loc, &mut key, indexed_values)?;
    if !distinct && let Some(h) = h {
        // For PartitionHandle on global indexes V1+, we must encode BOTH partition ID and inner handle
        // in the key to prevent collisions when different partitions have duplicate handles.
        // This is critical after EXCHANGE PARTITION, which can create duplicate _tidb_rowid values.
        // Only use the new format for version >= V1. Legacy indexes (version 0) use the old format.
        if idx_info.global_index_version >= GlobalIndexVersionV1 {
            if tbl_info.has_clustered_index() {
                return Err(errors::new(
                    "clustered index is not supported in GlobalIndexVersionV1+",
                ));
            }
            let Handle::Partition(ph) = h else {
                return Err(errors::new(
                    "handle is not a PartitionHandle in GlobalIndexVersionV1+",
                ));
            };
            // Encode as: PartitionIDFlag + partition_id (8 bytes) + inner_handle_encoded
            key.push(PartitionIDFlag);
            encode_int(&mut key, ph.partition_id);
        }

        if h.is_int() {
            // We choose the efficient path here instead of calling `codec.EncodeKey`
            // because the int handle must be an int64, and it must be comparable.
            // This remains correct until codec.encodeSignedInt is changed.
            key.push(IntHandleFlag);
            encode_int(&mut key, h.int_value());
        } else {
            key.extend_from_slice(&h.encoded());
        }
    }
    Ok((key, distinct))
}

/// TempIndexPrefix used to generate temporary index ID from index ID.
pub const TempIndexPrefix: i64 = 0x7fff000000000000;

/// IndexIDMask used to get index id from index ID/temp index ID.
pub const IndexIDMask: i64 = 0xffffffffffff;

/// IndexKey2TempIndexKey generates a temporary index key.
// go: IndexKey2TempIndexKey L1
pub fn index_key2_temp_index_key(key: &mut [u8]) {
    let idx_id_bytes: [u8; 8] = key[prefixLen..prefixLen + idLen].try_into().unwrap();
    let idx_id = decode_cmp_uint_to_int(u64::from_be_bytes(idx_id_bytes));
    let eid = encode_int_to_cmp_uint(TempIndexPrefix | idx_id);
    key[prefixLen..prefixLen + 8].copy_from_slice(&eid.to_be_bytes());
}

/// IsTempIndexKey checks whether the input key is for a temp index.
// go: IsTempIndexKey L1
pub fn is_temp_index_key(index_key: &[u8]) -> bool {
    let index_id_key: [u8; 8] = index_key[prefixLen..prefixLen + 8].try_into().unwrap();
    let index_id = decode_cmp_uint_to_int(u64::from_be_bytes(index_id_key));
    let temp_index_id = TempIndexPrefix | index_id;
    temp_index_id == index_id
}

/// TruncateIndexValues truncates the index values created using only the leading part of column values.
// go: TruncateIndexValues L1
pub fn truncate_index_values(
    tbl_info: &TableInfo,
    idx_info: &IndexInfo,
    indexed_values: &mut [Datum],
) {
    let idx_cols = idx_info.columns.as_deref().unwrap_or(&[]);
    let tbl_cols = tbl_info.columns.as_deref().unwrap_or(&[]);
    for i in 0..indexed_values.len() {
        let idx_col = &idx_cols[i];
        let tbl_col = &tbl_cols[idx_col.offset as usize];
        truncate_index_value(&mut indexed_values[i], idx_col, tbl_col);
    }
}

/// TruncateIndexValue truncate one value in the index.
// go: TruncateIndexValue L1
pub fn truncate_index_value(v: &mut Datum, idx_col: &IndexColumn, tbl_col: &ColumnInfo) {
    let no_prefix_index = idx_col.length == UnspecifiedLength;
    if no_prefix_index {
        return;
    }
    let not_string_type = v.kind() != KindString && v.kind() != KindBytes;
    if not_string_type {
        return;
    }
    let col_value = v.get_bytes().to_vec();
    let length = idx_col.length;
    if tbl_col.get_charset() == CharsetBin || tbl_col.get_charset() == CharsetASCII {
        // Count character length by bytes if charset is binary or ascii.
        if col_value.len() as isize > length {
            // truncate value and limit its length
            if v.kind() == KindBytes {
                v.set_bytes(col_value[..length as usize].to_vec());
            } else {
                let s = v.get_string()[..length as usize].to_vec();
                v.set_string(s, tbl_col.get_collate().to_owned());
            }
        }
    } else if rune_count(&col_value) as isize > length {
        // Count character length by characters for other rune-based charsets, they are all internally encoded as UTF-8.
        let mut truncate_str: Vec<u8> = Vec::with_capacity(col_value.len());
        let mut rest = &col_value[..];
        for _ in 0..length {
            let (r, size) = decode_rune(rest);
            rest = &rest[size..];
            let mut tmp = [0u8; 4];
            truncate_str.extend_from_slice(r.encode_utf8(&mut tmp).as_bytes());
        }
        // truncate value and limit its length
        v.set_string(truncate_str, tbl_col.get_collate().to_owned());
    }
}

/// IndexValueSegments use to store result of SplitIndexValue.
///
/// `None` is Go's nil slice (Go tests some segments with `!= nil` and others with `len`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IndexValueSegments<'a> {
    pub common_handle: Option<&'a [u8]>,
    pub partition_id: Option<&'a [u8]>,
    pub restored_values: Option<&'a [u8]>,
    pub int_handle: Option<&'a [u8]>,
}

/// SplitIndexValue decodes segments in index value for both non-clustered and clustered table.
// go: SplitIndexValue L1
pub fn split_index_value(value: &[u8]) -> IndexValueSegments<'_> {
    if get_index_version(value) == 0 {
        // For Old Encoding (IntHandle without any others options)
        if value.len() <= MaxOldEncodeValueLen {
            return IndexValueSegments {
                int_handle: Some(value),
                ..IndexValueSegments::default()
            };
        }
        // For IndexValueVersion0
        return split_index_value_for_index_value_version0(value);
    }
    // For IndexValueForClusteredIndexVersion1
    split_index_value_for_clustered_index_version1(value)
}

/// `value[a:b]` with Go's bounds check (Go slices up to the capacity; the inputs have
/// `cap == len`).
// glue: Go slice expression with its panic
fn go_slice(value: &[u8], a: usize, b: usize) -> &[u8] {
    assert!(
        a <= b && b <= value.len(),
        "slice bounds out of range [{a}:{b}] with capacity {}",
        value.len()
    );
    &value[a..b]
}

/// splitIndexValueForIndexValueVersion0 splits index value into segments.
// go: splitIndexValueForIndexValueVersion0 L1
fn split_index_value_for_index_value_version0(value: &[u8]) -> IndexValueSegments<'_> {
    let mut segs = IndexValueSegments::default();
    let tail_len = value[0] as usize;
    let tail = go_slice(value, value.len().wrapping_sub(tail_len), value.len());
    let mut value = go_slice(value, 1, value.len().wrapping_sub(tail_len));
    if tail.len() >= 8 {
        segs.int_handle = Some(&tail[..8]);
    }
    if !value.is_empty() && value[0] == CommonHandleFlag {
        let handle_len = ((value[1] as u16) << 8) + value[2] as u16;
        let handle_end_off = 3 + handle_len as usize;
        segs.common_handle = Some(go_slice(value, 3, handle_end_off));
        value = &value[handle_end_off..];
    }
    if !value.is_empty() && value[0] == PartitionIDFlag {
        segs.partition_id = Some(go_slice(value, 1, 9));
        value = &value[9..];
    }
    if !value.is_empty() && value[0] == RestoreDataFlag {
        segs.restored_values = Some(value);
    }
    segs
}

/// splitIndexValueForClusteredIndexVersion1 splits index value into segments.
// go: splitIndexValueForClusteredIndexVersion1 L1
fn split_index_value_for_clustered_index_version1(value: &[u8]) -> IndexValueSegments<'_> {
    let mut segs = IndexValueSegments::default();
    let tail_len = value[0] as usize;
    // Skip the tailLen and version info.
    let mut value = go_slice(value, 3, value.len().wrapping_sub(tail_len));
    if !value.is_empty() && value[0] == CommonHandleFlag {
        let handle_len = ((value[1] as u16) << 8) + value[2] as u16;
        let handle_end_off = 3 + handle_len as usize;
        segs.common_handle = Some(go_slice(value, 3, handle_end_off));
        value = &value[handle_end_off..];
    }
    if !value.is_empty() && value[0] == PartitionIDFlag {
        segs.partition_id = Some(go_slice(value, 1, 9));
        value = &value[9..];
    }
    if !value.is_empty() && value[0] == RestoreDataFlag {
        segs.restored_values = Some(value);
    }
    segs
}

/// The partition ID datum bytes `codec.EncodeValue(time.UTC, nil, types.NewIntDatum(pid))`.
// glue: shared tail of the index KV decoders
fn partition_id_bytes(partition_id: &[u8]) -> Result<Vec<u8>, Error> {
    let (_, pid) = decode_int(partition_id)?;
    let datum = new_int_datum(pid);
    let mut pid_bytes = Vec::new();
    encode_value(&utc(), &mut pid_bytes, std::slice::from_ref(&datum))?;
    Ok(pid_bytes)
}

// go: decodeIndexKvForClusteredIndexVersion1 L1
fn decode_index_kv_for_clustered_index_version1<'a>(
    key: &'a [u8],
    value: &[u8],
    cols_len: usize,
    hd_status: HandleStatus,
    columns: &[ColInfo],
) -> Result<Vec<Cow<'a, [u8]>>, Error> {
    let segs = split_index_value_for_clustered_index_version1(value);
    let (mut result_values, key_suffix, err) = cut_index_key_new(key, cols_len);
    err?;
    if let Some(restored_values) = segs.restored_values {
        result_values =
            decode_restored_values_v5(&columns[..cols_len], result_values, restored_values)?;
    }
    if hd_status == HandleStatus::HandleNotNeeded {
        return Ok(result_values);
    }
    let handle: CommonHandle = if let Some(common_handle) = segs.common_handle {
        // In unique common handle index.
        new_common_handle(common_handle.to_vec())?
    } else {
        // In non-unique index, decode handle in keySuffix.
        new_common_handle(key_suffix.to_vec())?
    };
    let handle = Handle::Common(Arc::new(handle));
    let handle_bytes = re_encode_handle_consider_new_collation(
        &handle,
        &columns[cols_len..],
        segs.restored_values.unwrap_or(&[]),
    )?;
    result_values.extend(handle_bytes);
    if let Some(partition_id) = segs.partition_id {
        result_values.push(Cow::Owned(partition_id_bytes(partition_id)?));
    }
    Ok(result_values)
}

/// decodeIndexKvGeneral decodes index key value pair of new layout in an extensible way.
// go: decodeIndexKvGeneral L1
fn decode_index_kv_general<'a>(
    key: &'a [u8],
    value: &[u8],
    cols_len: usize,
    hd_status: HandleStatus,
    columns: &[ColInfo],
) -> Result<Vec<Cow<'a, [u8]>>, Error> {
    let segs = split_index_value_for_index_value_version0(value);
    let (mut result_values, key_suffix, err) = cut_index_key_new(key, cols_len);
    err?;
    if let Some(restored_values) = segs.restored_values {
        // new collation
        result_values = decode_restored_values(&columns[..cols_len], restored_values)?;
    }
    if hd_status == HandleStatus::HandleNotNeeded {
        return Ok(result_values);
    }

    let handle = if let Some(int_handle) = segs.int_handle {
        // In unique int handle index.
        decode_int_handle_in_index_value(int_handle)
    } else if let Some(common_handle) = segs.common_handle {
        // In unique common handle index.
        decode_handle_in_index_key(common_handle)?
    } else {
        // In non-unique index, decode handle in keySuffix
        decode_handle_in_index_key(key_suffix)?
    };
    let handle_bytes = re_encode_handle(&handle, hd_status == HandleStatus::HandleIsUnsigned)?;
    result_values.extend(handle_bytes);
    if let Some(partition_id) = segs.partition_id {
        result_values.push(Cow::Owned(partition_id_bytes(partition_id)?));
    }
    Ok(result_values)
}

/// VerifyTableIDForRanges verifies that all given ranges are valid to decode the table id.
///
/// Returns the table IDs collected before an error with it (M-ERRVAL).
// go: VerifyTableIDForRanges L1 [M-ERRVAL]
pub fn verify_table_id_for_ranges(key_ranges: &KeyRanges) -> (Vec<i64>, Result<(), Error>) {
    let mut tids: Vec<i64> = Vec::with_capacity(key_ranges.partition_num() as usize);
    let collect_func = |ranges: &[KeyRange], _: Option<&[isize]>| -> Result<(), Error> {
        if ranges.is_empty() {
            return Ok(());
        }
        let tid = decode_table_id(&ranges[0].start_key.0);
        if tid <= 0 {
            return Err(errors::new("Incorrect keyRange is constrcuted"));
        }
        tids.push(tid);
        for r in &ranges[1..] {
            let tmp_tid = decode_table_id(&r.start_key.0);
            if tmp_tid <= 0 {
                return Err(errors::new("Incorrect keyRange is constrcuted"));
            }
            if tid != tmp_tid {
                return Err(errors::errorf(
                    "Using multi partition's ranges as single table's",
                    &[],
                ));
            }
        }
        Ok(())
    };
    let err = key_ranges.for_each_partition_with_err(collect_func);
    (tids, err)
}

// glue: the assertion of Go TestError (tablecodec_test.go) on the package's private errors,
// which the govec records cannot reach.
#[cfg(test)]
mod tests {
    use tidb_base::mysql;
    use tidb_base::terror::to_sql_error;

    use super::*;

    /// Go `TestError`: every key error converts to its own SQL error code.
    // glue: test
    #[test]
    fn error_codes() {
        for err in [&*errInvalidKey, &*errInvalidRecordKey, &*errInvalidIndexKey] {
            let code = to_sql_error(err).code;
            assert_ne!(code, mysql::ErrUnknown);
            assert_eq!(err.code() as u16, code);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Index value encoders. Brought back from X for block D06.7 (the `SET GLOBAL` system-table
// writer, D05.2 spec §6): Go's `REPLACE INTO mysql.global_variables` writes the value of the
// non-clustered unique primary index with `GenIndexValuePortal`.

/// GenIndexValuePortal is the portal for generating index value.
/// Value layout: see the Go comment of `genIndexValueVersion0` / `GenIndexValueForClusteredIndexVersion1`.
// go: GenIndexValuePortal L1
#[allow(clippy::too_many_arguments)] // Go's signature
pub fn gen_index_value_portal(
    loc: Option<&Arc<Location>>,
    tbl_info: &TableInfo,
    idx_info: &IndexInfo,
    need_restored_data: bool,
    distinct: bool,
    untouched: bool,
    indexed_values: &[Datum],
    h: &Handle,
    partition_id: i64,
    restored_data: &[Datum],
    buf: Option<Vec<u8>>,
) -> Result<Vec<u8>, Error> {
    if tbl_info.is_common_handle && tbl_info.common_handle_version == 1 {
        return gen_index_value_for_clustered_index_version1(
            loc,
            tbl_info,
            idx_info,
            need_restored_data,
            distinct,
            untouched,
            indexed_values,
            h,
            partition_id,
            restored_data,
            buf,
        );
    }
    gen_index_value_version0(
        loc,
        tbl_info,
        idx_info,
        need_restored_data,
        distinct,
        untouched,
        indexed_values,
        h,
        partition_id,
        buf,
    )
}

/// TryGetCommonPkColumnRestoredIds get the IDs of primary key columns which need restored data if the table has common handle.
/// Caller need to make sure the table has common handle.
// go: TryGetCommonPkColumnRestoredIds L1
pub fn try_get_common_pk_column_restored_ids(tbl: &TableInfo) -> Vec<i64> {
    let mut pk_col_ids = Vec::new();
    let Some(pk_idx) = tbl
        .indices
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .find(|idx| idx.primary)
    else {
        return pk_col_ids;
    };
    let cols = tbl.columns.as_deref().unwrap_or(&[]);
    for idx_col in pk_idx.columns.as_deref().unwrap_or(&[]) {
        let col = &cols[idx_col.offset as usize];
        if need_restored_data(&col.field_type) {
            pk_col_ids.push(col.id);
        }
    }
    pk_col_ids
}

/// GenIndexValueForClusteredIndexVersion1 generates the index value for the clustered index with version 1(New in v5.0.0).
// go: GenIndexValueForClusteredIndexVersion1 L1
#[allow(clippy::too_many_arguments)] // Go's signature
pub fn gen_index_value_for_clustered_index_version1(
    loc: Option<&Arc<Location>>,
    tbl_info: &TableInfo,
    idx_info: &IndexInfo,
    idx_val_need_restored_data: bool,
    distinct: bool,
    untouched: bool,
    indexed_values: &[Datum],
    h: &Handle,
    partition_id: i64,
    handle_restored_data: &[Datum],
    buf: Option<Vec<u8>>,
) -> Result<Vec<u8>, Error> {
    let mut idx_val = match buf {
        Some(mut b) => {
            b.clear();
            b
        }
        None => Vec::new(),
    };
    idx_val.push(0);
    let mut tail_len = 0u8;
    // Version info.
    idx_val.push(IndexVersionFlag);
    idx_val.push(1);

    if distinct {
        encode_common_handle(&mut idx_val, h);
    }
    if idx_info.global {
        encode_partition_id(&mut idx_val, partition_id);
    }
    if idx_val_need_restored_data || !handle_restored_data.is_empty() {
        let idx_cols = idx_info.columns.as_deref().unwrap_or(&[]);
        let tbl_cols = tbl_info.columns.as_deref().unwrap_or(&[]);
        let mut col_ids = Vec::with_capacity(idx_cols.len());
        let mut all_restored_data = Vec::with_capacity(handle_restored_data.len() + idx_cols.len());
        for (i, idx_col) in idx_cols.iter().enumerate() {
            let col = &tbl_cols[idx_col.offset as usize];
            // If the column is the primary key's column,
            // the restored data will be written later. Skip writing it here to avoid redundancy.
            if mysql::has_pri_key_flag(col.field_type.get_flag()) {
                continue;
            }
            if tidb_model::column_need_restored_data(idx_col, tbl_cols) {
                col_ids.push(col.id);
                if is_bin_collation(
                    tidb_model::get_idx_changing_field_type(idx_col, col).get_collate(),
                ) {
                    all_restored_data.push(new_uint_datum(
                        tidb_types::stringutil::get_tail_space_count(
                            indexed_values[i].get_string(),
                        ) as u64,
                    ));
                } else {
                    all_restored_data.push(indexed_values[i].clone());
                }
            }
        }

        if !handle_restored_data.is_empty() {
            let pk_col_ids = try_get_common_pk_column_restored_ids(tbl_info);
            col_ids.extend(pk_col_ids);
            all_restored_data.extend_from_slice(handle_restored_data);
        }

        let mut rd = rowcodec::encoder::Encoder::with_enable(true);
        idx_val = rd.encode(loc, &col_ids, &all_restored_data, None, idx_val)?;
    }

    if untouched {
        tail_len = 1;
        idx_val.push(UnCommitIndexKVFlag);
    }
    idx_val[0] = tail_len;

    Ok(idx_val)
}

/// genIndexValueVersion0 create index value for both local and global index.
// go: genIndexValueVersion0 L1
#[allow(clippy::too_many_arguments)] // Go's signature
fn gen_index_value_version0(
    loc: Option<&Arc<Location>>,
    tbl_info: &TableInfo,
    idx_info: &IndexInfo,
    idx_val_need_restored_data: bool,
    distinct: bool,
    untouched: bool,
    indexed_values: &[Datum],
    h: &Handle,
    partition_id: i64,
    buf: Option<Vec<u8>>,
) -> Result<Vec<u8>, Error> {
    let mut idx_val = match buf {
        Some(mut b) => {
            b.clear();
            b
        }
        None => Vec::new(),
    };
    idx_val.push(0);
    let mut new_encode = false;
    let mut tail_len = 0usize;
    if !h.is_int() && distinct {
        encode_common_handle(&mut idx_val, h);
        new_encode = true;
    }
    if idx_info.global {
        encode_partition_id(&mut idx_val, partition_id);
        new_encode = true;
    }
    if idx_val_need_restored_data {
        let tbl_cols = tbl_info.columns.as_deref().unwrap_or(&[]);
        let col_ids: Vec<i64> = idx_info
            .columns
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .map(|col| tbl_cols[col.offset as usize].id)
            .collect();
        let mut rd = rowcodec::encoder::Encoder::with_enable(true);
        // Encode row restored value.
        idx_val = rd.encode(loc, &col_ids, indexed_values, None, idx_val)?;
        new_encode = true;
    }

    if new_encode {
        if h.is_int() && distinct {
            // The len of the idxVal is always >= 10 since len (restoredValue) > 0.
            tail_len += 8;
            idx_val.extend_from_slice(&encode_handle_in_unique_index_value(h, false));
        } else if idx_val.len() < 10 {
            // Padding the len to 10
            let padding_len = 10 - idx_val.len();
            tail_len += padding_len;
            idx_val.resize(idx_val.len() + padding_len, 0);
        }
        if untouched {
            // If index is untouched and fetch here means the key is exists in TiKV, but not in txn mem-buffer,
            // then should also write the untouched index key/value to mem-buffer to make sure the data
            // is consistent with the index in txn mem-buffer.
            tail_len += 1;
            idx_val.push(UnCommitIndexKVFlag);
        }
        idx_val[0] = tail_len as u8;
    } else {
        // Old index value encoding.
        idx_val.clear();
        if distinct {
            idx_val = encode_handle_in_unique_index_value(h, untouched);
        }
        if untouched {
            // If index is untouched and fetch here means the key is exists in TiKV, but not in txn mem-buffer,
            // then should also write the untouched index key/value to mem-buffer to make sure the data
            // is consistent with the index in txn mem-buffer.
            idx_val.push(UnCommitIndexKVFlag);
        }
        if idx_val.is_empty() {
            idx_val.push(b'0');
        }
    }
    Ok(idx_val)
}

/// EncodeHandleInUniqueIndexValue encodes handle in data.
// go: EncodeHandleInUniqueIndexValue L1
pub fn encode_handle_in_unique_index_value(h: &Handle, is_untouched: bool) -> Vec<u8> {
    if h.is_int() {
        return (h.int_value() as u64).to_be_bytes().to_vec();
    }
    let untouched_flag = u8::from(is_untouched);
    let mut out = vec![untouched_flag];
    encode_common_handle(&mut out, h);
    out
}

// go: encodeCommonHandle L1
fn encode_common_handle(idx_val: &mut Vec<u8>, h: &Handle) {
    idx_val.push(CommonHandleFlag);
    let enc = h.encoded();
    let h_len = enc.len() as u16;
    idx_val.push((h_len >> 8) as u8);
    idx_val.push(h_len as u8);
    idx_val.extend_from_slice(&enc);
}

// go: encodePartitionID L1
fn encode_partition_id(idx_val: &mut Vec<u8>, partition_id: i64) {
    idx_val.push(PartitionIDFlag);
    encode_int(idx_val, partition_id);
}
