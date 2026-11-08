//! go-file: pkg/types/binary_literal.go
//!
//! Mapping: `BinaryLiteral` is a byte vector (Go `[]byte`; Go's nil and empty slices behave
//! the same in every function here). Go `string` results that may hold any byte are
//! `Vec<u8>`. The standard library errors (`strconv.ParseUint`, `hex.DecodeString`,
//! `errors.Errorf`) are `Error::plain` with Go's message text.

#![allow(non_upper_case_globals)]

use tidb_base::errors::errorf;
use tidb_base::gofmt::{Arg, sprintf};

use crate::context::Context;
use crate::errors::{ErrTruncatedWrongVal, Error};

/// BinaryLiteral is the internal type for storing bit / hex literal type.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct BinaryLiteral(pub Vec<u8>);

/// BitLiteral is the bit literal type.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BitLiteral(pub Vec<u8>);

/// HexLiteral is the hex literal type.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HexLiteral(pub Vec<u8>);

/// ZeroBinaryLiteral is a BinaryLiteral literal with zero value.
pub const ZeroBinaryLiteral: BinaryLiteral = BinaryLiteral(Vec::new());

// go: trimLeadingZeroBytes L1
fn trim_leading_zero_bytes(bytes: &[u8]) -> &[u8] {
    if bytes.is_empty() {
        return bytes;
    }
    let (mut pos, pos_max) = (0, bytes.len() - 1);
    while pos < pos_max {
        if bytes[pos] != 0 {
            break;
        }
        pos += 1;
    }
    &bytes[pos..]
}

/// NewBinaryLiteralFromUint creates a new BinaryLiteral instance by the given uint value in BitEndian.
/// byteSize will be used as the length of the new BinaryLiteral, with leading bytes filled to zero.
/// If byteSize is -1, the leading zeros in new BinaryLiteral will be trimmed.
// go: NewBinaryLiteralFromUint L1
pub fn new_binary_literal_from_uint(value: u64, byte_size: isize) -> BinaryLiteral {
    if byte_size != -1 && !(1..=8).contains(&byte_size) {
        panic!("Invalid byteSize");
    }
    let buf = value.to_be_bytes();
    if byte_size == -1 {
        BinaryLiteral(trim_leading_zero_bytes(&buf).to_vec())
    } else {
        BinaryLiteral(buf[8 - byte_size as usize..].to_vec())
    }
}

