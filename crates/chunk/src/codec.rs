//! go-file: pkg/util/chunk/codec.go
//!
//! Chunk encoding and decoding (N5a 3.1: L1; ADR 0007 point 1: the chunk codec is ported
//! from Go, TiKV has no decoder). The byte format is Go's; [`Codec::decode`] slices the
//! input [`Bytes`] without copying (Go: sub-slices of the gRPC buffer, N5a 3.4), and the
//! offsets adjustment of [`Decoder::reuse_interm_chk`] copies the offsets of that column
//! (Go rewrites the shared buffer in place; N5a 3.4 item 4).

// Go names for package-level constants (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use std::sync::Arc;

use bytes::Bytes;
use tidb_base::mysql;
use tidb_parser::types::FieldType;
use tidb_types::MY_DECIMAL_STRUCT_SIZE;

use crate::chunk::Chunk;
use crate::chunk::col_mut;
use crate::column::Column;
use crate::column::sizeTime;
use crate::goslice::ColAlloc;
use crate::goslice::GoSlice;

/// Codec is used to:
/// 1. encode a Chunk to a byte slice.
/// 2. decode a Chunk from a byte slice.
#[derive(Clone, Debug)]
pub struct Codec {
    /// colTypes is used to check whether a Column is fixed sized and what the
    /// fixed size for every element.
    /// NOTE: It's only used for decoding.
    col_types: Vec<FieldType>,
}

/// NewCodec creates a new Codec object for encode or decode a Chunk.
// go: NewCodec L1
pub fn new_codec(col_types: Vec<FieldType>) -> Codec {
    Codec { col_types }
}

impl Codec {
    /// Encode encodes a Chunk to a byte slice.
    // go: Codec.Encode L1
    pub fn encode(&self, chk: &Chunk) -> Vec<u8> {
        let mut buffer = Vec::with_capacity(chk.memory_usage().max(0) as usize);
        for col in chk.cols() {
            Self::encode_column(&mut buffer, col);
        }
        buffer
    }

    // go: Codec.encodeColumn L1
    fn encode_column(buffer: &mut Vec<u8>, col: &Column) {
        // encode length.
        buffer.extend_from_slice(&(col.length as u32).to_le_bytes());

        // encode nullCount.
        buffer.extend_from_slice(&(col.null_count() as u32).to_le_bytes());

        // encode nullBitmap.
        if col.null_count() > 0 {
            let num_null_bitmap_bytes = col.length.div_ceil(8);
            buffer.extend_from_slice(&col.null_bitmap.capb()[..num_null_bitmap_bytes]);
        }

        // encode offsets.
        if !col.is_fixed() {
            let num_offset_bytes = (col.length + 1) * 8;
            let offset_bytes = i64_slice_to_bytes(&col.offsets);
            buffer.extend_from_slice(&offset_bytes[..num_offset_bytes]);
        }

        // encode data.
        buffer.extend_from_slice(col.data.bytes());
    }

    /// Decode decodes a Chunk from a byte slice, return the remained unused bytes.
    // go: Codec.Decode L1
    pub fn decode(&self, mut buffer: Bytes) -> (Chunk, Bytes) {
        let mut chk = Chunk::default();
        let mut ordinal = 0;
        while !buffer.is_empty() {
            let mut col = Column::zero(ColAlloc::Global);
            buffer = self.decode_column(buffer, &mut col, ordinal);
            chk.columns.get_or_insert_with(Vec::new).push(Arc::new(col));
            ordinal += 1;
        }
        (chk, buffer)
    }

    /// DecodeToChunk decodes a Chunk from a byte slice, return the remained unused bytes.
    // go: Codec.DecodeToChunk L1
    pub fn decode_to_chunk(&self, mut buffer: Bytes, chk: &mut Chunk) -> Bytes {
        for i in 0..chk.num_cols() {
            let col = col_mut(&mut chk.cols_mut()[i]);
            buffer = self.decode_column(buffer, col, i);
        }
        buffer
    }

