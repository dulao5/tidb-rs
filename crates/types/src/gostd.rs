//! Go standard-library behaviour that `mydecimal.go` depends on and that Rust's std does not
//! reproduce exactly (ON-DEMAND tool code, Go 1.25.12 as recorded in the govec vectors).
//!
//! go-file: $GOROOT/src/strings/strings.go
//! go-file: $GOROOT/src/unicode/utf8/utf8.go
//! go-file: $GOROOT/src/strconv/ftoa.go
//! go-file: $GOROOT/src/strconv/atof.go
//! go-file: $GOROOT/src/strconv/atoi.go
//! go-file: $GOROOT/src/encoding/json/decode.go
//!
//! Only the logic is ported; the shortest round-trip digits of a float come from the ryu
//! crate (the same Ryū algorithm as Go's `ftoaryu.go`), and JSON syntax comes from
//! serde_json.

mod eisel_lemire;

use serde::de::{self, Deserializer, IgnoredAny, MapAccess, Visitor};
use serde_json::Value;
use std::fmt;

/// `utf8.DecodeRune`: the rune at the start of `s` and its width. An empty or invalid
/// sequence is `(None, 1)` (Go: `RuneError, 1`; `RuneError, 0` for empty input).
// go-std: unicode/utf8.DecodeRune L2 (std::str::from_utf8 does the validation)
pub(crate) fn decode_rune(s: &[u8]) -> (Option<char>, usize) {
    if s.is_empty() {
        return (None, 0);
    }
    let want = match s[0] {
        0x00..=0x7f => return (Some(char::from(s[0])), 1),
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return (None, 1),
    };
    if s.len() >= want
        && let Ok(t) = std::str::from_utf8(&s[..want])
    {
        return (t.chars().next(), want);
    }
    (None, 1)
}

/// `utf8.DecodeLastRune`: the rune at the end of `s` and its width; `(None, 1)` if invalid.
// go-std: unicode/utf8.DecodeLastRune L2 (std::str::from_utf8 does the validation)
fn decode_last_rune(s: &[u8]) -> (Option<char>, usize) {
    let end = s.len();
    if end == 0 {
        return (None, 0);
    }
    for w in 1..=4.min(end) {
        if let Ok(t) = std::str::from_utf8(&s[end - w..]) {
            let mut it = t.chars();
            if let (Some(c), None) = (it.next(), it.next()) {
                return (Some(c), w);
            }
        }
    }
    (None, 1)
}

/// `unicode.IsSpace`. Go's set ('\t', '\n', '\v', '\f', '\r', ' ', U+0085, U+00A0 and the
/// Unicode White_Space property above Latin-1) is exactly Rust's `char::is_whitespace`.
// go-std: unicode.IsSpace L1
fn unicode_is_space(r: Option<char>) -> bool {
    r.is_some_and(char::is_whitespace)
}

/// `strings.TrimSpace` on bytes (Go converts `[]byte` to `string` without validation).
// go-std: strings.TrimSpace L1
pub(crate) fn trim_space(s: &[u8]) -> &[u8] {
    let mut start = 0;
    while start < s.len() {
        let (r, w) = decode_rune(&s[start..]);
        if !unicode_is_space(r) {
            break;
        }
        start += w;
    }
    let mut stop = s.len();
    while stop > start {
        let (r, w) = decode_last_rune(&s[start..stop]);
        if !unicode_is_space(r) {
            break;
        }
        stop -= w;
    }
    &s[start..stop]
}

/// `strconv.FormatFloat(f, 'g', -1, 64)`.
// go-std: strconv.FormatFloat L2 (only fmt 'g', prec -1, bitSize 64; shortest digits from the ryu crate)
pub fn format_float_g(f: f64) -> String {
    if f.is_nan() {
        return "NaN".to_string();
    }
    if f.is_infinite() {
        return if f.is_sign_negative() { "-Inf" } else { "+Inf" }.to_string();
    }
    let neg = f.is_sign_negative();
    let digs = ryu_ftoa_shortest(f.abs());
    let mut dst = Vec::with_capacity(24);
    // FormatFloat(.., 'g', -1, ..): shortest, prec = digs.nd.
    let prec = digs.nd;
    format_digits(&mut dst, true, neg, &digs, prec, b'g');
    String::from_utf8(dst).expect("formatted float is ASCII")
}

