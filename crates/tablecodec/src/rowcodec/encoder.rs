//! go-file: pkg/util/rowcodec/encoder.go

// Go identifiers (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]
//!
//! The row format v2 encoder. Brought back from X for block D06.7 (the system-table writer of
//! `SET GLOBAL`, D05.2 spec §6, owner decision #58): Go's `REPLACE INTO mysql.global_variables`
//! encodes the new row with `rowcodec.Encoder`, and a non-clustered unique index value with
//! restored data uses it too (`genIndexValueVersion0`).
//!
//! Mapping: Go's `Encoder` embeds `row`; here the encoder owns its own `colIDs` / `offsets` /
//! `data` buffers (the decoder's `Row` is a borrowed view). The sorters of `common.go`
//! (`smallNotNullSorter` …) are `sort.Sort` over unique column IDs, so any correct sort gives
//! the same order; they are a sort of the column-ID / value pairs here.

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::errors;
use tidb_base::gofmt::Arg;
use tidb_base::gostd::time::{Location, utc};
use tidb_base::mysql;
use tidb_kv::Handle;
use tidb_types::context::StrictContext;
use tidb_types::datum::*;

use super::common::CodecVer;
use super::common::crc32_update;
use super::row::{checksumFlagExtra, checksumMaskVersion, rowFlagChecksum, rowFlagLarge};

/// Encoder is used to encode a row.
// go: Encoder L1
#[derive(Debug, Default)]
pub struct Encoder {
    // row
    flags: u8,
    checksum_header: u8,
    num_not_null_cols: u16,
    num_null_cols: u16,
    col_ids: Vec<u8>,
    offsets: Vec<u16>,
    col_ids32: Vec<u32>,
    offsets32: Vec<u32>,
    data: Vec<u8>,
    checksum1: u32,
    checksum2: u32,

    temp_col_ids: Vec<i64>,
    values: Vec<Datum>,
    /// Enable indicates whether this encoder should be use.
    pub enable: bool,
}

/// Checksum is used to calculate and append checksum data into the raw bytes.
// go: Checksum L1 (interface → enum: NoChecksum, RawChecksum)
#[derive(Debug, Clone)]
pub enum Checksum {
    /// NoChecksum indicates no checksum is encoded into the returned raw bytes.
    No,
    /// RawChecksum indicates encode the raw bytes checksum and append it to the raw bytes.
    Raw {
        /// RawChecksum.Handle.
        handle: Handle,
    },
}

// introduced since v8.4.0
const checksumVersionRawHandle: u8 = 2;

impl Encoder {
    /// `rowcodec.Encoder{Enable: enable}`.
    // glue: Go struct literal
    pub fn with_enable(enable: bool) -> Encoder {
        Encoder {
            enable,
            ..Default::default()
        }
    }

    // go: Encoder.large (row.large) L1
    fn large(&self) -> bool {
        self.flags & rowFlagLarge > 0
    }

    /// Encode encodes a row from a datums slice.
    /// `buf` is not truncated before encoding.
    /// Go may return both bytes and an error group; on an error Go returns `nil, err`.
    // go: Encoder.Encode L1
    pub fn encode(
        &mut self,
        loc: Option<&Arc<Location>>,
        col_ids: &[i64],
        values: &[Datum],
        checksum: Option<&Checksum>,
        buf: Vec<u8>,
    ) -> Result<Vec<u8>, Error> {
        self.reset();
        self.append_col_vals(col_ids, values);
        let (num_cols, not_null_idx) = self.reformat_cols();
        self.encode_row_cols(loc, num_cols, not_null_idx)?;
        match checksum.unwrap_or(&Checksum::No) {
            Checksum::No => Ok(self.encode_no_checksum(buf)),
            Checksum::Raw { handle } => Ok(self.encode_raw_checksum(handle, buf)),
        }
    }

    // go: Encoder.reset L1
    fn reset(&mut self) {
        self.flags = 0;
        self.num_not_null_cols = 0;
        self.num_null_cols = 0;
        self.data.clear();
        self.temp_col_ids.clear();
        self.values.clear();
        self.offsets32.clear();
        self.offsets.clear();
        self.checksum_header = 0;
        self.checksum1 = 0;
        self.checksum2 = 0;
    }

    // go: Encoder.appendColVals L1
    fn append_col_vals(&mut self, col_ids: &[i64], values: &[Datum]) {
        for (i, &col_id) in col_ids.iter().enumerate() {
            self.append_col_val(col_id, &values[i]);
        }
    }

    // go: Encoder.appendColVal L1
    fn append_col_val(&mut self, col_id: i64, d: &Datum) {
        if col_id > 255 {
            self.flags |= rowFlagLarge;
        }
        if d.is_null() {
            self.num_null_cols += 1;
        } else {
            self.num_not_null_cols += 1;
        }
        self.temp_col_ids.push(col_id);
        self.values.push(d.clone());
    }

