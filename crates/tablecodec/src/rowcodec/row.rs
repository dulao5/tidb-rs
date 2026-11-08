//! go-file: pkg/util/rowcodec/row.go
//!
//! The row format v2 reader.
//!
//! Mapping:
//! - Go's `row` holds slices into the row bytes and is embedded (and reused) in the
//!   decoders; the port is a borrowed view [`Row`] built per call by [`Row::from_bytes`].
//! - Go slices may be re-sliced up to their capacity: `r.data` is `rowData[cursor :
//!   cursor+lastOffset]`, so `getData` can read past `lastOffset` up to the end of `rowData`
//!   (a corrupted offset reads the checksum bytes rather than panicking). The view keeps the
//!   bytes up to the end of `rowData` (`data_cap`) and slices them as Go does (M-SLICECAP is
//!   not needed: the bound is the input's length, its capacity in every Go caller).
//! - The writer side (`toBytes`, `initColIDs*`, `initOffsets*`) belongs to the encoder (X);
//!   `ColumnIsNull` (unistore) and `CalculateRawChecksum` (TiCDC) have no read-path caller (X).

// Flags keep their Go names (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use tidb_base::Error;

use super::common::{
    CodecVer, U16Slice, U32Slice, bytes_to_u32_slice, bytes2_u16_slice, errInvalidChecksumVer,
    errInvalidCodecVer,
};

pub(crate) const rowFlagLarge: u8 = 1 << 0;
pub(crate) const rowFlagChecksum: u8 = 1 << 1;

pub(crate) const checksumMaskVersion: u8 = 0b0111;
pub(crate) const checksumFlagExtra: u8 = 0b1000;

/// row is the struct type used to access a row and the row format is shown as the following.
///
/// Row Format
///
/// ```text
///  0               1               2               3
///  +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
///  |      VER      |     FLAGS     |        NOT_NULL_COL_CNT       |
///  +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
///  |          NULL_COL_CNT         |     ...NOT_NULL_COL_IDS...    |
///  +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
///  |       ...NULL_COL_IDS...      |   ...NOT_NULL_COL_OFFSETS...  |
///  +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
///  |                    ...NOT_NULL_COL_DATA...                    |
///  +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
///  |         ...CHECKSUM...        |
///  +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
///
///  - FLAGS
///    - 0x01: large (when max(col_ids) > 255 or len(col_data) > max_u16)
///      - size of col_id     = large ? 4 : 1
///      - size of col_offset = large ? 4 : 2
///    - 0x02: has checksum
/// ```
///
/// Checksum
///
/// ```text
///  0               1               2               3               4               5               6               7               8
///  +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
///  |       |E| VER |                            CHECKSUM                           |                    EXTRA_CHECKSUM(OPTIONAL)                   |
///  +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
///       HEADER
///
///  - HEADER
///    - VER: version
///    - E:   has extra checksum
///  - CHECKSUM
///    - little-endian CRC32(IEEE) when hdr.ver = 0 (old version, columns-level checksum)
///    - little-endian CRC32(IEEE) when hdr.ver = 1 (default, bytes-level checksum)
/// ```
#[derive(Clone, Copy, Debug, Default)]
pub struct Row<'a> {
    flags: u8,
    checksum_header: u8,
    num_not_null_cols: u16,
    num_null_cols: u16,

    // for small row: colID []byte, offsets []uint16, optimized for most cases.
    col_ids: &'a [u8],
    offsets: U16Slice<'a>,

    // for large row: colID []uint32, offsets []uint32.
    col_ids32: U32Slice<'a>,
    offsets32: U32Slice<'a>,

    /// `rowData[cursor:]`: Go's `data` slice up to its capacity.
    data_cap: &'a [u8],
    /// `len(data)` (the last offset).
    data_len: usize,
    checksum1: u32,
    checksum2: u32,
}

/// `binary.LittleEndian.Uint16(b)` (panics on fewer than 2 bytes, as Go).
// go-std: encoding/binary.littleEndian.Uint16 L1
fn le_u16(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}

/// `binary.LittleEndian.Uint32(b)` (panics on fewer than 4 bytes, as Go).
// go-std: encoding/binary.littleEndian.Uint32 L1
fn le_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

impl<'a> Row<'a> {
    // go: row.large L1
    pub(crate) fn large(&self) -> bool {
        self.flags & rowFlagLarge > 0
    }

    // go: row.hasChecksum L1
    pub(crate) fn has_checksum(&self) -> bool {
        self.flags & rowFlagChecksum > 0
    }

    // go: row.hasExtraChecksum L1
    pub(crate) fn has_extra_checksum(&self) -> bool {
        self.checksum_header & checksumFlagExtra > 0
    }

    // go: row.getOffsets L1
    pub(crate) fn get_offsets(&self, i: usize) -> (u32, u32) {
        let mut start = 0u32;
        let end = if self.large() {
            if i > 0 {
                start = self.offsets32.get(i - 1);
            }
            self.offsets32.get(i)
        } else {
            if i > 0 {
                start = self.offsets.get(i - 1) as u32;
            }
            self.offsets.get(i) as u32
        };
        (start, end)
    }

