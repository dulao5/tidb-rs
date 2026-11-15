//! go-file: pkg/server/internal/util/util.go
//!
//! The parts of the server's helpers the result encoding and the binary parameters use.
//! `CorsHandler` and `NewTestConfig` belong to the HTTP status server and Go's tests, which
//! are not ported here (spec D03.1 §2).

use tidb_base::gostd::strconv::append_float;
use tidb_parser::gocharset::encoding::{Encoding, OpDecode, find_encoding_take_utf8_as_noop};

/// ParseNullTermString parses a null terminated string.
// go: ParseNullTermString L1
pub fn parse_null_term_string(b: &[u8]) -> (Option<&[u8]>, &[u8]) {
    match b.iter().position(|&c| c == 0) {
        None => (None, b),
        Some(off) => (Some(&b[..off]), &b[off + 1..]),
    }
}

/// ParseLengthEncodedInt parses a length encoded integer.
///
/// Like Go it indexes `b` without checking its length: a truncated integer panics.
// go: ParseLengthEncodedInt L1
pub fn parse_length_encoded_int(b: &[u8]) -> (u64, bool, usize) {
    match b[0] {
        // 251: NULL
        0xfb => (0, true, 1),
        // 252: value of following 2
        0xfc => (u64::from(b[1]) | u64::from(b[2]) << 8, false, 3),
        // 253: value of following 3
        0xfd => (
            u64::from(b[1]) | u64::from(b[2]) << 8 | u64::from(b[3]) << 16,
            false,
            4,
        ),
        // 254: value of following 8
        0xfe => (
            u64::from_le_bytes(b[1..9].try_into().expect("8 bytes")),
            false,
            9,
        ),
        // 0-250: value of first byte
        n => (u64::from(n), false, 1),
    }
}

/// ParseLengthEncodedBytes parses a length encoded byte slice. `Err(())` is Go's `io.EOF`.
// go: ParseLengthEncodedBytes L1
#[allow(clippy::result_unit_err)]
pub fn parse_length_encoded_bytes(b: &[u8]) -> (Option<&[u8]>, bool, usize, Result<(), ()>) {
    // Get length
    let (num, is_null, n) = parse_length_encoded_int(b);
    if num < 1 {
        return (None, is_null, n, Ok(()));
    }
    let n = n.wrapping_add(num as usize);
    // Check data length
    if b.len() >= n {
        return (Some(&b[n - num as usize..n]), false, n, Ok(()));
    }
    (None, false, n, Err(()))
}

/// InputDecoder is used to decode input.
#[derive(Clone, Copy, Debug)]
pub struct InputDecoder {
    encoding: Encoding,
}

impl InputDecoder {
    /// NewInputDecoder creates a new InputDecoder.
    // go: NewInputDecoder L1
    pub fn new(chs: &str) -> InputDecoder {
        InputDecoder {
            encoding: find_encoding_take_utf8_as_noop(chs),
        }
    }

    /// DecodeInput decodes input.
    // go: InputDecoder.DecodeInput L1
    pub fn decode_input(&self, src: &[u8]) -> Vec<u8> {
        let (result, err) = self.encoding.transform(src, OpDecode);
        if err.is_err() {
            return src.to_vec();
        }
        result.into_owned()
    }
}

/// LengthEncodedIntSize returns the size of length encoded integer.
// go: LengthEncodedIntSize L1
pub fn length_encoded_int_size(n: u64) -> usize {
    match n {
        0..=250 => 1,
        251..=0xffff => 3,
        0x10000..=0xffffff => 4,
        _ => 9,
    }
}

const EXP_FORMAT_BIG: f64 = 1e15;
const EXP_FORMAT_SMALL: f64 = 1e-15;
const DEFAULT_MYSQL_PREC: isize = 5;

/// AppendFormatFloat appends a float64 to dst in MySQL format.
// go: AppendFormatFloat L1
pub fn append_format_float(out: &mut Vec<u8>, f_val: f64, prec: isize, bit_size: u8) {
    let abs_val = f_val.abs();
    if abs_val > f64::MAX || abs_val.is_nan() {
        // Go returns a new slice `[]byte{'0'}`, dropping what `in` held.
        out.clear();
        out.push(b'0');
        return;
    }
    let is_e_format = if bit_size == 32 {
        let a = abs_val as f32;
        a >= EXP_FORMAT_BIG as f32 || (a != 0.0 && a < EXP_FORMAT_SMALL as f32)
    } else {
        abs_val >= EXP_FORMAT_BIG || (abs_val != 0.0 && abs_val < EXP_FORMAT_SMALL)
    };
    let start = out.len();
    if is_e_format {
        let prec = if bit_size == 32 {
            DEFAULT_MYSQL_PREC
        } else {
            prec
        };
        append_float(out, f_val, b'e', prec, bit_size);
        // remove the '+' from the string for compatibility.
        if let Some(plus_pos) = out[start..].iter().position(|&c| c == b'+')
            && plus_pos > 0
        {
            out.remove(start + plus_pos);
        }
        // remove extra '0'
        let e_pos = out[start..]
            .iter()
            .position(|&c| c == b'e')
            .map_or(-1, |p| p as isize);
        let point_pos = out[start..]
            .iter()
            .position(|&c| c == b'.')
            .map_or(-1, |p| p as isize);
        let e_pos_in_out = start as isize + e_pos;
        let point_pos_in_out = start as isize + point_pos;
        let mut valid_pos = e_pos_in_out;
        let mut i = e_pos_in_out - 1;
        while i >= point_pos_in_out {
            let c = out[i as usize];
            if !(c == b'0' || c == b'.') {
                break;
            }
            valid_pos = i;
            i -= 1;
        }
        out.drain(valid_pos as usize..e_pos_in_out as usize);
    } else {
        append_float(out, f_val, b'f', prec, bit_size);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The helpers no govec op reaches (Go's `util_test.go` cases).
    #[test]
    fn length_encoded_helpers() {
        assert_eq!(
            parse_null_term_string(b"abc\0def"),
            (Some(&b"abc"[..]), &b"def"[..])
        );
        assert_eq!(parse_null_term_string(b"abc"), (None, &b"abc"[..]));
        assert_eq!(parse_length_encoded_bytes(&[0xfb]), (None, true, 1, Ok(())));
        assert_eq!(parse_length_encoded_bytes(&[0]), (None, false, 1, Ok(())));
        assert_eq!(
            parse_length_encoded_bytes(&[2, b'a', b'b', b'c']),
            (Some(&b"ab"[..]), false, 3, Ok(()))
        );
        assert_eq!(
            parse_length_encoded_bytes(&[3, b'a']),
            (None, false, 4, Err(()))
        );
        for (n, size) in [
            (0u64, 1),
            (250, 1),
            (251, 3),
            (0xffff, 3),
            (0x10000, 4),
            (0xffffff, 4),
            (0x1000000, 9),
            (u64::MAX, 9),
        ] {
            assert_eq!(length_encoded_int_size(n), size, "{n}");
            let mut b = Vec::new();
            crate::dump::length_encoded_int(&mut b, n);
            assert_eq!(b.len(), size);
            assert_eq!(parse_length_encoded_int(&b), (n, false, size));
        }
    }
}