    // go: Encoder.reformatCols L1 (sorters of common.go: sort over unique column IDs)
    fn reformat_cols(&mut self) -> (usize, usize) {
        let num_cols = self.temp_col_ids.len();
        let mut null_idx = num_cols - self.num_null_cols as usize;
        let mut not_null_idx = 0;
        if self.large() {
            self.init_col_ids32();
            self.init_offsets32();
        } else {
            self.init_col_ids();
            self.init_offsets();
        }
        for i in 0..self.temp_col_ids.len() {
            let col_id = self.temp_col_ids[i];
            if self.values[i].is_null() {
                if self.large() {
                    self.col_ids32[null_idx] = col_id as u32;
                } else {
                    self.col_ids[null_idx] = col_id as u8;
                }
                null_idx += 1;
            } else {
                if self.large() {
                    self.col_ids32[not_null_idx] = col_id as u32;
                } else {
                    self.col_ids[not_null_idx] = col_id as u8;
                }
                self.values[not_null_idx] = self.values[i].clone();
                not_null_idx += 1;
            }
        }
        let nn = self.num_not_null_cols as usize;
        if self.large() {
            // largeNotNullSorter / largeNullSorter
            let mut pairs: Vec<(u32, Datum)> = (0..nn)
                .map(|i| (self.col_ids32[i], self.values[i].clone()))
                .collect();
            pairs.sort_by_key(|p| p.0);
            for (i, (id, v)) in pairs.into_iter().enumerate() {
                self.col_ids32[i] = id;
                self.values[i] = v;
            }
            if self.num_null_cols > 0 {
                self.col_ids32[nn..num_cols].sort_unstable();
            }
        } else {
            // smallNotNullSorter / smallNullSorter
            let mut pairs: Vec<(u8, Datum)> = (0..nn)
                .map(|i| (self.col_ids[i], self.values[i].clone()))
                .collect();
            pairs.sort_by_key(|p| p.0);
            for (i, (id, v)) in pairs.into_iter().enumerate() {
                self.col_ids[i] = id;
                self.values[i] = v;
            }
            if self.num_null_cols > 0 {
                self.col_ids[nn..num_cols].sort_unstable();
            }
        }
        (num_cols, not_null_idx)
    }

    // go: Encoder.encodeRowCols L1 [M-ERR: the multierr group is the first error]
    fn encode_row_cols(
        &mut self,
        loc: Option<&Arc<Location>>,
        num_cols: usize,
        not_null_idx: usize,
    ) -> Result<(), Error> {
        let mut errs: Option<Error> = None;
        for i in 0..not_null_idx {
            let d = self.values[i].clone();
            let data = std::mem::take(&mut self.data);
            match encode_value_datum(loc, &d, data) {
                (data, Ok(())) => self.data = data,
                (data, Err(err)) => {
                    self.data = data;
                    errs.get_or_insert(err);
                }
            }
            // handle convert to large
            if self.data.len() > u16::MAX as usize && !self.large() {
                self.init_col_ids32();
                for j in 0..num_cols {
                    self.col_ids32[j] = u32::from(self.col_ids[j]);
                }
                self.init_offsets32();
                for j in 0..=i {
                    self.offsets32[j] = u32::from(self.offsets[j]);
                }
                self.flags |= rowFlagLarge;
            }
            if self.large() {
                self.offsets32[i] = self.data.len() as u32;
            } else {
                self.offsets[i] = self.data.len() as u16;
            }
        }
        match errs {
            None => Ok(()),
            Some(err) => Err(err),
        }
    }

    // go: row.initColIDs L1
    fn init_col_ids(&mut self) {
        let num_cols = (self.num_not_null_cols + self.num_null_cols) as usize;
        self.col_ids.resize(num_cols, 0);
    }

    // go: row.initColIDs32 L1
    fn init_col_ids32(&mut self) {
        let num_cols = (self.num_not_null_cols + self.num_null_cols) as usize;
        self.col_ids32.resize(num_cols, 0);
    }

    // go: row.initOffsets L1
    fn init_offsets(&mut self) {
        self.offsets.resize(self.num_not_null_cols as usize, 0);
    }

    // go: row.initOffsets32 L1
    fn init_offsets32(&mut self) {
        self.offsets32.resize(self.num_not_null_cols as usize, 0);
    }

    // go: row.toBytes L1
    fn to_bytes(&self, mut buf: Vec<u8>) -> Vec<u8> {
        buf.push(CodecVer);
        buf.push(self.flags);
        buf.push(self.num_not_null_cols as u8);
        buf.push((self.num_not_null_cols >> 8) as u8);
        buf.push(self.num_null_cols as u8);
        buf.push((self.num_null_cols >> 8) as u8);
        if self.large() {
            buf.extend_from_slice(&u32_slice_to_bytes(&self.col_ids32));
            buf.extend_from_slice(&u32_slice_to_bytes(&self.offsets32));
        } else {
            buf.extend_from_slice(&self.col_ids);
            buf.extend_from_slice(&u16_slice_to_bytes(&self.offsets));
        }
        buf.extend_from_slice(&self.data);
        buf
    }

    // go: NoChecksum.encode L1
    fn encode_no_checksum(&mut self, buf: Vec<u8>) -> Vec<u8> {
        self.flags &= !rowFlagChecksum; // revert checksum flag
        self.to_bytes(buf)
    }