    /// decodeColumn decodes a Column from a byte slice, return the remained unused bytes.
    // go: Codec.decodeColumn L2 (M-POOL: no avoidReusing; the buffers are shared slices, N5a 3.4)
    fn decode_column(&self, mut buffer: Bytes, col: &mut Column, ordinal: usize) -> Bytes {
        // Todo(Shenghui Wu): Optimize all data is null.
        // decode length.
        col.length = u32::from_le_bytes(buffer[..4].try_into().unwrap()) as usize;
        buffer = buffer.slice(4..);

        // decode nullCount.
        let null_count = u32::from_le_bytes(buffer[..4].try_into().unwrap()) as usize;
        buffer = buffer.slice(4..);

        // decode nullBitmap.
        if null_count > 0 {
            let num_null_bitmap_bytes = col.length.div_ceil(8);
            col.null_bitmap = GoSlice::shared(buffer.slice(..num_null_bitmap_bytes));
            buffer = buffer.slice(num_null_bitmap_bytes..);
        } else {
            self.set_all_not_null(col);
        }

        // decode offsets.
        let num_fixed_bytes = get_fixed_len(&self.col_types[ordinal]);
        let mut num_data_bytes = num_fixed_bytes * col.length as isize;
        if num_fixed_bytes == -1 {
            let num_offset_bytes = (col.length + 1) * 8;
            col.offsets = bytes_to_i64_slice(buffer.slice(..num_offset_bytes));
            buffer = buffer.slice(num_offset_bytes..);
            num_data_bytes = col.offsets.i64_at(col.length) as isize;
        } else if col.elem_buf.map_or(0, |e| e_cap(&e)) < num_fixed_bytes as usize {
            col.elem_buf = Some(make_elem_buf(num_fixed_bytes as usize));
        }

        // decode data.
        let n = num_data_bytes as usize;
        col.data = GoSlice::shared(buffer.slice(..n));
        // The column reference the data of the grpc response, the memory of the grpc message
        // cannot be GCed if we reuse this column (Go sets avoidReusing; no pool here, M-POOL).
        buffer.slice(n..)
    }

    // go: Codec.setAllNotNull L1
    fn set_all_not_null(&self, col: &mut Column) {
        let num_null_bitmap_bytes = col.length.div_ceil(8);
        col.null_bitmap.reslice(0);
        let mut i = 0;
        while i < num_null_bitmap_bytes {
            let num_append_bytes = (num_null_bitmap_bytes - i).min(allNotNullBitmap.len());
            col.null_bitmap.append(&allNotNullBitmap[..num_append_bytes]);
            i += num_append_bytes;
        }
    }
}

/// The capacity of an `elemBuf`.
// glue: Go cap(col.elemBuf)
fn e_cap(e: &crate::column::ElemBuf) -> usize {
    e.cap()
}

/// `make([]byte, n)` for an `elemBuf`.
// glue: Go make([]byte, n)
fn make_elem_buf(n: usize) -> crate::column::ElemBuf {
    crate::column::ElemBuf::new_zeroed(n)
}

/// The offsets as their little-endian bytes (Go reinterprets the `[]int64`).
// go: i64SliceToBytes L2 (the offsets are stored little-endian, so the bytes are the storage)
fn i64_slice_to_bytes(i64s: &GoSlice<8>) -> &[u8] {
    i64s.bytes()
}

/// `allNotNullBitmap` is 128 bytes of 0xFF (Go fills it in `init`).
static allNotNullBitmap: [u8; 128] = [0xFF; 128];

/// The offsets of a decoded column, a shared slice of the input (Go reinterprets the bytes).
// go: bytesToI64Slice L2 (a shared slice with little-endian reads instead of a slice header cast)
fn bytes_to_i64_slice(b: Bytes) -> GoSlice<8> {
    GoSlice::shared(b)
}

/// VarElemLen indicates this Column is a variable length Column.
pub const VarElemLen: isize = -1;

// go: getFixedLen L1
pub(crate) fn get_fixed_len(col_type: &FieldType) -> isize {
    match col_type.get_type() {
        mysql::TypeFloat => 4,
        mysql::TypeTiny
        | mysql::TypeShort
        | mysql::TypeInt24
        | mysql::TypeLong
        | mysql::TypeLonglong
        | mysql::TypeDouble
        | mysql::TypeYear
        | mysql::TypeDuration => 8,
        mysql::TypeDate | mysql::TypeDatetime | mysql::TypeTimestamp => sizeTime as isize,
        mysql::TypeNewDecimal => MY_DECIMAL_STRUCT_SIZE as isize,
        _ => VarElemLen,
    }
}

