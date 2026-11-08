//! go-file: pkg/util/codec/number.go
//!
//! Fixed-length (memcomparable) and variable-length integer encodings.
//!
//! Mapping: Go's `func F(b []byte, v T) []byte` appends to `b` and returns it; the port
//! appends to `b: &mut Vec<u8>` (Go's capacity growth is not observable here). Go's
//! decoders return `(remain []byte, v T, err error)` with `remain == nil` on error; the port
//! returns `Result<(&[u8], T), Error>`. `errors.Trace` is not ported (M-ERR).

// Constants keep their Go names (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use tidb_base::Error;
use tidb_base::errors;

use crate::gobinary;

const signMask: u64 = 0x8000000000000000;

/// EncodeIntToCmpUint make int v to comparable uint type
// go: EncodeIntToCmpUint L1
pub fn encode_int_to_cmp_uint(v: i64) -> u64 {
    (v as u64) ^ signMask
}

/// DecodeCmpUintToInt decodes the u that encoded by EncodeIntToCmpUint
// go: DecodeCmpUintToInt L1
pub fn decode_cmp_uint_to_int(u: u64) -> i64 {
    (u ^ signMask) as i64
}

/// `errors.New("insufficient bytes to decode value")`.
// glue: the error message every decoder of the package returns on a short input
pub(crate) fn err_insufficient() -> Error {
    errors::new("insufficient bytes to decode value")
}

/// EncodeInt appends the encoded value to slice b and returns the appended slice.
/// EncodeInt guarantees that the encoded value is in ascending order for comparison.
// go: EncodeInt L1
pub fn encode_int(b: &mut Vec<u8>, v: i64) {
    let u = encode_int_to_cmp_uint(v);
    b.extend_from_slice(&u.to_be_bytes());
}

/// EncodeIntDesc appends the encoded value to slice b and returns the appended slice.
/// EncodeIntDesc guarantees that the encoded value is in descending order for comparison.
// go: EncodeIntDesc L1
pub fn encode_int_desc(b: &mut Vec<u8>, v: i64) {
    let u = encode_int_to_cmp_uint(v);
    b.extend_from_slice(&(!u).to_be_bytes());
}

/// The first 8 bytes of `b` as a big-endian `uint64`, or the short-input error.
// glue: binary.BigEndian.Uint64(b[:8]) after the length check of the Decode functions
fn be_u64(b: &[u8]) -> Result<u64, Error> {
    match b.first_chunk::<8>() {
        Some(x) => Ok(u64::from_be_bytes(*x)),
        None => Err(err_insufficient()),
    }
}

/// DecodeInt decodes value encoded by EncodeInt before.
/// It returns the leftover un-decoded slice, decoded value if no error.
// go: DecodeInt L1
pub fn decode_int(b: &[u8]) -> Result<(&[u8], i64), Error> {
    let u = be_u64(b)?;
    let v = decode_cmp_uint_to_int(u);
    Ok((&b[8..], v))
}

/// DecodeIntDesc decodes value encoded by EncodeInt before.
/// It returns the leftover un-decoded slice, decoded value if no error.
// go: DecodeIntDesc L1
pub fn decode_int_desc(b: &[u8]) -> Result<(&[u8], i64), Error> {
    let u = be_u64(b)?;
    let v = decode_cmp_uint_to_int(!u);
    Ok((&b[8..], v))
}

/// EncodeUint appends the encoded value to slice b and returns the appended slice.
/// EncodeUint guarantees that the encoded value is in ascending order for comparison.
// go: EncodeUint L1
pub fn encode_uint(b: &mut Vec<u8>, v: u64) {
    b.extend_from_slice(&v.to_be_bytes());
}

/// EncodeUintDesc appends the encoded value to slice b and returns the appended slice.
/// EncodeUintDesc guarantees that the encoded value is in descending order for comparison.
// go: EncodeUintDesc L1
pub fn encode_uint_desc(b: &mut Vec<u8>, v: u64) {
    b.extend_from_slice(&(!v).to_be_bytes());
}

/// DecodeUint decodes value encoded by EncodeUint before.
/// It returns the leftover un-decoded slice, decoded value if no error.
// go: DecodeUint L1
pub fn decode_uint(b: &[u8]) -> Result<(&[u8], u64), Error> {
    let v = be_u64(b)?;
    Ok((&b[8..], v))
}

/// DecodeUintDesc decodes value encoded by EncodeInt before.
/// It returns the leftover un-decoded slice, decoded value if no error.
// go: DecodeUintDesc L1
pub fn decode_uint_desc(b: &[u8]) -> Result<(&[u8], u64), Error> {
    let v = be_u64(b)?;
    Ok((&b[8..], !v))
}

/// EncodeVarint appends the encoded value to slice b and returns the appended slice.
/// Note that the encoded result is not memcomparable.
// go: EncodeVarint L1
pub fn encode_varint(b: &mut Vec<u8>, v: i64) {
    gobinary::put_varint(b, v);
}

/// DecodeVarint decodes value encoded by EncodeVarint before.
/// It returns the leftover un-decoded slice, decoded value if no error.
// go: DecodeVarint L1
pub fn decode_varint(b: &[u8]) -> Result<(&[u8], i64), Error> {
    let (v, n) = gobinary::varint(b);
    if n > 0 {
        return Ok((&b[n as usize..], v));
    }
    if n < 0 {
        return Err(errors::new("value larger than 64 bits"));
    }
    Err(err_insufficient())
}