/// `ryuFtoaShortest`: the shortest decimal digits that parse back to `f` (finite, `f >= 0`)
/// and the decimal point position, without trailing zeros; zero is `nd = 0, dp = 0`.
///
/// Go's `ryuFtoaShortest` and the ryu crate are the same algorithm (Ryū, Adams 2018),
/// including the round-half-even choice between two shortest candidates (e.g.
/// 979980073090831.25 is "9799800730908312", where Rust's `{:e}` gives "...313"). The crate
/// only exposes the formatted string, so its digits and exponent are read back from it.
// go-std: strconv.ryuFtoaShortest L2 (ryu crate)
fn ryu_ftoa_shortest(f: f64) -> DecimalSlice {
    let mut digs = DecimalSlice {
        d: Vec::with_capacity(17),
        nd: 0,
        dp: 0,
    };
    if f == 0.0 {
        return digs;
    }
    let mut buf = ryu::Buffer::new();
    // "1234.5", "0.00012345", "12340000000.0", "1e30", "1.234e-7".
    let s = buf.format_finite(f).as_bytes();
    let (mant, exp) = match s.iter().position(|&b| b == b'e') {
        Some(i) => (&s[..i], parse_exp(&s[i + 1..])),
        None => (s, 0),
    };
    let mut point = None;
    for &b in mant {
        if b == b'.' {
            point = Some(digs.d.len() as isize);
        } else if b == b'0' && digs.d.is_empty() {
            // Leading zeros of "0.000123" only move the decimal point.
            point = point.map(|p| p - 1);
        } else {
            digs.d.push(b);
        }
    }
    while digs.d.last() == Some(&b'0') {
        digs.d.pop();
    }
    digs.nd = digs.d.len() as isize;
    digs.dp = point.unwrap_or(mant.len() as isize) + exp;
    digs
}

/// The exponent of ryu's scientific form: `-?[0-9]{1,3}`.
// glue: reads the exponent of ryu's scientific output (Go's ryuFtoaShortest fills a decimalSlice directly)
fn parse_exp(e: &[u8]) -> isize {
    let (neg, e) = match e.split_first() {
        Some((b'-', rest)) => (true, rest),
        _ => (false, e),
    };
    let v = e
        .iter()
        .fold(0isize, |v, &b| v * 10 + isize::from(b - b'0'));
    if neg { -v } else { v }
}

struct DecimalSlice {
    d: Vec<u8>,
    nd: isize,
    dp: isize,
}

// go-std: strconv.formatDigits L1
fn format_digits(
    dst: &mut Vec<u8>,
    shortest: bool,
    neg: bool,
    digs: &DecimalSlice,
    mut prec: isize,
    fmt: u8,
) {
    match fmt {
        b'e' | b'E' => fmt_e(dst, neg, digs, prec, fmt),
        b'f' => fmt_f(dst, neg, digs, prec),
        b'g' | b'G' => {
            // trailing fractional zeros in 'e' form will be trimmed.
            let mut eprec = prec;
            if eprec > digs.nd && digs.nd >= digs.dp {
                eprec = digs.nd;
            }
            // %e is used if the exponent from the conversion
            // is less than -4 or greater than or equal to the precision.
            // if precision was the shortest possible, use precision 6 for this decision.
            if shortest {
                eprec = 6;
            }
            let exp = digs.dp - 1;
            if exp < -4 || exp >= eprec {
                if prec > digs.nd {
                    prec = digs.nd;
                }
                return fmt_e(dst, neg, digs, prec - 1, fmt + b'e' - b'g');
            }
            if prec > digs.dp {
                prec = digs.nd;
            }
            fmt_f(dst, neg, digs, (prec - digs.dp).max(0))
        }
        _ => {
            // unknown format
            dst.push(b'%');
            dst.push(fmt);
        }
    }
}

// go-std: strconv.fmtE L1
fn fmt_e(dst: &mut Vec<u8>, neg: bool, d: &DecimalSlice, prec: isize, fmt: u8) {
    // sign
    if neg {
        dst.push(b'-');
    }

    // first digit
    let mut ch = b'0';
    if d.nd != 0 {
        ch = d.d[0];
    }
    dst.push(ch);

    // .moredigits
    if prec > 0 {
        dst.push(b'.');
        let mut i = 1;
        let m = d.nd.min(prec + 1);
        if i < m {
            dst.extend_from_slice(&d.d[i as usize..m as usize]);
            i = m;
        }
        while i <= prec {
            dst.push(b'0');
            i += 1;
        }
    }

    // e±
    dst.push(fmt);
    let mut exp = d.dp - 1;
    if d.nd == 0 {
        // special case: 0 has exponent 0
        exp = 0;
    }
    if exp < 0 {
        ch = b'-';
        exp = -exp;
    } else {
        ch = b'+';
    }
    dst.push(ch);

    // dd or ddd
    if exp < 10 {
        dst.extend_from_slice(&[b'0', exp as u8 + b'0']);
    } else if exp < 100 {
        dst.extend_from_slice(&[(exp / 10) as u8 + b'0', (exp % 10) as u8 + b'0']);
    } else {
        dst.extend_from_slice(&[
            (exp / 100) as u8 + b'0',
            ((exp / 10) as u8) % 10 + b'0',
            (exp % 10) as u8 + b'0',
        ]);
    }
}