/// GetFixedLen get the memory size of a fixed-length type.
/// if colType is not fixed-length, it returns VarElemLen, aka -1.
// go: GetFixedLen L1
pub fn get_fixed_len_pub(col_type: &FieldType) -> isize {
    get_fixed_len(col_type)
}

/// EstimateTypeWidth estimates the average width of values of the type.
/// This is used by the planner, which doesn't require absolutely correct results;
/// it's OK (and expected) to guess if we don't know for sure.
///
/// mostly study from https://github.com/postgres/postgres/blob/REL_12_STABLE/src/backend/utils/cache/lsyscache.c#L2356
// go: EstimateTypeWidth L1
pub fn estimate_type_width(col_type: &FieldType) -> isize {
    let col_len = get_fixed_len(col_type);
    // Easy if it's a fixed-width type
    if col_len != VarElemLen {
        return col_len;
    }

    let col_len = col_type.get_flen();
    if col_len > 0 {
        if col_len <= 32 {
            return col_len;
        }
        if col_len < 1000 {
            return 32 + (col_len - 32) / 2; // assume 50%
        }
        // Beyond 1000, assume we're looking at something like
        // "varchar(10000)" where the limit isn't actually reached often, and
        // use a fixed estimate.
        return 32 + (1000 - 32) / 2;
    }
    // Oops, we have no idea ... wild guess time.
    32
}

/// Go's `init` fills `allNotNullBitmap` with 0xFF; the static is initialised so.
// go: init L2 (the table is a constant)
#[allow(dead_code)]
fn init() {}

/// Decoder decodes the data returned from the coprocessor and stores the result in Chunk.
/// How Decoder works:
///  1. Initialization phase: Decode a whole input byte slice to Decoder.intermChk(intermediate chunk) using Codec.Decode.
///     intermChk is introduced to simplify the implementation of decode phase. This phase uses pointer operations with
///     less CPU and memory cost.
///  2. Decode phase:
///     2.1 Set the number of rows to be decoded to a value that is a multiple of 8 and greater than
///     `chk.RequiredRows() - chk.NumRows()`. This reduces the overhead of copying the srcCol.nullBitMap into
///     destCol.nullBitMap.
///     2.2 Append srcCol.offsets to destCol.offsets when the elements is of var-length type. And further adjust the
///     offsets according to descCol.offsets[destCol.length]-srcCol.offsets[0].
///     2.3 Append srcCol.nullBitMap to destCol.nullBitMap.
///  3. Go to step 1 when the input byte slice is consumed.
#[derive(Debug)]
pub struct Decoder {
    interm_chk: Chunk,
    codec: Codec,
    remained_rows: usize,
}

/// NewDecoder creates a new Decoder object for decode a Chunk.
// go: NewDecoder L1
pub fn new_decoder(chk: Chunk, col_types: Vec<FieldType>) -> Decoder {
    Decoder {
        interm_chk: chk,
        codec: new_codec(col_types),
        remained_rows: 0,
    }
}

impl Decoder {
    /// Decode decodes multiple rows of Decoder.intermChk and stores the result in chk.
    // go: Decoder.Decode L1
    pub fn decode(&mut self, chk: &mut Chunk) {
        let mut required_rows = chk.required_rows() as isize - chk.num_rows() as isize;
        // Set the requiredRows to a multiple of 8.
        required_rows = (required_rows + 7) >> 3 << 3;
        let mut required_rows = required_rows.max(0) as usize;
        if required_rows > self.remained_rows {
            required_rows = self.remained_rows;
        }
        for i in 0..chk.num_cols() {
            self.decode_column(chk, i, required_rows);
        }
        self.remained_rows -= required_rows;
    }

    /// Reset decodes data and store the result in Decoder.intermChk. This decode phase uses pointer operations with less
    /// CPU and memory costs.
    // go: Decoder.Reset L1
    pub fn reset(&mut self, data: Bytes) {
        self.codec.decode_to_chunk(data, &mut self.interm_chk);
        self.remained_rows = self.interm_chk.num_rows();
    }

    /// IsFinished indicates whether Decoder.intermChk has been dried up.
    // go: Decoder.IsFinished L1
    pub fn is_finished(&self) -> bool {
        self.remained_rows == 0
    }

    /// RemainedRows indicates Decoder.intermChk has remained rows.
    // go: Decoder.RemainedRows L1
    pub fn remained_rows(&self) -> usize {
        self.remained_rows
    }

