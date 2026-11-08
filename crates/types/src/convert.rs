//! go-file: pkg/types/convert.go
//!
//! Numeric and string conversions (block D01.4, ported with D01.1).
//!
//! Mapping:
//! - Go `string` arguments are `&[u8]` (any byte); results that Go builds from them are
//!   `Vec<u8>`.
//! - Functions returning `(T, error)` whose T is meaningful with the error return
//!   `(T, Result<(), Error>)` (M-ERRVAL).
//! - `int64(f)` of a float outside the int64 range or NaN is platform dependent in Go
//!   (M-F2I): `gostd::conv::f64_to_i64`.
//! - `errors.Trace` / `errors.WithStack` are not ported (M-ERR).
//! - `ToString(value any)` takes Go's dynamic types; it comes with the Datum part of the
//!   block (the `GoValue` enum), see specs/D01.1-datum.

#![allow(non_upper_case_globals)]

use tidb_base::gofmt::Arg;
use tidb_base::gostd::conv::f64_to_i64;
use tidb_base::mysql;
use tidb_parser::types::UnspecifiedLength;

use crate::context::DefaultStmtNoWarningContext;
use crate::context::{Context, Flags};
use crate::errors::{ErrOverflow, ErrTruncatedWrongVal, Error, TimeStr};
use crate::etc::overflow;
use crate::gostd::{NumErrKind, atoi, parse_float, parse_int, parse_uint, trim_space};
use crate::helper::{is_digit, round_float};
use crate::json::{
    BinaryJSON, JSONLiteralFalse, JSONLiteralNil, JSONTypeCodeArray, JSONTypeCodeDate,
    JSONTypeCodeDatetime, JSONTypeCodeDuration, JSONTypeCodeFloat64, JSONTypeCodeInt64,
    JSONTypeCodeLiteral, JSONTypeCodeObject, JSONTypeCodeOpaque, JSONTypeCodeString,
    JSONTypeCodeTimestamp, JSONTypeCodeUint64,
};
use crate::mydecimal::MyDecimal;
use crate::time::{
    Duration, Time, TimeMaxHour, TimeMaxValue, ZeroDuration, max_mysql_duration, new_duration,
    parse_datetime_from_num, parse_duration, parse_time,
};

// go: truncateStr L1
pub fn truncate_str(str: &[u8], flen: isize) -> &[u8] {
    if flen != UnspecifiedLength && str.len() as isize > flen {
        return &str[..flen as usize];
    }
    str
}

/// IntegerUnsignedUpperBound indicates the max uint64 values of different mysql types.
// go: IntegerUnsignedUpperBound L1
pub fn integer_unsigned_upper_bound(int_type: u8) -> u64 {
    match int_type {
        mysql::TypeTiny => u64::from(u8::MAX),
        mysql::TypeShort => u64::from(u16::MAX),
        mysql::TypeInt24 => mysql::MaxUint24 as u64,
        mysql::TypeLong => u64::from(u32::MAX),
        mysql::TypeLonglong => u64::MAX,
        mysql::TypeBit => u64::MAX,
        mysql::TypeEnum => {
            // enum can have at most 65535 distinct elements
            // it would be better to use len(FieldType.GetElems()), but we only have a byte type here
            65535
        }
        mysql::TypeSet => u64::MAX,
        _ => panic!("Input byte is not a mysql type"),
    }
}

/// IntegerSignedUpperBound indicates the max int64 values of different mysql types.
// go: IntegerSignedUpperBound L1
pub fn integer_signed_upper_bound(int_type: u8) -> i64 {
    match int_type {
        mysql::TypeTiny => i64::from(i8::MAX),
        mysql::TypeShort => i64::from(i16::MAX),
        mysql::TypeInt24 => mysql::MaxInt24,
        mysql::TypeLong => i64::from(i32::MAX),
        mysql::TypeLonglong => i64::MAX,
        mysql::TypeEnum => {
            // enum can have at most 65535 distinct elements
            // it would be better to use len(FieldType.GetElems()), but we only have a byte type here
            65535
        }
        _ => panic!("Input byte is not a mysql int type"),
    }
}

/// IntegerSignedLowerBound indicates the min int64 values of different mysql types.
// go: IntegerSignedLowerBound L1
pub fn integer_signed_lower_bound(int_type: u8) -> i64 {
    match int_type {
        mysql::TypeTiny => i64::from(i8::MIN),
        mysql::TypeShort => i64::from(i16::MIN),
        mysql::TypeInt24 => mysql::MinInt24,
        mysql::TypeLong => i64::from(i32::MIN),
        mysql::TypeLonglong => i64::MIN,
        mysql::TypeEnum => 0,
        _ => panic!("Input byte is not a mysql type"),
    }
}