// go-std: strconv.fmtF L1
fn fmt_f(dst: &mut Vec<u8>, neg: bool, d: &DecimalSlice, prec: isize) {
    // sign
    if neg {
        dst.push(b'-');
    }

    // integer, padded with zeros as needed.
    if d.dp > 0 {
        let mut m = d.nd.min(d.dp);
        dst.extend_from_slice(&d.d[..m as usize]);
        while m < d.dp {
            dst.push(b'0');
            m += 1;
        }
    } else {
        dst.push(b'0');
    }

    // fraction
    if prec > 0 {
        dst.push(b'.');
        for i in 0..prec {
            let mut ch = b'0';
            let j = d.dp + i;
            if 0 <= j && j < d.nd {
                ch = d.d[j as usize];
            }
            dst.push(ch);
        }
    }
}

/// `strconv.ErrSyntax` / `strconv.ErrRange`: the `Err` field of a `*strconv.NumError`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NumErrKind {
    /// `strconv.ErrSyntax`: "invalid syntax".
    Syntax,
    /// `strconv.ErrRange`: "value out of range".
    Range,
}

/// `*strconv.NumError` as `ParseFloat` returns it. Callers test `err` the way Go tests
/// `err.(*strconv.NumError).Err == strconv.ErrRange` (or `errors.Is`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NumError {
    /// The failing function (`"ParseFloat"`).
    pub func: &'static str,
    /// The input.
    pub num: String,
    /// Why it failed.
    pub err: NumErrKind,
}

impl fmt::Display for NumError {
    // go-std: strconv.NumError.Error L2 (strconv.Quote simplified: ASCII escapes only)
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let why = match self.err {
            NumErrKind::Syntax => "invalid syntax",
            NumErrKind::Range => "value out of range",
        };
        write!(f, "strconv.{}: parsing \"", self.func)?;
        for c in self.num.chars() {
            match c {
                '"' => f.write_str("\\\"")?,
                '\\' => f.write_str("\\\\")?,
                '\n' => f.write_str("\\n")?,
                '\t' => f.write_str("\\t")?,
                '\r' => f.write_str("\\r")?,
                c if (c as u32) < 0x20 || c as u32 == 0x7f => write!(f, "\\x{:02x}", c as u32)?,
                c => write!(f, "{c}")?,
            }
        }
        write!(f, "\": {why}")
    }
}

impl std::error::Error for NumError {}

// go-std: strconv.syntaxError L2 (also strconv.rangeError: one constructor, the kind is an argument)
fn num_error(s: &[u8], err: NumErrKind) -> NumError {
    NumError {
        func: "ParseFloat",
        num: String::from_utf8_lossy(s).into_owned(),
        err,
    }
}

/// `strconv.ParseFloat(s, 64)`, Go-shaped (`M-ERRVAL`): the value is meaningful together
/// with the error. Overflow is `±Inf` with an `ErrRange` error; a syntax error is `0`.
///
/// Go's syntax is kept: `inf`/`infinity`/`nan` in any case (NaN without a sign), `_` digit
/// separators where `underscoreOK` allows them, hexadecimal mantissas with a mandatory `p`
/// exponent (`0x1p-2`), exponents clamped at 10000 while reading. Decimal values take Go's
/// path: `atof64exact`, then `eiselLemire64`, then the slow path, where Rust's correctly
/// rounded `str::parse::<f64>` stands in for Go's `decimal` conversion (also correctly
/// rounded) on the digits and decimal point Go's `decimal.set` uses; hexadecimal values are
/// Go's `atofHex`.
// go-std: strconv.ParseFloat L2 (bitSize 64 only; the decimal slow path converts with Rust's str::parse)
pub fn parse_float(s: &str) -> (f64, Result<(), NumError>) {
    let s = s.as_bytes();
    let (f, n, err) = atof64(s);
    let not_syntax = match &err {
        Ok(()) => true,
        Err(e) => e.err != NumErrKind::Syntax,
    };
    if n != s.len() && not_syntax {
        return (0.0, Err(num_error(s, NumErrKind::Syntax)));
    }
    (f, err)
}