    // go: row.getData L1
    pub fn get_data(&self, i: usize) -> &'a [u8] {
        let (start, end) = self.get_offsets(i);
        let (start, end) = (start as usize, end as usize);
        assert!(
            start <= end && end <= self.data_cap.len(),
            "slice bounds out of range [{start}:{end}] with capacity {}",
            self.data_cap.len()
        );
        &self.data_cap[start..end]
    }

    /// `r.fromBytes(rowData)`: like Go, the fields parsed before an error stay set (the
    /// decoders' `GetChecksum` reads them after a failed decode).
    // go: row.fromBytes L1 [M-ERRVAL]
    pub fn from_bytes(&mut self, row_data: &'a [u8]) -> Result<(), Error> {
        let r = self;
        if row_data[0] != CodecVer {
            return Err(errInvalidCodecVer.clone());
        }
        r.flags = row_data[1];
        r.num_not_null_cols = le_u16(&row_data[2..]);
        r.num_null_cols = le_u16(&row_data[4..]);
        let mut cursor = 6usize;
        let mut last_offset = 0usize;
        if r.large() {
            // Go: int(r.numNotNullCols + r.numNullCols), a uint16 sum.
            let col_ids_len = r.num_not_null_cols.wrapping_add(r.num_null_cols) as usize * 4;
            r.col_ids32 = bytes_to_u32_slice(&row_data[cursor..cursor + col_ids_len]);
            cursor += col_ids_len;
            let offsets_len = r.num_not_null_cols as usize * 4;
            r.offsets32 = bytes_to_u32_slice(&row_data[cursor..cursor + offsets_len]);
            cursor += offsets_len;
            let n = r.offsets32.len();
            if n > 0 {
                last_offset = r.offsets32.get(n - 1) as usize;
            }
        } else {
            // Go: int(r.numNotNullCols + r.numNullCols), a uint16 sum.
            let col_ids_len = r.num_not_null_cols.wrapping_add(r.num_null_cols) as usize;
            r.col_ids = &row_data[cursor..cursor + col_ids_len];
            cursor += col_ids_len;
            let offsets_len = r.num_not_null_cols as usize * 2;
            r.offsets = bytes2_u16_slice(&row_data[cursor..cursor + offsets_len]);
            cursor += offsets_len;
            let n = r.offsets.len();
            if n > 0 {
                last_offset = r.offsets.get(n - 1) as usize;
            }
        }
        let _ = &row_data[cursor..cursor + last_offset];
        r.data_cap = &row_data[cursor..];
        r.data_len = last_offset;
        cursor += last_offset;

        if r.has_checksum() {
            r.checksum_header = row_data[cursor];
            let checksum_version = r.checksum_version();
            // make sure it can be read previous version checksum to support backward compatibility.
            match checksum_version {
                0..=2 => {}
                _ => return Err(errInvalidChecksumVer.clone()),
            }
            cursor += 1;
            r.checksum1 = le_u32(&row_data[cursor..]);
            if r.has_extra_checksum() {
                cursor += 4;
                r.checksum2 = le_u32(&row_data[cursor..]);
            }
        } else {
            r.checksum_header = 0;
            r.checksum1 = 0;
            r.checksum2 = 0;
        }
        Ok(())
    }

    /// Returns `(idx, isNil, notFound)`.
    // go: row.findColID L1
    pub fn find_col_id(&self, col_id: i64) -> (usize, bool, bool) {
        // Search the column in not-null columns array.
        let (mut i, mut j) = (0usize, self.num_not_null_cols as usize);
        while i < j {
            let h = (i + j) >> 1; // avoid overflow when computing h
            // i ≤ h < j
            let v = if self.large() {
                self.col_ids32.get(h) as i64
            } else {
                self.col_ids[h] as i64
            };
            if v < col_id {
                i = h + 1;
            } else if v == col_id {
                return (h, false, false);
            } else {
                j = h;
            }
        }

        // Search the column in null columns array.
        (i, j) = (
            self.num_not_null_cols as usize,
            // Go: int(r.numNotNullCols + r.numNullCols), a uint16 sum.
            self.num_not_null_cols.wrapping_add(self.num_null_cols) as usize,
        );
        while i < j {
            let h = (i + j) >> 1; // avoid overflow when computing h
            // i ≤ h < j
            let v = if self.large() {
                self.col_ids32.get(h) as i64
            } else {
                self.col_ids[h] as i64
            };
            if v < col_id {
                i = h + 1;
            } else if v == col_id {
                return (0, true, false);
            } else {
                j = h;
            }
        }
        (0, false, true)
    }

    /// The number of column IDs of the row (not-null and null), Go's
    /// `int(r.numNotNullCols + r.numNullCols)`.
    // glue: column count of the row header
    pub fn num_cols(&self) -> usize {
        self.num_not_null_cols.wrapping_add(self.num_null_cols) as usize
    }

    /// ChecksumVersion returns the version of checksum. Note that it's valid only if checksum has been encoded in the row
    /// value (callers can check it by `GetChecksum`).
    // go: row.ChecksumVersion L1
    pub fn checksum_version(&self) -> isize {
        (self.checksum_header & checksumMaskVersion) as isize
    }

    /// GetChecksum returns the checksum of row data (not null columns).
    // go: row.GetChecksum L1
    pub fn get_checksum(&self) -> (u32, bool) {
        if !self.has_checksum() {
            return (0, false);
        }
        (self.checksum1, true)
    }

    /// GetExtraChecksum returns the extra checksum which shall be calculated in the last stable schema version (whose
    /// elements are all public).
    // go: row.GetExtraChecksum L1
    pub fn get_extra_checksum(&self) -> (u32, bool) {
        if !self.has_extra_checksum() {
            return (0, false);
        }
        (self.checksum2, true)
    }
}