/// ConvertFloatToInt converts a float64 value to a int value.
/// `tp` is used in err msg, if there is overflow, this func will report err according to `tp`
// go: ConvertFloatToInt L1 [M-ERRVAL, M-F2I]
pub fn convert_float_to_int(
    fval: f64,
    lower_bound: i64,
    upper_bound: i64,
    tp: u8,
) -> (i64, Result<(), Error>) {
    let val = round_float(fval);
    if val < lower_bound as f64 {
        return (lower_bound, Err(overflow(Arg::from(val), tp)));
    }

    if val >= upper_bound as f64 {
        if val == upper_bound as f64 {
            return (upper_bound, Ok(()));
        }
        return (upper_bound, Err(overflow(Arg::from(val), tp)));
    }
    // [M-F2I] NaN passes both tests: int64(NaN) depends on the platform.
    (f64_to_i64(val), Ok(()))
}

/// ConvertIntToInt converts an int value to another int value of different precision.
// go: ConvertIntToInt L1 [M-ERRVAL]
pub fn convert_int_to_int(
    val: i64,
    lower_bound: i64,
    upper_bound: i64,
    tp: u8,
) -> (i64, Result<(), Error>) {
    if val < lower_bound {
        return (lower_bound, Err(overflow(Arg::from(val), tp)));
    }

    if val > upper_bound {
        return (upper_bound, Err(overflow(Arg::from(val), tp)));
    }

    (val, Ok(()))
}

/// ConvertUintToInt converts an uint value to an int value.
// go: ConvertUintToInt L1 [M-ERRVAL]
pub fn convert_uint_to_int(val: u64, upper_bound: i64, tp: u8) -> (i64, Result<(), Error>) {
    if val > upper_bound as u64 {
        return (upper_bound, Err(overflow(Arg::from(val), tp)));
    }

    (val as i64, Ok(()))
}

/// ConvertIntToUint converts an int value to an uint value.
// go: ConvertIntToUint L1 [M-ERRVAL]
pub fn convert_int_to_uint(
    flags: Flags,
    val: i64,
    upper_bound: u64,
    tp: u8,
) -> (u64, Result<(), Error>) {
    if val < 0 && !flags.allow_negative_to_unsigned() {
        return (0, Err(overflow(Arg::from(val), tp)));
    }

    if val as u64 > upper_bound {
        return (upper_bound, Err(overflow(Arg::from(val), tp)));
    }

    (val as u64, Ok(()))
}

/// ConvertUintToUint converts an uint value to another uint value of different precision.
// go: ConvertUintToUint L1 [M-ERRVAL]
pub fn convert_uint_to_uint(val: u64, upper_bound: u64, tp: u8) -> (u64, Result<(), Error>) {
    if val > upper_bound {
        return (upper_bound, Err(overflow(Arg::from(val), tp)));
    }

    (val, Ok(()))
}

/// ConvertFloatToUint converts a float value to an uint value.
///
/// `new(big.Float).SetFloat64(val).Uint64()`: `val` is an integer (RoundToEven) or ±Inf or
/// NaN here. NaN panics in `SetFloat64` (`big.ErrNaN`); +Inf and values above MaxUint64 are
/// `(MaxUint64, Below)`; the rest converts exactly.
// go: ConvertFloatToUint L1 [M-ERRVAL, M-F2I]
pub fn convert_float_to_uint(
    flags: Flags,
    fval: f64,
    upper_bound: u64,
    tp: u8,
) -> (u64, Result<(), Error>) {
    let val = round_float(fval);
    if val < 0.0 {
        if !flags.allow_negative_to_unsigned() {
            return (0, Err(overflow(Arg::from(val), tp)));
        }
        // [M-F2I] uint64(int64(val)): int64 of a value below MinInt64 depends on the platform.
        return (f64_to_i64(val) as u64, Err(overflow(Arg::from(val), tp)));
    }

    if val.is_nan() {
        panic!("Float.SetFloat64(NaN)");
    }
    // big.Float.Uint64: (MaxUint64, Below) above the range, exact otherwise.
    let (ret, below) = if val >= 18446744073709551616.0 {
        (u64::MAX, true)
    } else {
        (val as u64, false)
    };
    if below || ret > upper_bound {
        return (upper_bound, Err(overflow(Arg::from(val), tp)));
    }
    (ret, Ok(()))
}

