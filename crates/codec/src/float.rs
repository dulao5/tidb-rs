//! go-file: pkg/util/codec/float.go
//!
//! float64 to a comparable uint64. Go's `f >= 0` is false for NaN (and true for -0), so a NaN
//! with the sign bit clear is complemented like a negative number; the port keeps the test.

// Constants keep their Go names (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use tidb_base::Error;

use crate::number::{decode_uint, decode_uint_desc, encode_uint, encode_uint_desc};

const signMask: u64 = 0x8000000000000000;

// go: encodeFloatToCmpUint64 L1
fn encode_float_to_cmp_uint64(f: f64) -> u64 {
    let mut u = f.to_bits();
    if f >= 0.0 {
        u |= signMask;
    } else {
        u = !u;
    }
    u
}

// go: decodeCmpUintToFloat L1
fn decode_cmp_uint_to_float(mut u: u64) -> f64 {
    if u & signMask > 0 {
        u &= !signMask;
    } else {
        u = !u;
    }
    f64::from_bits(u)
}

/// EncodeFloat encodes a float v into a byte slice which can be sorted lexicographically later.
/// EncodeFloat guarantees that the encoded value is in ascending order for comparison.
// go: EncodeFloat L1
pub fn encode_float(b: &mut Vec<u8>, v: f64) {
    let u = encode_float_to_cmp_uint64(v);
    encode_uint(b, u);
}

/// DecodeFloat decodes a float from a byte slice generated with EncodeFloat before.
///
/// Go returns `decodeCmpUintToFloat(0)` (a NaN) with the error; the value is not used.
// go: DecodeFloat L1
pub fn decode_float(b: &[u8]) -> Result<(&[u8], f64), Error> {
    let (b, u) = decode_uint(b)?;
    Ok((b, decode_cmp_uint_to_float(u)))
}

/// EncodeFloatDesc encodes a float v into a byte slice which can be sorted lexicographically later.
/// EncodeFloatDesc guarantees that the encoded value is in descending order for comparison.
// go: EncodeFloatDesc L1
pub fn encode_float_desc(b: &mut Vec<u8>, v: f64) {
    let u = encode_float_to_cmp_uint64(v);
    encode_uint_desc(b, u);
}

/// DecodeFloatDesc decodes a float from a byte slice generated with EncodeFloatDesc before.
// go: DecodeFloatDesc L1
pub fn decode_float_desc(b: &[u8]) -> Result<(&[u8], f64), Error> {
    let (b, u) = decode_uint_desc(b)?;
    Ok((b, decode_cmp_uint_to_float(u)))
}