    // go: RawChecksum.encode L1
    fn encode_raw_checksum(&mut self, handle: &Handle, buf: Vec<u8>) -> Vec<u8> {
        self.flags |= rowFlagChecksum;
        self.checksum_header &= !checksumFlagExtra; // revert extra checksum flag
        self.checksum_header &= !checksumMaskVersion; // revert checksum version
        self.checksum_header |= checksumVersionRawHandle; // set checksum version
        let mut value_bytes = self.to_bytes(buf);
        value_bytes.push(self.checksum_header);
        self.checksum1 = crc32_update(0, &value_bytes);
        self.checksum1 = crc32_update(self.checksum1, &handle.encoded());
        value_bytes.extend_from_slice(&self.checksum1.to_le_bytes());
        value_bytes
    }
}

/// encodeValueDatum encodes one row datum entry into bytes.
/// due to encode as value, this method will flatten value type like tablecodec.flatten
// go: encodeValueDatum L1 [M-ERRVAL]
pub fn encode_value_datum(
    loc: Option<&Arc<Location>>,
    d: &Datum,
    mut buffer: Vec<u8>,
) -> (Vec<u8>, Result<(), Error>) {
    let err: Result<(), Error> = (|| {
        match d.kind() {
            KindInt64 => encode_int(&mut buffer, d.get_int64()),
            KindUint64 => encode_uint(&mut buffer, d.get_uint64()),
            KindString | KindBytes => buffer.extend_from_slice(d.get_bytes()),
            KindMysqlTime => {
                // for mysql datetime, timestamp and date type
                let mut t = d.get_mysql_time();
                if t.tp() == mysql::TypeTimestamp
                    && let Some(loc) = loc
                    && !Arc::ptr_eq(loc, &utc())
                {
                    t.convert_time_zone(loc, &utc())?;
                }
                let v = t.to_packed_uint()?;
                encode_uint(&mut buffer, v);
            }
            KindMysqlDuration => encode_int(&mut buffer, d.get_mysql_duration().duration),
            KindMysqlEnum => encode_uint(&mut buffer, d.get_mysql_enum().value),
            KindMysqlSet => encode_uint(&mut buffer, d.get_mysql_set().value),
            KindBinaryLiteral | KindMysqlBit => {
                // We don't need to handle errors here since the literal is ensured to be able to store in uint64 in convertToMysqlBit.
                let (val, err) = d.get_binary_literal().to_int(&StrictContext);
                err?;
                encode_uint(&mut buffer, val);
            }
            KindFloat32 | KindFloat64 => tidb_codec::encode_float(&mut buffer, d.get_float64()),
            KindMysqlDecimal => {
                tidb_codec::encode_decimal(
                    &mut buffer,
                    d.get_mysql_decimal(),
                    d.length(),
                    d.frac(),
                )?;
            }
            KindMysqlJSON => {
                let j = d.get_mysql_json();
                buffer.push(j.type_code);
                buffer.extend_from_slice(&j.value);
            }
            KindVectorFloat32 => {
                let v = d.get_vector_float32();
                buffer = v.serialize_to(std::mem::take(&mut buffer));
            }
            k => {
                return Err(errors::errorf(
                    b"unsupport encode type %d",
                    &[Arg::from(i64::from(k))],
                ));
            }
        }
        Ok(())
    })();
    (buffer, err)
}

// go: encodeInt L1
fn encode_int(buf: &mut Vec<u8>, i_val: i64) {
    if i64::from(i_val as i8) == i_val {
        buf.push(i_val as u8);
    } else if i64::from(i_val as i16) == i_val {
        buf.extend_from_slice(&(i_val as u16).to_le_bytes());
    } else if i64::from(i_val as i32) == i_val {
        buf.extend_from_slice(&(i_val as u32).to_le_bytes());
    } else {
        buf.extend_from_slice(&(i_val as u64).to_le_bytes());
    }
}

// go: encodeUint L1
fn encode_uint(buf: &mut Vec<u8>, u_val: u64) {
    if u64::from(u_val as u8) == u_val {
        buf.push(u_val as u8);
    } else if u64::from(u_val as u16) == u_val {
        buf.extend_from_slice(&(u_val as u16).to_le_bytes());
    } else if u64::from(u_val as u32) == u_val {
        buf.extend_from_slice(&(u_val as u32).to_le_bytes());
    } else {
        buf.extend_from_slice(&u_val.to_le_bytes());
    }
}

/// Go reinterprets the slice memory: native byte order, little-endian on both targets.
// go: u16SliceToBytes L2 (byte copy instead of a reinterpreted slice header)
fn u16_slice_to_bytes(u16s: &[u16]) -> Vec<u8> {
    u16s.iter().flat_map(|v| v.to_le_bytes()).collect()
}

// go: u32SliceToBytes L2 (byte copy instead of a reinterpreted slice header)
fn u32_slice_to_bytes(u32s: &[u32]) -> Vec<u8> {
    u32s.iter().flat_map(|v| v.to_le_bytes()).collect()
}