// go-std: strconv.atof64 L2 (the decimal slow path converts with Rust's str::parse)
fn atof64(s: &[u8]) -> (f64, usize, Result<(), NumError>) {
    if let Some((val, n)) = special(s) {
        return (val, n, Ok(()));
    }

    let r = read_float(s);
    if !r.ok {
        return (0.0, r.i, Err(num_error(s, NumErrKind::Syntax)));
    }

    if r.hex {
        let (f, err) = atof_hex(&s[..r.i], r.mantissa, r.exp, r.neg, r.trunc);
        return (f, r.i, err);
    }

    // Go: try pure floating-point arithmetic conversion, and if that fails,
    // the Eisel-Lemire algorithm.
    if !r.trunc
        && let Some(f) = atof64exact(r.mantissa, r.exp, r.neg)
    {
        return (f, r.i, Ok(()));
    }
    if let Some(f) = eisel_lemire::eisel_lemire64(r.mantissa, r.exp, r.neg) {
        if !r.trunc {
            return (f, r.i, Ok(()));
        }
        // Even if the mantissa was truncated, we may
        // have found the correct result. Confirm by
        // converting the upper mantissa bound.
        if eisel_lemire::eisel_lemire64(r.mantissa + 1, r.exp, r.neg) == Some(f) {
            return (f, r.i, Ok(()));
        }
    }

    // Slow fallback. Go's decimal.set + floatBits round correctly; Rust's parser does the
    // same on the digits and the decimal point position decimal.set computes. That position
    // counts at most 800 digits (len(decimal.d)), so an integer part longer than 800 digits
    // loses the excess: ParseFloat("1" + 1000 zeros + "e-500") is 1e299 in Go, not +Inf.
    let dp = r.dp_slow;
    let f = if !r.underscores && !r.exp_clamped && dp == r.dp {
        // s[:n] is plain decimal syntax that Rust's parser reads as Go does.
        std::str::from_utf8(&s[..r.i])
            .expect("ASCII prefix of a str")
            .parse::<f64>()
            .expect("readFloat validated the syntax")
    } else {
        // Significant digits (no sign, point, '_' or leading zeros) and the exponent.
        let mut text = String::with_capacity(r.i + 24);
        if r.neg {
            text.push('-');
        }
        let mut nd: i64 = 0;
        for &c in &s[..r.mant_end] {
            if c.is_ascii_digit() && (c != b'0' || nd > 0) {
                text.push(char::from(c));
                nd += 1;
            }
        }
        if nd == 0 {
            text.push('0');
        } else {
            text.push('e');
            text.push_str(&(dp - nd).to_string());
        }
        text.parse::<f64>().expect("digits and exponent")
    };
    if f.is_infinite() {
        // Go: decimal.floatBits overflow -> ±Inf and ErrRange.
        return (f, r.i, Err(num_error(s, NumErrKind::Range)));
    }
    (f, r.i, Ok(()))
}

// go-std: strconv.atof64exact L1
fn atof64exact(mantissa: u64, mut exp: i64, neg: bool) -> Option<f64> {
    if mantissa >> 52 != 0 {
        return None;
    }
    let mut f = mantissa as f64;
    if neg {
        f = -f;
    }
    match exp {
        // an integer.
        0 => Some(f),
        // Exact integers are <= 10^15.
        // Exact powers of ten are <= 10^22.
        1..=37 => {
            // int * 10^k
            // If exponent is big but number of digits is not,
            // can move a few zeros into the integer part.
            if exp > 22 {
                f *= FLOAT64_POW10[(exp - 22) as usize];
                exp = 22;
            }
            if f > 1e15 || f < -1e15 {
                // the exponent was really too large.
                return None;
            }
            Some(f * FLOAT64_POW10[exp as usize])
        }
        // int / 10^k
        -22..=-1 => Some(f / FLOAT64_POW10[(-exp) as usize]),
        _ => None,
    }
}

// Exact powers of 10.
const FLOAT64_POW10: [f64; 23] = [
    1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16,
    1e17, 1e18, 1e19, 1e20, 1e21, 1e22,
];

/// `strconv.special`: a signed infinity or an unsigned NaN at the start of `s`, and its length.
// go-std: strconv.special L1
fn special(s: &[u8]) -> Option<(f64, usize)> {
    if s.is_empty() {
        return None;
    }
    let mut sign = 1.0;
    let mut nsign = 0;
    let mut t = s;
    match s[0] {
        b'+' | b'-' | b'i' | b'I' => {
            if s[0] == b'+' || s[0] == b'-' {
                if s[0] == b'-' {
                    sign = -1.0;
                }
                nsign = 1;
                t = &s[1..];
            }
            let mut n = common_prefix_len_ignore_case(t, b"infinity");
            // Anything longer than "inf" is ok, but if we
            // don't have "infinity", only consume "inf".
            if 3 < n && n < 8 {
                n = 3;
            }
            if n == 3 || n == 8 {
                return Some((f64::INFINITY * sign, nsign + n));
            }
        }
        b'n' | b'N' if common_prefix_len_ignore_case(t, b"nan") == 3 => {
            // Go's math.NaN() is 0x7FF8000000000001.
            return Some((f64::from_bits(0x7FF8_0000_0000_0001), 3));
        }
        _ => {}
    }
    None
}

// go-std: strconv.commonPrefixLenIgnoreCase L1
fn common_prefix_len_ignore_case(s: &[u8], prefix: &[u8]) -> usize {
    let n = prefix.len().min(s.len());
    for i in 0..n {
        let c = s[i].to_ascii_lowercase();
        if c != prefix[i] {
            return i;
        }
    }
    n
}

/// Result of `readFloat`; the fields after `ok` are kept for the decimal conversion (Go hands
/// `s[:n]` to its own converters instead).
struct ReadFloat {
    mantissa: u64,
    exp: i64,
    neg: bool,
    trunc: bool,
    hex: bool,
    i: usize,
    ok: bool,
    /// End of the mantissa (start of the exponent) in `s`.
    mant_end: usize,
    underscores: bool,
    /// An exponent digit was dropped by Go's `e < 10000` clamp.
    exp_clamped: bool,
    dp: i64,
    /// The decimal point as Go's slow path (`decimal.set`) computes it: it counts at most
    /// 800 digits (`len(decimal.d)`) when it places the point, see `atof64`.
    dp_slow: i64,
}