/// convertScientificNotation converts a decimal string with scientific notation to a normal decimal string.
/// 1E6 => 1000000, .12345E+5 => 12345
///
/// Go slices and concatenates with `int(exp)`; an exponent that moves the point outside the
/// string panics (slice bounds) in Go, and here (the same slices).
// go: convertScientificNotation L1
fn convert_scientific_notation(str: &[u8]) -> Result<Vec<u8>, Error> {
    // https://golang.org/ref/spec#Floating-point_literals
    let mut e_idx: isize = -1;
    let mut point: isize = -1;
    for (i, &c) in str.iter().enumerate() {
        if c == b'.' {
            point = i as isize;
        }
        if c == b'e' || c == b'E' {
            e_idx = i as isize;
            if point == -1 {
                point = i as isize;
            }
            break;
        }
    }
    if e_idx == -1 {
        return Ok(str.to_vec());
    }
    let (exp, err) = parse_int(&str[e_idx as usize + 1..], 64);
    if let Err(e) = err {
        return Err(e.into_base());
    }

    let f = &str[..e_idx as usize];
    let p = point as usize;
    let cat = |parts: &[&[u8]]| -> Vec<u8> { parts.concat() };
    let zeros = |n: isize| -> Vec<u8> {
        // strings.Repeat panics on a negative count.
        assert!(n >= 0, "strings: negative Repeat count");
        vec![b'0'; n as usize]
    };
    let flen = f.len() as isize;
    if exp == 0 {
        return Ok(f.to_vec());
    } else if exp > 0 {
        // move point right
        let e = exp as isize; // Go: int(exp)
        let pe = point.wrapping_add(e);
        if pe == flen - 1 {
            // 123.456 >> 3 = 123456. = 123456
            return Ok(cat(&[&f[..p], &f[p + 1..]]));
        } else if pe < flen - 1 {
            // 123.456 >> 2 = 12345.6
            let q = pe as usize + 1;
            return Ok(cat(&[&f[..p], &f[p + 1..q], b".", &f[q..]]));
        }
        // 123.456 >> 5 = 12345600
        return Ok(cat(&[&f[..p], &f[p + 1..], &zeros(pe - flen + 1)]));
    }
    // move point left
    let e = exp.wrapping_neg() as isize;
    if e < point {
        // 123.456 << 2 = 1.23456
        let q = (point - e) as usize;
        return Ok(cat(&[&f[..q], b".", &f[q..p], &f[p + 1..]]));
    }
    // 123.456 << 5 = 0.00123456
    Ok(cat(&[
        b"0.",
        &zeros(e.wrapping_sub(point)),
        &f[..p],
        &f[p + 1..],
    ]))
}

// go: convertDecimalStrToUint L1 [M-ERRVAL]
fn convert_decimal_str_to_uint(str: &[u8], upper_bound: u64, tp: u8) -> (u64, Result<(), Error>) {
    let str = match convert_scientific_notation(str) {
        Ok(s) => s,
        Err(err) => return (0, Err(err)),
    };

    let (int_str, frac_str): (&[u8], &[u8]) = match str.iter().position(|&c| c == b'.') {
        None => (&str, b""),
        Some(p) => (&str[..p], &str[p + 1..]),
    };
    let mut int_str = {
        let n = int_str.iter().take_while(|&&c| c == b'0').count();
        &int_str[n..]
    };
    if int_str.is_empty() {
        int_str = b"0";
    }
    if int_str[0] == b'-' {
        return (0, Err(overflow(Arg::go_string(str.clone()), tp)));
    }

    let mut round = 0u64;
    if !frac_str.is_empty() && frac_str[0] >= b'5' {
        round += 1;
    }

    let upper_str = upper_bound.wrapping_sub(round).to_string();
    if int_str.len() > upper_str.len()
        || (int_str.len() == upper_str.len() && int_str > upper_str.as_bytes())
    {
        return (upper_bound, Err(overflow(Arg::go_string(str.clone()), tp)));
    }

    let (val, err) = parse_uint(int_str, 64);
    if err.is_err() {
        return (val, Err(overflow(Arg::go_string(str.clone()), tp)));
    }
    (val.wrapping_add(round), Ok(()))
}

