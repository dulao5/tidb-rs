//! go-file: pkg/util/codec/bytes.go
//!
//! Memcomparable byte strings (8-byte groups with a marker byte) and compact (length-prefixed)
//! byte strings.
//!
//! Mapping: `reallocBytes` reserves exactly the extra capacity Go allocates (capacity is not
//! observable, it only saves reallocations). Go's `decodeBytes(b, buf, ...)` reuses `buf`'s
//! storage and returns it; the port takes `buf: Option<Vec<u8>>` (`None` is Go's `nil`, which
//! allocates `len(b)`) and returns the filled vector. `fastReverseBytes` complements machine
//! words through an unsafe slice cast; the port complements bytes and leaves vectorisation to
//! the compiler (same result).

// Constants keep their Go names (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use tidb_base::Error;
use tidb_base::errors;
use tidb_base::gofmt::Arg;

use crate::gobinary::MaxVarintLen64;
use crate::number::{decode_varint, encode_varint, err_insufficient};

pub(crate) const encGroupSize: usize = 8;
pub(crate) const encMarker: u8 = 0xFF;
const encPad: u8 = 0x0;

const pads: [u8; encGroupSize] = [0; encGroupSize];

/// EncodeBytes guarantees the encoded value is in ascending order for comparison,
/// encoding with the following rule:
///
/// ```text
///  [group1][marker1]...[groupN][markerN]
///  group is 8 bytes slice which is padding with 0.
///  marker is `0xFF - padding 0 count`
/// ```
///
/// For example:
///
/// ```text
///  [] -> [0, 0, 0, 0, 0, 0, 0, 0, 247]
///  [1, 2, 3] -> [1, 2, 3, 0, 0, 0, 0, 0, 250]
///  [1, 2, 3, 0] -> [1, 2, 3, 0, 0, 0, 0, 0, 251]
///  [1, 2, 3, 4, 5, 6, 7, 8] -> [1, 2, 3, 4, 5, 6, 7, 8, 255, 0, 0, 0, 0, 0, 0, 0, 0, 247]
/// ```
///
/// Refer: https://github.com/facebook/mysql-5.6/wiki/MyRocks-record-format#memcomparable-format
// go: EncodeBytes L1
pub fn encode_bytes(result: &mut Vec<u8>, data: &[u8]) {
    // Allocate more space to avoid unnecessary slice growing.
    // Assume that the byte slice size is about `(len(data) / encGroupSize + 1) * (encGroupSize + 1)` bytes,
    // that is `(len(data) / 8 + 1) * 9` in our implement.
    let d_len = data.len();
    let realloc_size = (d_len / encGroupSize + 1) * (encGroupSize + 1);
    realloc_bytes(result, realloc_size);
    let mut idx = 0;
    while idx <= d_len {
        let remain = d_len - idx;
        let mut pad_count = 0;
        if remain >= encGroupSize {
            result.extend_from_slice(&data[idx..idx + encGroupSize]);
        } else {
            pad_count = encGroupSize - remain;
            result.extend_from_slice(&data[idx..]);
            result.extend_from_slice(&pads[..pad_count]);
        }

        let marker = encMarker - pad_count as u8;
        result.push(marker);
        idx += encGroupSize;
    }
}

/// EncodeBytesExt is an extension of `EncodeBytes`, which will not encode for `isRawKv = true` but just append `data` to `b`.
// go: EncodeBytesExt L1
pub fn encode_bytes_ext(b: &mut Vec<u8>, data: &[u8], is_raw_kv: bool) {
    if is_raw_kv {
        b.extend_from_slice(data);
        return;
    }
    encode_bytes(b, data);
}

/// EncodedBytesLength returns the length of data after encoded
// go: EncodedBytesLength L1
pub fn encoded_bytes_length(data_len: isize) -> isize {
    let group = encGroupSize as isize;
    let m = data_len % group;
    let pad_count = group - m;
    data_len + pad_count + 1 + data_len / group
}