// go-std: strconv.readFloat L2 (also reports what atof64's decimal conversion needs)
fn read_float(s: &[u8]) -> ReadFloat {
    let mut r = ReadFloat {
        mantissa: 0,
        exp: 0,
        neg: false,
        trunc: false,
        hex: false,
        i: 0,
        ok: false,
        mant_end: 0,
        underscores: false,
        exp_clamped: false,
        dp: 0,
        dp_slow: 0,
    };
    // decimal.set's digit count, capped at len(decimal.d) = 800.
    let mut nd_slow: i64 = 0;
    let mut underscores = false;
    let mut i = 0;

    // optional sign
    if i >= s.len() {
        return r;
    }
    match s[i] {
        b'+' => i += 1,
        b'-' => {
            i += 1;
            r.neg = true;
        }
        _ => {}
    }

    // digits
    let mut base: u64 = 10;
    let mut max_mant_digits = 19; // 10^19 fits in uint64
    let mut exp_char = b'e';
    if i + 2 < s.len() && s[i] == b'0' && s[i + 1].eq_ignore_ascii_case(&b'x') {
        base = 16;
        max_mant_digits = 16; // 16^16 fits in uint64
        i += 2;
        exp_char = b'p';
        r.hex = true;
    }
    let mut sawdot = false;
    let mut sawdigits = false;
    let mut nd: i64 = 0;
    let mut nd_mant: i64 = 0;
    let mut dp: i64 = 0;
    while i < s.len() {
        let c = s[i];
        if c == b'_' {
            underscores = true;
        } else if c == b'.' {
            if sawdot {
                break;
            }
            sawdot = true;
            dp = nd;
            r.dp_slow = nd_slow;
        } else if c.is_ascii_digit() {
            sawdigits = true;
            if c == b'0' && nd == 0 {
                // ignore leading zeros
                dp -= 1;
                r.dp_slow -= 1;
                i += 1;
                continue;
            }
            nd += 1;
            if nd_slow < 800 {
                nd_slow += 1;
            }
            if nd_mant < max_mant_digits {
                r.mantissa *= base;
                r.mantissa += u64::from(c - b'0');
                nd_mant += 1;
            } else if c != b'0' {
                r.trunc = true;
            }
        } else if base == 16 && (b'a'..=b'f').contains(&c.to_ascii_lowercase()) {
            sawdigits = true;
            nd += 1;
            if nd_mant < max_mant_digits {
                r.mantissa *= 16;
                r.mantissa += u64::from(c.to_ascii_lowercase() - b'a' + 10);
                nd_mant += 1;
            } else {
                r.trunc = true;
            }
        } else {
            break;
        }
        i += 1;
    }
    r.i = i;
    r.mant_end = i;
    if !sawdigits {
        return r;
    }
    if !sawdot {
        dp = nd;
        r.dp_slow = nd_slow;
    }

    if base == 16 {
        dp *= 4;
        nd_mant *= 4;
    }

    // optional exponent moves decimal point.
    // if we read a very large, very long number,
    // just be sure to move the decimal point by
    // a lot (say, 100000).  it doesn't matter if it's
    // not the exact number.
    if i < s.len() && s[i].to_ascii_lowercase() == exp_char {
        i += 1;
        r.i = i;
        if i >= s.len() {
            return r;
        }
        let mut esign = 1;
        match s[i] {
            b'+' => i += 1,
            b'-' => {
                i += 1;
                esign = -1;
            }
            _ => {}
        }
        r.i = i;
        if i >= s.len() || !s[i].is_ascii_digit() {
            return r;
        }
        let mut e: i64 = 0;
        while i < s.len() && (s[i].is_ascii_digit() || s[i] == b'_') {
            if s[i] == b'_' {
                underscores = true;
                i += 1;
                continue;
            }
            if e < 10000 {
                e = e * 10 + i64::from(s[i] - b'0');
            } else {
                r.exp_clamped = true;
            }
            i += 1;
        }
        r.i = i;
        dp += e * esign;
        r.dp_slow += e * esign;
    } else if base == 16 {
        // Must have exponent.
        return r;
    }

    if r.mantissa != 0 {
        r.exp = dp - nd_mant;
    }

    if underscores && !underscore_ok(&s[..i]) {
        return r;
    }
    r.underscores = underscores;

    r.dp = dp;
    r.ok = true;
    r
}