/// ConvertDecimalToUint converts a decimal to a uint by converting it to a string first to avoid float overflow (#10181).
// go: ConvertDecimalToUint L1 [M-ERRVAL]
pub fn convert_decimal_to_uint(
    d: &MyDecimal,
    upper_bound: u64,
    tp: u8,
) -> (u64, Result<(), Error>) {
    convert_decimal_str_to_uint(&d.to_string(), upper_bound, tp)
}

/// StrToInt converts a string to an integer at the best-effort.
// go: StrToInt L1 [M-ERRVAL]
pub fn str_to_int(ctx: &Context, str: &[u8], is_func_cast: bool) -> (i64, Result<(), Error>) {
    let str = trim_space(str);
    let (valid_prefix, err) = get_valid_int_prefix(ctx, str, is_func_cast);
    let (i_val, err1) = parse_int(&valid_prefix, 64);
    if err1.is_err() {
        return (
            i_val,
            Err(ErrOverflow
                .gen_with_stack_by_args(vec![Arg::from("BIGINT"), Arg::go_string(valid_prefix)])),
        );
    }
    (i_val, err)
}

/// StrToUint converts a string to an unsigned integer at the best-effort.
// go: StrToUint L1 [M-ERRVAL]
pub fn str_to_uint(ctx: &Context, str: &[u8], is_func_cast: bool) -> (u64, Result<(), Error>) {
    let str = trim_space(str);
    let (mut valid_prefix, err) = get_valid_int_prefix(ctx, str, is_func_cast);
    let mut u_val = 0u64;
    let mut has_parse_err = false;

    if valid_prefix[0] == b'-' {
        // only `-000*` is valid to be converted into unsigned integer
        for &v in &valid_prefix[1..] {
            if v != b'0' {
                has_parse_err = true;
                break;
            }
        }
    } else {
        if valid_prefix[0] == b'+' {
            valid_prefix.remove(0);
        }
        let (v, e) = parse_uint(&valid_prefix, 64);
        u_val = v;
        has_parse_err = e.is_err();
    }

    if has_parse_err {
        return (
            u_val,
            Err(ErrOverflow.gen_with_stack_by_args(vec![
                Arg::from("BIGINT UNSIGNED"),
                Arg::go_string(valid_prefix),
            ])),
        );
    }
    (u_val, err)
}

/// StrToDateTime converts str to MySQL DateTime.
// go: StrToDateTime L1 [M-ERRVAL]
pub fn str_to_date_time(ctx: &Context, str: &[u8], fsp: isize) -> (Time, Result<(), Error>) {
    parse_time(ctx, str, mysql::TypeDatetime, fsp)
}

/// StrToDuration converts str to Duration. It returns Duration in normal case,
/// and returns Time when str is in datetime format.
/// when isDuration is true, the d is returned, when it is false, the t is returned.
/// See https://dev.mysql.com/doc/refman/5.5/en/date-and-time-literals.html.
// go: StrToDuration L1 [M-ERRVAL]
pub fn str_to_duration(
    ctx: &Context,
    str: &[u8],
    fsp: isize,
) -> (Duration, Time, bool, Result<(), Error>) {
    let str = trim_space(str);
    let mut length = str.len() as isize;
    if length > 0 && str[0] == b'-' {
        length -= 1;
    }
    if let Some(n) = str.iter().position(|&c| c == b'.') {
        length -= (str.len() - n) as isize;
    }
    let d = Duration::default();
    let mut t = Time::default();
    // Timestamp format is 'YYYYMMDDHHMMSS' or 'YYMMDDHHMMSS', which length is 12.
    // See #3923, it explains what we do here.
    if length >= 12 {
        let err;
        (t, err) = str_to_date_time(ctx, str, fsp);
        if err.is_ok() {
            return (d, t, false, Ok(()));
        }
    }

    let (d, _, mut err) = parse_duration(ctx, str, fsp);
    if let Err(e) = &err
        && ErrTruncatedWrongVal.equal(e)
    {
        err = ctx.handle_truncate(err);
    }
    (d, t, true, err)
}

