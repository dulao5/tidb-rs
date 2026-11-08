//! The `encoding/binary` varint functions the codec package calls (Go standard library,
//! go1.25.12, `src/encoding/binary/varint.go`).

// Constants keep their Go names (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

/// MaxVarintLen64 is the maximum length of a varint-encoded 64-bit integer.
pub const MaxVarintLen64: usize = 10;

/// PutUvarint encodes a uint64 into buf (appended here) and returns the number of bytes
/// written.
// go-std: encoding/binary.PutUvarint L1 (appends to a Vec instead of writing into a sized buffer)
pub fn put_uvarint(buf: &mut Vec<u8>, mut x: u64) -> usize {
    let start = buf.len();
    while x >= 0x80 {
        buf.push(x as u8 | 0x80);
        x >>= 7;
    }
    buf.push(x as u8);
    buf.len() - start
}

/// Uvarint decodes a uint64 from buf and returns that value and the number of bytes read
/// (> 0). If an error occurred, the value is 0 and the number of bytes n is <= 0 meaning:
///   - n == 0: buf too small;
///   - n < 0: value larger than 64 bits (overflow) and -n is the number of bytes read.
// go-std: encoding/binary.Uvarint L1
pub fn uvarint(buf: &[u8]) -> (u64, isize) {
    let mut x: u64 = 0;
    let mut s: u32 = 0;
    for (i, &b) in buf.iter().enumerate() {
        if i == MaxVarintLen64 {
            // Catch byte reads past MaxVarintLen64.
            // See issue https://golang.org/issues/41185
            return (0, -(i as isize + 1)); // overflow
        }
        if b < 0x80 {
            if i == MaxVarintLen64 - 1 && b > 1 {
                return (0, -(i as isize + 1)); // overflow
            }
            return (x | (u64::from(b) << s), i as isize + 1);
        }
        x |= u64::from(b & 0x7f) << s;
        s += 7;
    }
    (0, 0)
}

/// PutVarint encodes an int64 into buf (appended here) and returns the number of bytes
/// written.
// go-std: encoding/binary.PutVarint L1 (appends to a Vec)
pub fn put_varint(buf: &mut Vec<u8>, x: i64) -> usize {
    let mut ux = (x as u64) << 1;
    if x < 0 {
        ux = !ux;
    }
    put_uvarint(buf, ux)
}

/// Varint decodes an int64 from buf and returns that value and the number of bytes read
/// (> 0). If an error occurred, the value is 0 and the number of bytes n is <= 0 with the
/// following meaning:
///   - n == 0: buf too small;
///   - n < 0: value larger than 64 bits (overflow) and -n is the number of bytes read.
// go-std: encoding/binary.Varint L1
pub fn varint(buf: &[u8]) -> (i64, isize) {
    let (ux, n) = uvarint(buf); // ok to continue in presence of error
    let mut x = (ux >> 1) as i64;
    if ux & 1 != 0 {
        x = !x;
    }
    (x, n)
}