// go-std: strconv.underscoreOK L1
fn underscore_ok(s: &[u8]) -> bool {
    // saw tracks the last character (class) we saw:
    // ^ for beginning of number,
    // 0 for a digit or base prefix,
    // _ for an underscore,
    // ! for none of the above.
    let mut saw = b'^';
    let mut i = 0;
    let mut s = s;

    // Optional sign.
    if !s.is_empty() && (s[0] == b'-' || s[0] == b'+') {
        s = &s[1..];
    }

    // Optional base prefix.
    let mut hex = false;
    if s.len() >= 2 && s[0] == b'0' && matches!(s[1].to_ascii_lowercase(), b'b' | b'o' | b'x') {
        i = 2;
        saw = b'0'; // base prefix counts as a digit for "underscore as digit separator"
        hex = s[1].eq_ignore_ascii_case(&b'x');
    }

    // Number proper.
    while i < s.len() {
        // Digits are always okay.
        if s[i].is_ascii_digit() || hex && (b'a'..=b'f').contains(&s[i].to_ascii_lowercase()) {
            saw = b'0';
            i += 1;
            continue;
        }
        // Underscore must follow digit.
        if s[i] == b'_' {
            if saw != b'0' {
                return false;
            }
            saw = b'_';
            i += 1;
            continue;
        }
        // Underscore must also be followed by digit.
        if saw == b'_' {
            return false;
        }
        // Saw non-digit, non-underscore.
        saw = b'!';
        i += 1;
    }
    saw != b'_'
}

// go-std: strconv.atofHex L1 (float64info only)
fn atof_hex(
    s: &[u8],
    mut mantissa: u64,
    mut exp: i64,
    neg: bool,
    trunc: bool,
) -> (f64, Result<(), NumError>) {
    const MANTBITS: u32 = 52;
    const EXPBITS: u32 = 11;
    const BIAS: i64 = -1023;
    let max_exp = (1i64 << EXPBITS) + BIAS - 2;
    let min_exp = BIAS + 1;
    exp += i64::from(MANTBITS); // mantissa now implicitly divided by 2^mantbits.

    // Shift mantissa and exponent to bring representation into float range.
    // Eventually we want a mantissa with a leading 1-bit followed by mantbits other bits.
    // For rounding, we need two more, where the bottom bit represents
    // whether that bit or any later bit was non-zero.
    // (If the mantissa has already lost non-zero bits, trunc is true,
    // and we OR in a 1 below after shifting left appropriately.)
    while mantissa != 0 && mantissa >> (MANTBITS + 2) == 0 {
        mantissa <<= 1;
        exp -= 1;
    }
    if trunc {
        mantissa |= 1;
    }
    while mantissa >> (1 + MANTBITS + 2) != 0 {
        mantissa = mantissa >> 1 | mantissa & 1;
        exp += 1;
    }

    // If exponent is too negative,
    // denormalize in hopes of making it representable.
    // (The -2 is for the rounding bits.)
    while mantissa > 1 && exp < min_exp - 2 {
        mantissa = mantissa >> 1 | mantissa & 1;
        exp += 1;
    }

    // Round using two bottom bits.
    let mut round = mantissa & 3;
    mantissa >>= 2;
    round |= mantissa & 1; // round to even (round up if mantissa is odd)
    exp += 2;
    if round == 3 {
        mantissa += 1;
        if mantissa == 1 << (1 + MANTBITS) {
            mantissa >>= 1;
            exp += 1;
        }
    }

    if mantissa >> MANTBITS == 0 {
        // Denormal or zero.
        exp = BIAS;
    }
    let mut err = Ok(());
    if exp > max_exp {
        // infinity and range error
        mantissa = 1 << MANTBITS;
        exp = max_exp + 1;
        err = Err(num_error(s, NumErrKind::Range));
    }

    let mut bits = mantissa & ((1 << MANTBITS) - 1);
    bits |= (((exp - BIAS) & ((1 << EXPBITS) - 1)) as u64) << MANTBITS;
    if neg {
        bits |= 1 << MANTBITS << EXPBITS;
    }
    (f64::from_bits(bits), err)
}

/// The fields of Go's `jsonMyDecimal` as `encoding/json.Unmarshal` fills them.
#[derive(Default)]
pub(crate) struct JsonMyDecimal {
    pub(crate) digits_int: i8,
    pub(crate) digits_frac: i8,
    pub(crate) result_frac: i8,
    pub(crate) negative: bool,
    pub(crate) word_buf: [i32; 9],
}

/// `json.Unmarshal(data, &r)` with `r` a zero `jsonMyDecimal`.
///
/// Go semantics kept: a syntax error wins over type errors; `null` leaves a value unchanged;
/// keys match field names exactly or case-insensitively, last one wins, unknown keys are
/// ignored; integers must be integer literals that fit the field type; a short `WordBuf`
/// array zeroes the remaining elements and extra elements are ignored; a type mismatch is
/// remembered and reported after the whole input is decoded.
/// Not kept (documented in pilot-notes): Unicode case folding of keys beyond ASCII, the
/// literal `-0` (serde_json reports it as a float), invalid UTF-8 inside strings, and the
/// wording of Go's error messages.
// go-std: encoding/json.Unmarshal L2 (serde_json for syntax, Go's struct decoding rules for jsonMyDecimal)
pub(crate) fn json_unmarshal_my_decimal(data: &[u8]) -> Result<JsonMyDecimal, String> {
    let mut de = serde_json::Deserializer::from_slice(data);
    let decoded = TopLevel
        .deserialize(&mut de)
        .and_then(|r| de.end().map(|()| r))
        .map_err(|e| e.to_string())?;
    match decoded {
        Decoded::Ok(r) => Ok(r),
        Decoded::TypeError(msg) => Err(msg),
    }
}