/// NumberToDuration converts number to Duration.
// go: NumberToDuration L1 [M-ERRVAL]
pub fn number_to_duration(number: i64, fsp: isize) -> (Duration, Result<(), Error>) {
    let max = TimeMaxValue as i64;
    if number > max {
        // Try to parse DATETIME.
        if number >= 10000000000 {
            // '2001-00-00 00-00-00'
            let (t, err) = parse_datetime_from_num(&DefaultStmtNoWarningContext, number);
            if err.is_ok() {
                return match t.convert_to_duration() {
                    Ok(dur) => (dur, Ok(())),
                    Err(e) => (Duration::default(), Err(e)),
                };
            }
        }
        let dur = max_mysql_duration(fsp);
        return (
            dur,
            Err(ErrOverflow.gen_with_stack_by_args(vec![
                Arg::from("Duration"),
                Arg::from(number.to_string()),
            ])),
        );
    } else if number < -max {
        let mut dur = max_mysql_duration(fsp);
        dur.duration = dur.duration.wrapping_neg();
        return (
            dur,
            Err(ErrOverflow.gen_with_stack_by_args(vec![
                Arg::from("Duration"),
                Arg::from(number.to_string()),
            ])),
        );
    }
    let mut number = number;
    let neg = number < 0;
    if neg {
        number = -number;
    }

    if number / 10000 > TimeMaxHour as i64 || number % 100 >= 60 || (number / 100) % 100 >= 60 {
        return (
            ZeroDuration,
            Err(ErrTruncatedWrongVal
                .gen_with_stack_by_args(vec![Arg::from(TimeStr), Arg::from(number.to_string())])),
        );
    }
    let mut dur = new_duration(
        (number / 10000) as isize,
        ((number / 100) % 100) as isize,
        (number % 100) as isize,
        0,
        fsp,
    );
    if neg {
        dur.duration = dur.duration.wrapping_neg();
    }
    (dur, Ok(()))
}

/// getValidIntPrefix gets prefix of the string which can be successfully parsed as int.
// go: getValidIntPrefix L1 [M-ERRVAL]
fn get_valid_int_prefix(
    ctx: &Context,
    str: &[u8],
    is_func_cast: bool,
) -> (Vec<u8>, Result<(), Error>) {
    if !is_func_cast {
        let (float_prefix, err) = get_valid_float_prefix(ctx, str, is_func_cast);
        if err.is_err() {
            return (float_prefix, err);
        }
        return float_str_to_int_str(&float_prefix, str);
    }

    let mut valid_len = 0;

    for (i, &c) in str.iter().enumerate() {
        if (c == b'+' || c == b'-') && i == 0 {
            continue;
        }

        if c.is_ascii_digit() {
            valid_len = i + 1;
            continue;
        }

        break;
    }
    let mut valid = str[..valid_len].to_vec();
    if valid.is_empty() {
        valid = b"0".to_vec();
    }
    if valid_len == 0 || valid_len != str.len() {
        return (
            valid,
            ctx.handle_truncate(Err(ErrTruncatedWrongVal.gen_with_stack_by_args(vec![
                Arg::from("INTEGER"),
                Arg::go_string(str.to_vec()),
            ]))),
        );
    }
    (valid, Ok(()))
}

/// roundIntStr is to round a **valid int string** base on the number following dot.
// go: roundIntStr L1
fn round_int_str(num_next_dot: u8, int_str: &[u8]) -> Vec<u8> {
    if num_next_dot < b'5' {
        return int_str.to_vec();
    }
    let mut ret_str = int_str.to_vec();
    let mut idx = int_str.len() as isize - 1;
    while idx >= 1 {
        let i = idx as usize;
        if ret_str[i] != b'9' {
            ret_str[i] += 1;
            break;
        }
        ret_str[i] = b'0';
        idx -= 1;
    }
    if idx == 0 {
        if int_str[0] == b'9' {
            ret_str[0] = b'1';
            ret_str.push(b'0');
        } else if is_digit(int_str[0]) {
            ret_str[0] += 1;
        } else {
            ret_str[1] = b'1';
            ret_str.push(b'0');
        }
    }
    ret_str
}

/// `maxUintStr`.
const MAX_UINT_STR: &[u8] = b"18446744073709551615";
/// `minIntStr`.
const MIN_INT_STR: &[u8] = b"-9223372036854775808";

