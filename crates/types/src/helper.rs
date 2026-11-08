//! go-file: pkg/types/helper.go

use tidb_base::gofmt::Arg;
use tidb_base::gostd::math::pow10;
use tidb_base::gostd::strconv::format_float;

use crate::errors::{ErrBadNumber, ErrOverflow, ErrTruncated, Error};
use crate::gostd::trim_space;

/// RoundFloat rounds float val to the nearest even integer value with float64 format, like MySQL Round function.
/// RoundFloat uses default rounding mode, see https://dev.mysql.com/doc/refman/5.7/en/precision-math-rounding.html
/// so rounding use "round to nearest even".
/// e.g, 1.5 -> 2, -1.5 -> -2.
// go: RoundFloat L1
pub fn round_float(f: f64) -> f64 {
    f.round_ties_even()
}

/// Round rounds the argument f to dec decimal places.
/// dec defaults to 0 if not specified. dec can be negative
/// to cause dec digits left of the decimal point of the
/// value f to become zero.
// go: Round L1
pub fn round(f: f64, dec: isize) -> f64 {
    let shift = pow10(dec);
    let tmp = f * shift;
    if tmp.is_infinite() {
        return f;
    }
    let result = round_float(tmp) / shift;
    if result.is_nan() {
        return 0.0;
    }
    result
}

/// Truncate truncates the argument f to dec decimal places.
/// dec defaults to 0 if not specified. dec can be negative
/// to cause dec digits left of the decimal point of the
/// value f to become zero.
// go: Truncate L1
pub fn truncate(f: f64, dec: isize) -> f64 {
    let shift = pow10(dec);
    let tmp = f * shift;
    if tmp.is_infinite() {
        return f;
    }
    if shift == 0.0 {
        if f.is_nan() {
            return f;
        }
        return 0.0;
    }
    tmp.trunc() / shift
}

/// GetMaxFloat gets the max float for given flen and decimal.
// go: GetMaxFloat L1
pub fn get_max_float(flen: isize, decimal: isize) -> f64 {
    let int_part_len = flen.wrapping_sub(decimal);
    let mut f = pow10(int_part_len);
    f -= pow10(decimal.wrapping_neg());
    f
}

/// TruncateFloat tries to truncate f.
/// If the result exceeds the max/min float that flen/decimal allowed, returns the max/min float allowed.
/// Go returns `(float64, error)`; the value is meaningful together with the error.
// go: TruncateFloat L1 [M-ERRVAL]
pub fn truncate_float(f: f64, flen: isize, decimal: isize) -> (f64, Result<(), Error>) {
    let overflow = || ErrOverflow.gen_with_stack_by_args(vec![Arg::from("DOUBLE"), Arg::from("")]);
    if f.is_nan() {
        // nan returns 0
        return (0.0, Err(overflow()));
    }

    let max_f = get_max_float(flen, decimal);

    let mut f = f;
    if !f.is_infinite() {
        f = round(f, decimal);
    }

    let mut err = Ok(());
    if f > max_f {
        f = max_f;
        err = Err(overflow());
    } else if f < -max_f {
        f = -max_f;
        err = Err(overflow());
    }

    (f, err)
}

/// TruncateFloatToString is used to truncate float to string where dec is the number of digits after the decimal point.
// go: TruncateFloatToString L1
pub fn truncate_float_to_string(f: f64, dec: isize) -> String {
    let f = truncate(f, dec);
    format_float(f, b'f', -1, 64)
}

// go: isSpace L1
pub(crate) fn is_space(c: u8) -> bool {
    c == b' ' || c == b'\t'
}

// go: isDigit L1
pub(crate) fn is_digit(c: u8) -> bool {
    c.is_ascii_digit()
}

/// Returns true if the given byte is an ASCII punctuation character (printable and non-alphanumeric).
// go: isPunctuation L1
pub(crate) fn is_punctuation(c: u8) -> bool {
    (0x21..=0x2F).contains(&c)
        || (0x3A..=0x40).contains(&c)
        || (0x5B..=0x60).contains(&c)
        || (0x7B..=0x7E).contains(&c)
}

/// A byte predicate (Go `func(byte) bool`).
pub(crate) type Pattern = fn(u8) -> bool;

const MAX_UINT: u64 = u64::MAX;
const UINT_CUT_OFF: u64 = MAX_UINT / 10 + 1;
const INT_CUT_OFF: u64 = i64::MAX as u64 + 1;

/// strToInt converts a string to an integer in best effort.
/// Go returns `(int64, error)`; the value is meaningful together with the error.
// go: strToInt L1 [M-INT, M-ERRVAL]
pub fn str_to_int(str: &[u8]) -> (i64, Result<(), Error>) {
    let str = trim_space(str);
    if str.is_empty() {
        return (0, Err(ErrTruncated.share()));
    }
    let mut negative = false;
    let mut i = 0;
    if str[i] == b'-' {
        negative = true;
        i += 1;
    } else if str[i] == b'+' {
        i += 1;
    }

    let mut err: Result<(), Error> = Ok(());
    let mut has_num = false;
    let mut r: u64 = 0;
    while i < str.len() {
        // Go: unicode.IsDigit(rune(str[i])). A single byte is a rune <= MaxLatin1, for which
        // unicode.IsDigit is exactly '0' <= r <= '9'.
        if !str[i].is_ascii_digit() {
            err = Err(ErrTruncated.share());
            break;
        }
        has_num = true;
        if r >= UINT_CUT_OFF {
            r = 0;
            err = Err(ErrBadNumber.share());
            break;
        }
        r *= 10;

        // Go uint64 addition wraps; the `r1 < r` test below relies on it.
        let r1 = r.wrapping_add(u64::from(str[i] - b'0'));
        #[allow(clippy::absurd_extreme_comparisons)]
        if r1 < r || r1 > MAX_UINT {
            r = 0;
            err = Err(ErrBadNumber.share());
            break;
        }
        r = r1;
        i += 1;
    }
    if !has_num {
        err = Err(ErrTruncated.share());
    }

    if !negative && r >= INT_CUT_OFF {
        return (i64::MAX, Err(ErrBadNumber.share()));
    }

    if negative && r > INT_CUT_OFF {
        return (i64::MIN, Err(ErrBadNumber.share()));
    }

    if negative {
        r = r.wrapping_neg();
    }
    (r as i64, err)
}

/// DecimalLength2Precision gets the precision.
// go: DecimalLength2Precision L1
pub fn decimal_length2_precision(length: isize, scale: isize, has_unsigned_flag: bool) -> isize {
    let mut length = length;
    if scale > 0 {
        length -= 1;
    }
    if has_unsigned_flag || length > 0 {
        length -= 1;
    }
    length
}

/// Precision2LengthNoTruncation gets the length.
// go: Precision2LengthNoTruncation L1
pub fn precision2_length_no_truncation(
    length: isize,
    scale: isize,
    has_unsigned_flag: bool,
) -> isize {
    let mut length = length;
    if scale > 0 {
        length += 1;
    }
    if has_unsigned_flag || length > 0 {
        length += 1;
    }
    length
}