    /// ReuseIntermChk swaps `Decoder.intermChk` with `chk` directly when `Decoder.intermChk.NumRows()` is no less
    /// than `chk.requiredRows * factor` where `factor` is 0.8 now. This can avoid the overhead of appending the
    /// data from `Decoder.intermChk` to `chk`. Moreover, the column.offsets needs to be further adjusted
    /// according to column.offset[0].
    // go: Decoder.ReuseIntermChk L2 (the adjusted offsets of a shared column are copied, N5a 3.4 item 4)
    pub fn reuse_interm_chk(&mut self, chk: &mut Chunk) {
        let remained = self.remained_rows;
        for (i, col) in self.interm_chk.cols_mut().iter_mut().enumerate() {
            let col = col_mut(col);
            col.length = remained;
            let elem_len = get_fixed_len(&self.codec.col_types[i]);
            if elem_len == VarElemLen {
                // For var-length types, we need to adjust the offsets before reuse.
                let delta_offset = col.offsets.i64_at(0);
                if delta_offset != 0 {
                    for j in 0..col.offsets.len() {
                        let v = col.offsets.i64_at(j);
                        col.offsets.set_i64(j, v - delta_offset);
                    }
                }
            }
        }
        chk.swap_columns(&mut self.interm_chk);
        self.remained_rows = 0;
    }

    // go: Decoder.decodeColumn L1
    fn decode_column(&mut self, chk: &mut Chunk, ordinal: usize, required_rows: usize) {
        let elem_len = get_fixed_len(&self.codec.col_types[ordinal]);
        let mut num_data_bytes = elem_len * required_rows as isize;
        let src_col = col_mut(&mut self.interm_chk.cols_mut()[ordinal]);
        let dest_col = col_mut(&mut chk.cols_mut()[ordinal]);

        if elem_len == VarElemLen {
            // For var-length types, we need to adjust the offsets after appending to destCol.
            num_data_bytes = (src_col.offsets.i64_at(required_rows) - src_col.offsets.i64_at(0)) as isize;
            let delta_offset = dest_col.offsets.i64_at(dest_col.length) - src_col.offsets.i64_at(0);
            dest_col
                .offsets
                .append(&src_col.offsets.capb()[8..(required_rows + 1) * 8]);
            for i in dest_col.length + 1..=dest_col.length + required_rows {
                let v = dest_col.offsets.i64_at(i);
                dest_col.offsets.set_i64(i, v + delta_offset);
            }
            src_col.offsets.advance(required_rows);
        }

        let num_null_bitmap_bytes = (required_rows + 7) >> 3;
        if dest_col.length % 8 == 0 {
            dest_col
                .null_bitmap
                .append(&src_col.null_bitmap.capb()[..num_null_bitmap_bytes]);
        } else {
            dest_col.append_multi_same_null_bitmap(false, required_rows);
            let bit_map_len = dest_col.null_bitmap.len();
            // bitOffset indicates the number of valid bits in destCol.nullBitmap's last byte.
            let bit_offset = (dest_col.length % 8) as u32;
            let start_idx = (dest_col.length - 1) >> 3;
            let src = src_col.null_bitmap.bytes();
            let dst = dest_col.null_bitmap.bytes_mut();
            for i in 0..num_null_bitmap_bytes {
                dst[start_idx + i] |= src[i] << bit_offset;
                // The high order 8-bitOffset bits in `srcCol.nullBitmap[i]` should be appended to the low order of the next slot.
                if start_idx + i + 1 < bit_map_len {
                    dst[start_idx + i + 1] |= src[i] >> (8 - bit_offset);
                }
            }
        }
        // Set all the redundant bits in the last slot of destCol.nullBitmap to 0.
        let num_redundant_bits = (dest_col.null_bitmap.len() * 8 - dest_col.length - required_rows) as u32;
        let bit_mask = ((1u32 << (8 - num_redundant_bits)) as u8).wrapping_sub(1);
        let last = dest_col.null_bitmap.len() - 1;
        *dest_col.null_bitmap.at_mut(last) &= bit_mask;

        src_col.null_bitmap.advance(num_null_bitmap_bytes);
        dest_col.length += required_rows;

        let n = num_data_bytes as usize;
        dest_col.data.append(&src_col.data.capb()[..n]);
        src_col.data.advance(n);
    }
}