/// floatStrToIntStr converts a valid float string into valid integer string which can be parsed by
/// strconv.ParseInt, we can't parse float first then convert it to string because precision will
/// be lost. For example, the string value "18446744073709551615" which is the max number of unsigned
/// int will cause some precision to lose. intStr[0] may be a positive and negative sign like '+' or '-'.
///
/// This func will find serious overflow such as the len of intStr > 20 (without prefix `+/-`)
/// however, it will not check whether the intStr overflow BIGINT.
// go: floatStrToIntStr L1 [M-ERRVAL]
#[allow(clippy::manual_range_contains)] // Go's comparison
fn float_str_to_int_str(valid_float: &[u8], ori_str: &[u8]) -> (Vec<u8>, Result<(), Error>) {
    let mut dot_idx: isize = -1;
    let mut e_idx: isize = -1;
    for (i, &c) in valid_float.iter().enumerate() {
        match c {
            b'.' => dot_idx = i as isize,
            b'e' | b'E' => e_idx = i as isize,
            _ => {}
        }
    }
    let overflow_err = || {
        Err(ErrOverflow
            .gen_with_stack_by_args(vec![Arg::from("BIGINT"), Arg::go_string(ori_str.to_vec())]))
    };
    if e_idx == -1 {
        if dot_idx == -1 {
            return (valid_float.to_vec(), Ok(()));
        }
        let digits: &[u8] = if valid_float[0] == b'-' || valid_float[0] == b'+' {
            dot_idx -= 1;
            &valid_float[1..]
        } else {
            valid_float
        };
        let mut int_str: Vec<u8> = if dot_idx == 0 {
            b"0".to_vec()
        } else {
            digits[..dot_idx as usize].to_vec()
        };
        if digits.len() as isize > dot_idx + 1 {
            int_str = round_int_str(digits[dot_idx as usize + 1], &int_str);
        }
        if (int_str.len() > 1 || int_str[0] != b'0') && valid_float[0] == b'-' {
            int_str.insert(0, b'-');
        }
        return (int_str, Ok(()));
    }
    // intCnt and digits contain the prefix `+/-` if validFloat[0] is `+/-`
    let mut int_cnt: isize;
    let mut digits: Vec<u8> = Vec::with_capacity(valid_float.len());
    let e = e_idx as usize;
    if dot_idx == -1 {
        digits.extend_from_slice(&valid_float[..e]);
        int_cnt = digits.len() as isize;
    } else {
        let d = dot_idx as usize;
        digits.extend_from_slice(&valid_float[..d]);
        int_cnt = digits.len() as isize;
        digits.extend_from_slice(&valid_float[d + 1..e]);
    }
    let (exp, err) = atoi(&valid_float[e + 1..]);
    if err.is_err() {
        let int_str = if digits[0] == b'-' {
            MIN_INT_STR
        } else {
            MAX_UINT_STR
        };
        return (int_str.to_vec(), overflow_err());
    }
    int_cnt = int_cnt.wrapping_add(exp);
    if exp >= 0 && (int_cnt > 21 || int_cnt < 0) {
        // MaxInt64 has 19 decimal digits.
        // MaxUint64 has 20 decimal digits.
        // And the intCnt may contain the len of `+/-`,
        // so I use 21 here as the early detection.
        let int_str = if digits[0] == b'-' {
            MIN_INT_STR
        } else {
            MAX_UINT_STR
        };
        return (int_str.to_vec(), overflow_err());
    }
    if int_cnt <= 0 {
        let mut int_str = b"0".to_vec();
        if int_cnt == 0 && !digits.is_empty() && is_digit(digits[0]) {
            int_str = round_int_str(digits[0], &int_str);
        }
        return (int_str, Ok(()));
    }
    if int_cnt == 1 && (digits[0] == b'-' || digits[0] == b'+') {
        let mut int_str = b"0".to_vec();
        if digits.len() > 1 {
            int_str = round_int_str(digits[1], &int_str);
        }
        if int_str[0] == b'1' {
            let mut s = digits[..1].to_vec();
            s.extend_from_slice(&int_str);
            int_str = s;
        }
        return (int_str, Ok(()));
    }
    let ic = int_cnt as usize;
    let int_str = if ic <= digits.len() {
        let mut int_str = digits[..ic].to_vec();
        if ic < digits.len() {
            int_str = round_int_str(digits[ic], &int_str);
        }
        int_str
    } else {
        // convert scientific notation decimal number
        let extra_zero_count = ic - digits.len();
        let mut int_str = digits.clone();
        int_str.resize(int_str.len() + extra_zero_count, b'0');
        int_str
    };
    (int_str, Ok(()))
}