// go: decodeBytes L1
fn decode_bytes_impl(
    mut b: &[u8],
    buf: Option<Vec<u8>>,
    reverse: bool,
) -> Result<(&[u8], Vec<u8>), Error> {
    let mut buf = match buf {
        Some(buf) => buf,
        None => Vec::with_capacity(b.len()),
    };
    buf.clear();
    loop {
        if b.len() < encGroupSize + 1 {
            return Err(err_insufficient());
        }

        let group_bytes = &b[..encGroupSize + 1];

        let group = &group_bytes[..encGroupSize];
        let marker = group_bytes[encGroupSize];

        let pad_count = if reverse { marker } else { encMarker - marker };
        if pad_count as usize > encGroupSize {
            return Err(errors::errorf(
                "invalid marker byte, group bytes %q",
                &[Arg::Bytes(group_bytes.to_vec())],
            ));
        }

        let real_group_size = encGroupSize - pad_count as usize;
        buf.extend_from_slice(&group[..real_group_size]);
        b = &b[encGroupSize + 1..];

        if pad_count != 0 {
            let pad_byte = if reverse { encMarker } else { encPad };
            // Check validity of padding bytes.
            for &v in &group[real_group_size..] {
                if v != pad_byte {
                    return Err(errors::errorf(
                        "invalid padding byte, group bytes %q",
                        &[Arg::Bytes(group_bytes.to_vec())],
                    ));
                }
            }
            break;
        }
    }
    if reverse {
        reverse_bytes(&mut buf);
    }
    Ok((b, buf))
}

/// DecodeBytes decodes bytes which is encoded by EncodeBytes before,
/// returns the leftover bytes and decoded value if no error.
/// `buf` is used to buffer data to avoid the cost of makeslice in decodeBytes when DecodeBytes is called by Decoder.DecodeOne.
// go: DecodeBytes L1
pub fn decode_bytes(b: &[u8], buf: Option<Vec<u8>>) -> Result<(&[u8], Vec<u8>), Error> {
    decode_bytes_impl(b, buf, false)
}

/// EncodeBytesDesc first encodes bytes using EncodeBytes, then bitwise reverses
/// encoded value to guarantee the encoded value is in descending order for comparison.
// go: EncodeBytesDesc L1
pub fn encode_bytes_desc(b: &mut Vec<u8>, data: &[u8]) {
    let n = b.len();
    encode_bytes(b, data);
    reverse_bytes(&mut b[n..]);
}

/// DecodeBytesDesc decodes bytes which is encoded by EncodeBytesDesc before,
/// returns the leftover bytes and decoded value if no error.
// go: DecodeBytesDesc L1
pub fn decode_bytes_desc(b: &[u8], buf: Option<Vec<u8>>) -> Result<(&[u8], Vec<u8>), Error> {
    decode_bytes_impl(b, buf, true)
}

/// EncodeCompactBytes joins bytes with its length into a byte slice. It is more
/// efficient in both space and time compare to EncodeBytes. Note that the encoded
/// result is not memcomparable.
// go: EncodeCompactBytes L1
pub fn encode_compact_bytes(b: &mut Vec<u8>, data: &[u8]) {
    realloc_bytes(b, MaxVarintLen64 + data.len());
    encode_varint(b, data.len() as i64);
    b.extend_from_slice(data);
}

/// DecodeCompactBytes decodes bytes which is encoded by EncodeCompactBytes before.
// go: DecodeCompactBytes L1
pub fn decode_compact_bytes(b: &[u8]) -> Result<(&[u8], &[u8]), Error> {
    let (b, n) = decode_varint(b)?;
    if n < 0 || (b.len() as i64) < n {
        return Err(errors::errorf(
            "insufficient bytes to decode value, expected length: %v",
            &[Arg::from(n)],
        ));
    }
    let n = n as usize;
    Ok((&b[n..], &b[..n]))
}

// go: fastReverseBytes L2 (bytewise complement; Go complements machine words through an unsafe cast, same result)
fn fast_reverse_bytes(b: &mut [u8]) {
    for x in b {
        *x = !*x;
    }
}

// go: safeReverseBytes L1
fn safe_reverse_bytes(b: &mut [u8]) {
    for x in b {
        *x = !*x;
    }
}

/// `supportsUnaligned` (GOARCH 386 / amd64).
const supportsUnaligned: bool = cfg!(any(target_arch = "x86", target_arch = "x86_64"));

// go: reverseBytes L1
pub(crate) fn reverse_bytes(b: &mut [u8]) {
    if supportsUnaligned {
        fast_reverse_bytes(b);
        return;
    }

    safe_reverse_bytes(b);
}

/// reallocBytes is like realloc.
// go: reallocBytes L1
pub(crate) fn realloc_bytes(b: &mut Vec<u8>, n: usize) {
    let new_size = b.len() + n;
    if b.capacity() < new_size {
        b.reserve_exact(n);
    }

    // slice b has capability to store n bytes
}