impl BinaryLiteral {
    /// String implements fmt.Stringer interface.
    // go: BinaryLiteral.String L1
    pub fn string(&self) -> String {
        if self.0.is_empty() {
            return String::new();
        }
        let mut s = String::with_capacity(2 + 2 * self.0.len());
        s.push_str("0x");
        for b in &self.0 {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }

    /// ToString returns the string representation for the literal.
    // go: BinaryLiteral.ToString L1
    pub fn to_string(&self) -> Vec<u8> {
        self.0.clone()
    }

    /// ToBitLiteralString returns the bit literal representation for the literal.
    // go: BinaryLiteral.ToBitLiteralString L1
    pub fn to_bit_literal_string(&self, trim_leading_zero: bool) -> String {
        if self.0.is_empty() {
            return "b''".to_owned();
        }
        let mut buf = String::with_capacity(8 * self.0.len());
        for data in &self.0 {
            buf.push_str(&format!("{data:08b}"));
        }
        let mut ret: &str = &buf;
        if trim_leading_zero {
            ret = ret.trim_start_matches('0');
            if ret.is_empty() {
                ret = "0";
            }
        }
        format!("b'{ret}'")
    }

    /// ToInt returns the int value for the literal.
    /// Go returns `(uint64, error)`; the value is meaningful together with the error.
    // go: BinaryLiteral.ToInt L1 [M-ERRVAL]
    pub fn to_int(&self, ctx: &Context) -> (u64, Result<(), Error>) {
        let buf = trim_leading_zero_bytes(&self.0);
        let length = buf.len();
        if length == 0 {
            return (0, Ok(()));
        }
        if length > 8 {
            let err = ErrTruncatedWrongVal.fast_gen_by_args(vec![
                Arg::from("BINARY"),
                Arg::Stringer(self.string().into_bytes(), "types.BinaryLiteral"),
            ]);
            let err = ctx.handle_truncate(Err(err));
            return (u64::MAX, err);
        }
        // Note: the byte-order is BigEndian.
        let mut val = u64::from(buf[0]);
        for b in &buf[1..length] {
            val = (val << 8) | u64::from(*b);
        }
        (val, Ok(()))
    }

    /// Compare compares BinaryLiteral to another one
    // glue: method form of compare_bytes (BinaryLiteral.Compare)
    pub fn compare(&self, b2: &BinaryLiteral) -> isize {
        compare_bytes(&self.0, &b2.0)
    }
}

/// `BinaryLiteral(b).Compare(BinaryLiteral(b2))` on borrowed bytes: compares after trimming
/// leading zero bytes, the longer value is larger. Comparators over chunk cells call it
/// without building an owned `BinaryLiteral` per comparison.
// go: BinaryLiteral.Compare L1 (receiver and argument as borrowed bytes)
pub fn compare_bytes(b: &[u8], b2: &[u8]) -> isize {
    let buf_b = trim_leading_zero_bytes(b);
    let buf_b2 = trim_leading_zero_bytes(b2);
    if buf_b.len() > buf_b2.len() {
        return 1;
    }
    if buf_b.len() < buf_b2.len() {
        return -1;
    }
    buf_b.cmp(buf_b2) as isize
}

/// `strconv.ParseUint(s, 2, 8)` for the 8-character groups of ParseBitStr: Go's
/// `*strconv.NumError` text on a byte other than `0` / `1` (8 binary digits never overflow).
// go-std: strconv.ParseUint L2 (base 2, bitSize 8, len(s) == 8)
fn parse_uint_base2_8(s: &[u8]) -> Result<u8, Error> {
    let mut v: u8 = 0;
    for &c in s {
        match c {
            b'0' | b'1' => v = (v << 1) | (c - b'0'),
            _ => {
                return Err(Error::plain(sprintf(
                    "strconv.ParseUint: parsing %q: invalid syntax",
                    &[Arg::go_string(s.to_vec())],
                )));
            }
        }
    }
    Ok(v)
}

/// `strings.Trim(s, "'")`.
// go-std: strings.Trim L2 (cutset of one ASCII byte)
fn trim_quote(s: &[u8]) -> &[u8] {
    let mut s = s;
    while let [b'\'', rest @ ..] = s {
        s = rest;
    }
    while let [rest @ .., b'\''] = s {
        s = rest;
    }
    s
}

/// ParseBitStr parses bit string.
/// The string format can be b'val', B'val' or 0bval, val must be 0 or 1.
/// See https://dev.mysql.com/doc/refman/5.7/en/bit-value-literals.html
// go: ParseBitStr L1
pub fn parse_bit_str(s: &[u8]) -> Result<BinaryLiteral, Error> {
    if s.is_empty() {
        return Err(errorf("invalid empty string for parsing bit type", &[]));
    }
    let s = if s[0] == b'b' || s[0] == b'B' {
        // format is b'val' or B'val'
        trim_quote(&s[1..])
    } else if !s.starts_with(b"0b") {
        // here means format is not b'val', B'val' or 0bval.
        return Err(errorf(
            "invalid bit type format %s",
            &[Arg::go_string(s.to_vec())],
        ));
    } else {
        &s[2..]
    };
    if s.is_empty() {
        return Ok(ZeroBinaryLiteral);
    }
    let aligned_length = (s.len() + 7) & !7;
    let mut padded = b"00000000".to_vec();
    padded.extend_from_slice(s);
    let s = &padded[s.len() + 8 - aligned_length..]; // Pad with zero (slice from `-alignedLength`)
    let byte_length = s.len() >> 3;
    let mut buf = vec![0u8; byte_length];
    for (i, b) in buf.iter_mut().enumerate() {
        let str_position = i << 3;
        *b = parse_uint_base2_8(&s[str_position..str_position + 8])?;
    }
    Ok(BinaryLiteral(buf))
}

/// NewBitLiteral parses bit string as BitLiteral type.
// go: NewBitLiteral L1
pub fn new_bit_literal(s: &[u8]) -> Result<BitLiteral, Error> {
    let b = parse_bit_str(s)?;
    Ok(BitLiteral(b.0))
}

impl BitLiteral {
    /// ToString implement ast.BinaryLiteral interface
    // go: BitLiteral.ToString L1
    pub fn to_string(&self) -> Vec<u8> {
        self.0.clone()
    }
}

/// `hex.DecodeString(s)` for an even-length `s`: the `hex.InvalidByteError` text
/// (`encoding/hex: invalid byte: %#U`) for the first byte that is not a hex digit.
// go-std: encoding/hex.DecodeString L2 (even length; ErrLength cannot occur)
fn hex_decode_string(s: &[u8]) -> Result<Vec<u8>, Error> {
    let val = |c: u8| -> Result<u8, Error> {
        match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'a'..=b'f' => Ok(c - b'a' + 10),
            b'A'..=b'F' => Ok(c - b'A' + 10),
            _ => Err(Error::plain(sprintf(
                "encoding/hex: invalid byte: %#U",
                &[Arg::Int(i64::from(c), "int32")],
            ))),
        }
    };
    let mut out = Vec::with_capacity(s.len() / 2);
    for pair in s.chunks(2) {
        // Go checks the first byte of the pair before the second.
        let hi = val(pair[0])?;
        let lo = val(pair[1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

/// ParseHexStr parses hexadecimal string literal.
/// See https://dev.mysql.com/doc/refman/5.7/en/hexadecimal-literals.html
// go: ParseHexStr L1
pub fn parse_hex_str(s: &[u8]) -> Result<BinaryLiteral, Error> {
    if s.is_empty() {
        return Err(errorf(
            "invalid empty string for parsing hexadecimal literal",
            &[],
        ));
    }
    let mut s = if s[0] == b'x' || s[0] == b'X' {
        // format is x'val' or X'val'
        let s = trim_quote(&s[1..]);
        if !s.len().is_multiple_of(2) {
            return Err(errorf(
                "invalid hexadecimal format, must even numbers, but %d",
                &[Arg::from(s.len() as i64)],
            ));
        }
        s.to_vec()
    } else if !s.starts_with(b"0x") {
        // here means format is not x'val', X'val' or 0xval.
        return Err(errorf(
            "invalid hexadecimal format %s",
            &[Arg::go_string(s.to_vec())],
        ));
    } else {
        s[2..].to_vec()
    };
    if s.is_empty() {
        return Ok(ZeroBinaryLiteral);
    }
    if !s.len().is_multiple_of(2) {
        s.insert(0, b'0');
    }
    let buf = hex_decode_string(&s)?;
    Ok(BinaryLiteral(buf))
}

/// NewHexLiteral parses hexadecimal string as HexLiteral type.
// go: NewHexLiteral L1
pub fn new_hex_literal(s: &[u8]) -> Result<HexLiteral, Error> {
    let h = parse_hex_str(s)?;
    Ok(HexLiteral(h.0))
}

impl HexLiteral {
    /// ToString implement ast.BinaryLiteral interface
    // go: HexLiteral.ToString L1
    pub fn to_string(&self) -> Vec<u8> {
        self.0.clone()
    }
}