/// StrToFloat converts a string to a float64 at the best-effort.
// go: StrToFloat L1 [M-ERRVAL]
pub fn str_to_float(ctx: &Context, str: &[u8], is_func_cast: bool) -> (f64, Result<(), Error>) {
    let str = trim_space(str);
    let (valid_str, err) = get_valid_float_prefix(ctx, str, is_func_cast);
    // The valid prefix is ASCII.
    let valid = std::str::from_utf8(&valid_str).expect("ASCII float prefix");
    let (mut f, err1) = parse_float(valid);
    if let Err(e1) = err1 {
        let mut err1: Result<(), Error> = Err(tidb_base::Error::plain(e1.to_string()));
        // value will truncate to MAX/MIN if out of range.
        if e1.err == NumErrKind::Range {
            err1 = ctx.handle_truncate(Err(ErrTruncatedWrongVal
                .gen_with_stack_by_args(vec![Arg::from("DOUBLE"), Arg::go_string(str.to_vec())])));
            if f == f64::INFINITY {
                f = f64::MAX;
            } else if f == f64::NEG_INFINITY {
                f = -f64::MAX;
            }
        }
        return (f, err1);
    }
    (f, err)
}

/// ConvertJSONToInt64 casts JSON into int64.
// go: ConvertJSONToInt64 L1 [M-ERRVAL]
pub fn convert_json_to_int64(
    ctx: &Context,
    j: &BinaryJSON<'_>,
    unsigned: bool,
) -> (i64, Result<(), Error>) {
    convert_json_to_int(ctx, j, unsigned, mysql::TypeLonglong)
}

/// ConvertJSONToInt casts JSON into int by type.
// go: ConvertJSONToInt L1 [M-ERRVAL]
pub fn convert_json_to_int(
    ctx: &Context,
    j: &BinaryJSON<'_>,
    unsigned: bool,
    tp: u8,
) -> (i64, Result<(), Error>) {
    let wrong = || {
        ctx.handle_truncate(Err(ErrTruncatedWrongVal.gen_with_stack_by_args(vec![
            Arg::from("INTEGER"),
            Arg::go_string(j.string()),
        ])))
    };
    match j.type_code {
        JSONTypeCodeObject
        | JSONTypeCodeArray
        | JSONTypeCodeOpaque
        | JSONTypeCodeDate
        | JSONTypeCodeDatetime
        | JSONTypeCodeTimestamp
        | JSONTypeCodeDuration => (0, wrong()),
        JSONTypeCodeLiteral => match j.value[0] {
            JSONLiteralFalse => (0, Ok(())),
            JSONLiteralNil => (0, wrong()),
            _ => (1, Ok(())),
        },
        JSONTypeCodeInt64 => {
            let i = j.get_int64();
            if unsigned {
                let u_bound = integer_unsigned_upper_bound(tp);
                let (u, err) = convert_int_to_uint(ctx.flags(), i, u_bound, tp);
                return (u as i64, err);
            }

            let l_bound = integer_signed_lower_bound(tp);
            let u_bound = integer_signed_upper_bound(tp);
            convert_int_to_int(i, l_bound, u_bound, tp)
        }
        JSONTypeCodeUint64 => {
            let u = j.get_uint64();
            if unsigned {
                let u_bound = integer_unsigned_upper_bound(tp);
                let (u, err) = convert_uint_to_uint(u, u_bound, tp);
                return (u as i64, err);
            }

            let u_bound = integer_signed_upper_bound(tp);
            convert_uint_to_int(u, u_bound, tp)
        }
        JSONTypeCodeFloat64 => {
            let f = j.get_float64();
            if !unsigned {
                let l_bound = integer_signed_lower_bound(tp);
                let u_bound = integer_signed_upper_bound(tp);
                return convert_float_to_int(f, l_bound, u_bound, tp);
            }
            let bound = integer_unsigned_upper_bound(tp);
            let (u, err) = convert_float_to_uint(ctx.flags(), f, bound, tp);
            (u as i64, err)
        }
        JSONTypeCodeString => {
            let str = j.get_string();
            // The behavior of casting json string as an integer is consistent with casting a string as an integer.
            // See the `builtinCastStringAsIntSig` in `expression` pkg. The only difference is that this function
            // doesn't append any warning. This behavior is compatible with MySQL.
            let is_negative = str.len() > 1 && str[0] == b'-';
            if !is_negative {
                let (r, err) = str_to_uint(ctx, str, false);
                return (r as i64, err);
            }

            str_to_int(ctx, str, false)
        }
        _ => (0, Err(tidb_base::errors::new("Unknown type code in JSON"))),
    }
}