enum Decoded {
    Ok(JsonMyDecimal),
    TypeError(String),
}

struct TopLevel;

impl TopLevel {
    // go-part: encoding/json.Unmarshal
    fn deserialize<'de, D: Deserializer<'de>>(self, d: D) -> Result<Decoded, D::Error> {
        d.deserialize_any(self)
    }
}

const TYPE_NAME: &str = "types.jsonMyDecimal";

impl<'de> Visitor<'de> for TopLevel {
    type Value = Decoded;

    // glue: required by serde's Visitor; never reached for valid JSON
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }

    // go-std: encoding/json.decodeState.literalStore L2 (serde visitor: null into a struct is a no-op)
    fn visit_unit<E: de::Error>(self) -> Result<Decoded, E> {
        Ok(Decoded::Ok(JsonMyDecimal::default()))
    }
    // go-std: encoding/json.decodeState.literalStore L2 (serde visitor: a bool literal into a struct is an UnmarshalTypeError)
    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Decoded, E> {
        Ok(type_error("bool"))
    }
    // go-std: encoding/json.decodeState.literalStore L2 (serde visitor: a i64 literal into a struct is an UnmarshalTypeError)
    fn visit_i64<E: de::Error>(self, _: i64) -> Result<Decoded, E> {
        Ok(type_error("number"))
    }
    // go-std: encoding/json.decodeState.literalStore L2 (serde visitor: a u64 literal into a struct is an UnmarshalTypeError)
    fn visit_u64<E: de::Error>(self, _: u64) -> Result<Decoded, E> {
        Ok(type_error("number"))
    }
    // go-std: encoding/json.decodeState.literalStore L2 (serde visitor: a f64 literal into a struct is an UnmarshalTypeError)
    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Decoded, E> {
        Ok(type_error("number"))
    }
    // go-std: encoding/json.decodeState.literalStore L2 (serde visitor: a str literal into a struct is an UnmarshalTypeError)
    fn visit_str<E: de::Error>(self, _: &str) -> Result<Decoded, E> {
        Ok(type_error("string"))
    }
    // go-std: encoding/json.decodeState.array L2 (serde visitor: an array into a struct is an UnmarshalTypeError)
    fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Decoded, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(type_error("array"))
    }
    // go-std: encoding/json.decodeState.object L2 (serde visitor over the fields of jsonMyDecimal; key match exact, then ASCII case-insensitive)
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Decoded, A::Error> {
        let mut r = JsonMyDecimal::default();
        let mut first_err: Option<String> = None;
        while let Some(key) = map.next_key::<String>()? {
            let v: Value = map.next_value()?;
            let field = [
                "DigitsInt",
                "DigitsFrac",
                "ResultFrac",
                "Negative",
                "WordBuf",
            ]
            .iter()
            .position(|f| *f == key)
            .or_else(|| {
                [
                    "DigitsInt",
                    "DigitsFrac",
                    "ResultFrac",
                    "Negative",
                    "WordBuf",
                ]
                .iter()
                .position(|f| f.eq_ignore_ascii_case(&key))
            });
            let res = match field {
                Some(0) => store_int8(&v, &mut r.digits_int),
                Some(1) => store_int8(&v, &mut r.digits_frac),
                Some(2) => store_int8(&v, &mut r.result_frac),
                Some(3) => match v {
                    Value::Null => Ok(()),
                    Value::Bool(b) => {
                        r.negative = b;
                        Ok(())
                    }
                    other => Err(kind(&other)),
                },
                Some(4) => match v {
                    Value::Null => Ok(()),
                    Value::Array(items) => {
                        let mut res = Ok(());
                        for (i, item) in items.iter().enumerate().take(9) {
                            if let Err(k) = store_int32(item, &mut r.word_buf[i]) {
                                res = res.and(Err(k));
                            }
                        }
                        for w in r.word_buf.iter_mut().skip(items.len()) {
                            *w = 0;
                        }
                        res
                    }
                    other => Err(kind(&other)),
                },
                _ => Ok(()),
            };
            if let Err(k) = res
                && first_err.is_none()
            {
                first_err = Some(format!(
                    "json: cannot unmarshal {k} into Go struct field {TYPE_NAME}.{key}"
                ));
            }
        }
        Ok(match first_err {
            Some(msg) => Decoded::TypeError(msg),
            None => Decoded::Ok(r),
        })
    }
}