/// EncodeUvarint appends the encoded value to slice b and returns the appended slice.
/// Note that the encoded result is not memcomparable.
// go: EncodeUvarint L1
pub fn encode_uvarint(b: &mut Vec<u8>, v: u64) {
    gobinary::put_uvarint(b, v);
}

/// DecodeUvarint decodes value encoded by EncodeUvarint before.
/// It returns the leftover un-decoded slice, decoded value if no error.
// go: DecodeUvarint L1
pub fn decode_uvarint(b: &[u8]) -> Result<(&[u8], u64), Error> {
    let (v, n) = gobinary::uvarint(b);
    if n > 0 {
        return Ok((&b[n as usize..], v));
    }
    if n < 0 {
        return Err(errors::new("value larger than 64 bits"));
    }
    Err(err_insufficient())
}

const negativeTagEnd: u8 = 8; // negative tag is (negativeTagEnd - length).
const positiveTagStart: u8 = 0xff - 8; // Positive tag is (positiveTagStart + length).

/// EncodeComparableVarint encodes an int64 to a mem-comparable bytes.
// go: EncodeComparableVarint L1
pub fn encode_comparable_varint(b: &mut Vec<u8>, v: i64) {
    if v < 0 {
        // All negative value has a tag byte prefix (negativeTagEnd - length).
        // Smaller negative value encodes to more bytes, has smaller tag.
        let length = if v >= -0xff {
            1
        } else if v >= -0xffff {
            2
        } else if v >= -0xffffff {
            3
        } else if v >= -0xffffffff {
            4
        } else if v >= -0xffffffffff {
            5
        } else if v >= -0xffffffffffff {
            6
        } else if v >= -0xffffffffffffff {
            7
        } else {
            8
        };
        b.push(negativeTagEnd - length);
        b.extend_from_slice(&v.to_be_bytes()[8 - length as usize..]);
        return;
    }
    encode_comparable_uvarint(b, v as u64);
}

/// EncodeComparableUvarint encodes uint64 into mem-comparable bytes.
// go: EncodeComparableUvarint L1
pub fn encode_comparable_uvarint(b: &mut Vec<u8>, v: u64) {
    // The first byte has 256 values, [0, 7] is reserved for negative tags,
    // [248, 255] is reserved for larger positive tags,
    // So we can store value [0, 239] in a single byte.
    // Values cannot be stored in single byte has a tag byte prefix (positiveTagStart+length).
    // Larger value encodes to more bytes, has larger tag.
    if v <= u64::from(positiveTagStart - negativeTagEnd) {
        b.push(v as u8 + negativeTagEnd);
        return;
    }
    let length = if v <= 0xff {
        1
    } else if v <= 0xffff {
        2
    } else if v <= 0xffffff {
        3
    } else if v <= 0xffffffff {
        4
    } else if v <= 0xffffffffff {
        5
    } else if v <= 0xffffffffffff {
        6
    } else if v <= 0xffffffffffffff {
        7
    } else {
        8
    };
    b.push(positiveTagStart + length);
    b.extend_from_slice(&v.to_be_bytes()[8 - length as usize..]);
}

/// `errDecodeInvalid`.
// glue: package variable errDecodeInvalid
fn err_decode_invalid() -> Error {
    errors::new("invalid bytes to decode value")
}

/// DecodeComparableUvarint decodes mem-comparable uvarint.
// go: DecodeComparableUvarint L1
pub fn decode_comparable_uvarint(b: &[u8]) -> Result<(&[u8], u64), Error> {
    if b.is_empty() {
        return Err(err_insufficient());
    }
    let first = b[0];
    let b = &b[1..];
    if first < negativeTagEnd {
        return Err(err_decode_invalid());
    }
    if first <= positiveTagStart {
        return Ok((b, u64::from(first) - u64::from(negativeTagEnd)));
    }
    let length = (first - positiveTagStart) as usize;
    if b.len() < length {
        return Err(err_insufficient());
    }
    let mut v: u64 = 0;
    for &c in &b[..length] {
        v = (v << 8) | u64::from(c);
    }
    Ok((&b[length..], v))
}

/// DecodeComparableVarint decodes mem-comparable varint.
// go: DecodeComparableVarint L1
#[allow(clippy::if_same_then_else)] // Go's two checks kept as written
pub fn decode_comparable_varint(b: &[u8]) -> Result<(&[u8], i64), Error> {
    if b.is_empty() {
        return Err(err_insufficient());
    }
    let first = b[0];
    if (negativeTagEnd..=positiveTagStart).contains(&first) {
        return Ok((b, i64::from(first) - i64::from(negativeTagEnd)));
    }
    let b = &b[1..];
    let length;
    let mut v: u64;
    if first < negativeTagEnd {
        length = (negativeTagEnd - first) as usize;
        v = u64::MAX; // negative value has all bits on by default.
    } else {
        length = (first - positiveTagStart) as usize;
        v = 0;
    }
    if b.len() < length {
        return Err(err_insufficient());
    }
    for &c in &b[..length] {
        v = (v << 8) | u64::from(c);
    }
    if first > positiveTagStart && v > i64::MAX as u64 {
        return Err(err_decode_invalid());
    } else if first < negativeTagEnd && v <= i64::MAX as u64 {
        return Err(err_decode_invalid());
    }
    Ok((&b[length..], v as i64))
}