/// ConvertJSONToFloat casts JSON into float64.
// go: ConvertJSONToFloat L1 [M-ERRVAL]
pub fn convert_json_to_float(ctx: &Context, j: &BinaryJSON<'_>) -> (f64, Result<(), Error>) {
    let wrong = || {
        ctx.handle_truncate(Err(ErrTruncatedWrongVal.gen_with_stack_by_args(vec![
            Arg::from("FLOAT"),
            Arg::go_string(j.string()),
        ])))
    };
    match j.type_code {
        JSONTypeCodeObject
        | JSONTypeCodeArray
        | JSONTypeCodeOpaque
        | JSONTypeCodeDate
        | JSONTypeCodeDatetime
        | JSONTypeCodeTimestamp
        | JSONTypeCodeDuration => (0.0, wrong()),
        JSONTypeCodeLiteral => match j.value[0] {
            JSONLiteralFalse => (0.0, Ok(())),
            JSONLiteralNil => (0.0, wrong()),
            _ => (1.0, Ok(())),
        },
        JSONTypeCodeInt64 => (j.get_int64() as f64, Ok(())),
        JSONTypeCodeUint64 => (j.get_uint64() as f64, Ok(())),
        JSONTypeCodeFloat64 => (j.get_float64(), Ok(())),
        JSONTypeCodeString => str_to_float(ctx, j.get_string(), false),
        _ => (
            0.0,
            Err(tidb_base::errors::new("Unknown type code in JSON")),
        ),
    }
}

/// ConvertJSONToDecimal casts JSON into decimal.
// go: ConvertJSONToDecimal L1 [M-ERRVAL]
pub fn convert_json_to_decimal(
    ctx: &Context,
    j: &BinaryJSON<'_>,
) -> (MyDecimal, Result<(), Error>) {
    let mut err: Result<(), Error> = Ok(());
    let mut res = MyDecimal::default();
    let wrong = || {
        Err(ErrTruncatedWrongVal
            .gen_with_stack_by_args(vec![Arg::from("DECIMAL"), Arg::go_string(j.string())]))
    };
    match j.type_code {
        JSONTypeCodeObject
        | JSONTypeCodeArray
        | JSONTypeCodeOpaque
        | JSONTypeCodeDate
        | JSONTypeCodeDatetime
        | JSONTypeCodeTimestamp
        | JSONTypeCodeDuration => err = wrong(),
        JSONTypeCodeLiteral => match j.value[0] {
            JSONLiteralFalse => {
                res.from_int(0);
            }
            JSONLiteralNil => err = wrong(),
            _ => {
                res.from_int(1);
            }
        },
        JSONTypeCodeInt64 => {
            res.from_int(j.get_int64());
        }
        JSONTypeCodeUint64 => {
            res.from_uint(j.get_uint64());
        }
        JSONTypeCodeFloat64 => err = res.from_float64(j.get_float64()),
        JSONTypeCodeString => err = res.from_string(j.get_string()),
        _ => {}
    }
    let err = ctx.handle_truncate(err);
    (res, err)
}

/// getValidFloatPrefix gets prefix of string which can be successfully parsed as float.
// go: getValidFloatPrefix L1 [M-ERRVAL]
fn get_valid_float_prefix(
    ctx: &Context,
    s: &[u8],
    is_func_cast: bool,
) -> (Vec<u8>, Result<(), Error>) {
    if is_func_cast && s.is_empty() {
        return (b"0".to_vec(), Ok(()));
    }

    let mut s = s;
    let mut saw_dot = false;
    let mut saw_digit = false;
    let mut valid_len = 0usize;
    let mut e_idx: isize = -1;
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if c == b'+' || c == b'-' {
            if i != 0 && i as isize != e_idx + 1 {
                // "1e+1" is valid.
                break;
            }
        } else if c == b'.' {
            if saw_dot || e_idx > 0 {
                // "1.1." or "1e1.1"
                break;
            }
            saw_dot = true;
            if saw_digit {
                // "123." is valid.
                valid_len = i + 1;
            }
        } else if c == b'e' || c == b'E' {
            if !saw_digit {
                // "+.e"
                break;
            }
            if e_idx != -1 {
                // "1e5e"
                break;
            }
            e_idx = i as isize;
        } else if c == 0 {
            s = &s[..valid_len];
            break;
        } else if !c.is_ascii_digit() {
            break;
        } else {
            saw_digit = true;
            valid_len = i + 1;
        }
        i += 1;
    }
    let mut valid = s[..valid_len].to_vec();
    if valid.is_empty() {
        valid = b"0".to_vec();
    }
    let mut err = Ok(());
    if valid_len == 0 || valid_len != s.len() {
        err = ctx.handle_truncate(Err(ErrTruncatedWrongVal
            .gen_with_stack_by_args(vec![Arg::from("DOUBLE"), Arg::go_string(s.to_vec())])));
    }
    (valid, err)
}