// go-std: encoding/json.UnmarshalTypeError.Error L2 (message of a top-level type mismatch, built directly)
fn type_error(kind: &str) -> Decoded {
    Decoded::TypeError(format!(
        "json: cannot unmarshal {kind} into Go value of type {TYPE_NAME}"
    ))
}

// go-std: encoding/json.decodeState.literalStore L2 (JSON kind name used in UnmarshalTypeError.Value)
fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

// go-std: encoding/json.decodeState.literalStore L2 (number into an int8 field; reflect.Value.OverflowInt rejects out-of-range values)
fn store_int8(v: &Value, dst: &mut i8) -> Result<(), &'static str> {
    match v {
        Value::Null => Ok(()),
        Value::Number(n) => match n.as_i64().and_then(|x| i8::try_from(x).ok()) {
            Some(x) => {
                *dst = x;
                Ok(())
            }
            None => Err("number"),
        },
        other => Err(kind(other)),
    }
}

// go-std: encoding/json.decodeState.literalStore L2 (number into an int32 field; reflect.Value.OverflowInt rejects out-of-range values)
fn store_int32(v: &Value, dst: &mut i32) -> Result<(), &'static str> {
    match v {
        Value::Null => Ok(()),
        Value::Number(n) => match n.as_i64().and_then(|x| i32::try_from(x).ok()) {
            Some(x) => {
                *dst = x;
                Ok(())
            }
            None => Err("number"),
        },
        other => Err(kind(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_float_g_matches_go() {
        // Expected strings from Go 1.25 strconv.FormatFloat(f, 'g', -1, 64).
        for (f, want) in [
            (0.0, "0"),
            (-0.0, "-0"),
            (1.0, "1"),
            (0.1, "0.1"),
            (1e-4, "0.0001"),
            (1e-5, "1e-05"),
            (123456.0, "123456"),
            (1234567.0, "1.234567e+06"),
            (1e20, "1e+20"),
            (1e21, "1e+21"),
            (5e-324, "5e-324"),
            (f64::MAX, "1.7976931348623157e+308"),
            // 979980073090831.25 exactly: a tie between ...312 and ...313.
            (979_980_073_090_831.0 + 0.25, "9.799800730908312e+14"),
            (0.30000000000000004, "0.30000000000000004"),
            (f64::NAN, "NaN"),
            (f64::NEG_INFINITY, "-Inf"),
        ] {
            assert_eq!(format_float_g(f), want, "{f:e}");
        }
    }

    #[test]
    fn parse_float_matches_go() {
        // Expected values and errors from Go 1.25 strconv.ParseFloat(s, 64).
        let long = |zeros: usize, tail: &str| format!("1{}{tail}", "0".repeat(zeros));
        for (s, bits, kind) in [
            (
                "1e400".to_string(),
                0x7ff0000000000000u64,
                Some(NumErrKind::Range),
            ),
            (
                "-1e400".to_string(),
                0xfff0000000000000,
                Some(NumErrKind::Range),
            ),
            ("1e-400".to_string(), 0, None),
            ("-1e-400".to_string(), 0x8000000000000000, None),
            (
                "1.7976931348623159e308".to_string(),
                0x7ff0000000000000,
                Some(NumErrKind::Range),
            ),
            (
                "1.7976931348623158e308".to_string(),
                0x7fefffffffffffff,
                None,
            ),
            ("-infinity".to_string(), 0xfff0000000000000, None),
            ("nan".to_string(), 0x7ff8000000000001, None),
            ("+nan".to_string(), 0, Some(NumErrKind::Syntax)),
            ("0x1p-2".to_string(), 0x3fd0000000000000, None),
            ("0x1.8p1".to_string(), 0x4008000000000000, None),
            ("0x1".to_string(), 0, Some(NumErrKind::Syntax)),
            (
                "0x1p1024".to_string(),
                0x7ff0000000000000,
                Some(NumErrKind::Range),
            ),
            ("0x1p-1074".to_string(), 1, None),
            ("1_000".to_string(), 0x408f400000000000, None),
            ("1__0".to_string(), 0, Some(NumErrKind::Syntax)),
            (" 1".to_string(), 0, Some(NumErrKind::Syntax)),
            ("1.".to_string(), 0x3ff0000000000000, None),
            ("+.5".to_string(), 0x3fe0000000000000, None),
            ("".to_string(), 0, Some(NumErrKind::Syntax)),
            // Go's slow path counts at most 800 digits when placing the decimal point.
            (long(1000, "e-500"), 1e299f64.to_bits(), None),
            (long(1000, ""), 0x7ff0000000000000, Some(NumErrKind::Range)),
            (long(800, "e-500"), 1e300f64.to_bits(), None),
        ] {
            let (f, err) = parse_float(&s);
            assert_eq!(f.to_bits(), bits, "{s}");
            assert_eq!(err.as_ref().err().map(|e| e.err), kind, "{s}");
        }
        assert_eq!(
            parse_float("1e400").1.unwrap_err().to_string(),
            "strconv.ParseFloat: parsing \"1e400\": value out of range"
        );
    }
}
