//! `strconv` parsing used by the TOML decoder, and `FormatFloat` (Go's float formatting, `gofmt`).

/// `strconv.ParseInt(s, 0, 64)` for the integer literals the TOML lexer produces (optional
/// sign, `0x` / `0o` / `0b` prefixes, `_` separators already validated by the caller).
/// `Err(true)` is `ErrRange`, `Err(false)` is `ErrSyntax`.
// go-std: strconv.ParseInt L2 (base 0 with validated underscores; octal "0" prefix is rejected by the caller)
pub fn parse_int_base0(s: &str) -> Result<i64, bool> {
    let (neg, body) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let body: String = body.chars().filter(|&c| c != '_').collect();
    let (radix, digits) =
        if let Some(d) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
            (16, d)
        } else if let Some(d) = body.strip_prefix("0o").or_else(|| body.strip_prefix("0O")) {
            (8, d)
        } else if let Some(d) = body.strip_prefix("0b").or_else(|| body.strip_prefix("0B")) {
            (2, d)
        } else {
            (10, body.as_str())
        };
    if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
        return Err(false);
    }
    let mut un: u64 = 0;
    for c in digits.chars() {
        let d = u64::from(c.to_digit(radix).unwrap_or(0));
        un = match un
            .checked_mul(u64::from(radix))
            .and_then(|v| v.checked_add(d))
        {
            Some(v) => v,
            None => return Err(true),
        };
    }
    let cutoff = 1u64 << 63;
    if !neg && un >= cutoff {
        return Err(true);
    }
    if neg && un > cutoff {
        return Err(true);
    }
    if neg {
        Ok((un as i64).wrapping_neg())
    } else {
        Ok(un as i64)
    }
}

/// `strconv.ParseFloat(s, 64)` for the decimal / `inf` / `nan` literals the TOML lexer
/// produces (no `_`). `Err(true)` is `ErrRange` (overflow to ±Inf), `Err(false)` `ErrSyntax`.
// go-std: strconv.ParseFloat L2 (decimal and inf/nan forms; Rust's parser is correctly rounded like Go's)
pub fn parse_float(s: &str) -> Result<f64, bool> {
    let lower = s.to_ascii_lowercase();
    let unsigned = lower.trim_start_matches(['+', '-']);
    let special = matches!(unsigned, "inf" | "infinity" | "nan");
    if !special
        && !s
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'.' | b'e' | b'E'))
    {
        return Err(false);
    }
    if unsigned == "nan" {
        // Go's `special` accepts a sign only before "inf" / "infinity"; "NaN" is `math.NaN()`
        // (payload 1, not Rust's `f64::NAN`).
        if unsigned.len() != lower.len() {
            return Err(false);
        }
        return Ok(super::math::nan());
    }
    let v: f64 = s.parse().map_err(|_| false)?;
    if v.is_infinite() && !special {
        return Err(true);
    }
    Ok(v)
}

/// `strconv.FormatFloat(f, fmt, prec, bitSize)` (and `AppendFloat`, which appends the same
/// bytes): fmt `b e E f g G x X`, bitSize 32 or 64.
// go-std: strconv.FormatFloat L1
pub fn format_float(f: f64, fmt: u8, prec: isize, bit_size: u8) -> String {
    crate::gofmt::append_float(f, fmt, prec, bit_size)
}

/// `strconv.AppendFloat(dst, f, fmt, prec, bitSize)`: appends the bytes of
/// [`format_float`] to `dst`. The shortest `'e'` / `'f'` forms of a finite float64 (what
/// `encoding/json` and TiDB's JSON output use) are written straight into `dst`, without
/// temporaries; every other form goes through `format_float`.
// go-std: strconv.AppendFloat L1
pub fn append_float(dst: &mut Vec<u8>, f: f64, fmt: u8, prec: isize, bit_size: u8) {
    if prec < 0 && bit_size == 64 && (fmt == b'e' || fmt == b'f') && f.is_finite() {
        append_shortest64(dst, f, fmt);
    } else {
        dst.extend_from_slice(format_float(f, fmt, prec, bit_size).as_bytes());
    }
}

/// `genericFtoa` for a finite float64 with `prec < 0` and fmt `'e'` / `'f'`: Ryū shortest
/// digits, then `fmtE` / `fmtF` with the shortest precision.
// go-std: strconv.genericFtoa L2 (only the shortest 'e'/'f' float64 path, appending to dst)
fn append_shortest64(dst: &mut Vec<u8>, v: f64, fmt: u8) {
    use super::ftoaryu::{DecimalSlice, FLOAT64INFO, ryu_ftoa_shortest};
    let bits = v.to_bits();
    let neg = bits >> 63 != 0;
    let mut exp = ((bits >> 52) & 0x7FF) as i32;
    let mut mant = bits & ((1u64 << 52) - 1);
    if exp == 0 {
        exp += 1;
    } else {
        mant |= 1u64 << 52;
    }
    exp += -1023;
    let mut dd = DecimalSlice::new();
    ryu_ftoa_shortest(&mut dd, mant, exp - 52, &FLOAT64INFO);
    let (d, dp) = (dd.digits(), dd.dp);
    let nd = d.len() as isize;
    if neg {
        dst.push(b'-');
    }
    if fmt == b'e' {
        // fmtE with prec = max(nd-1, 0)
        let prec = (nd - 1).max(0);
        dst.push(if nd == 0 { b'0' } else { d[0] });
        if prec > 0 {
            dst.push(b'.');
            dst.extend_from_slice(&d[1..nd as usize]);
        }
        dst.push(b'e');
        let mut e = if nd == 0 { 0 } else { dp - 1 };
        if e < 0 {
            dst.push(b'-');
            e = -e;
        } else {
            dst.push(b'+');
        }
        if e < 10 {
            dst.push(b'0');
            dst.push(b'0' + e as u8);
        } else if e < 100 {
            dst.push(b'0' + (e / 10) as u8);
            dst.push(b'0' + (e % 10) as u8);
        } else {
            dst.push(b'0' + (e / 100) as u8);
            dst.push(b'0' + (e / 10 % 10) as u8);
            dst.push(b'0' + (e % 10) as u8);
        }
    } else {
        // fmtF with prec = max(nd-dp, 0)
        let prec = (nd - dp).max(0);
        if dp > 0 {
            let m = nd.min(dp);
            dst.extend_from_slice(&d[..m as usize]);
            for _ in m..dp {
                dst.push(b'0');
            }
        } else {
            dst.push(b'0');
        }
        if prec > 0 {
            dst.push(b'.');
            for i in 1..=prec {
                let j = dp + i - 1;
                dst.push(if 0 <= j && j < nd {
                    d[j as usize]
                } else {
                    b'0'
                });
            }
        }
    }
}

/// `strconv.ParseUint(s, 10, 64)`: the value, or the `*strconv.NumError` text
/// (`strconv.ParseUint: parsing "<Quote(s)>": invalid syntax | value out of range`).
// glue: Result form of parse_uint_go(s, 10, 64)
pub fn parse_uint10(s: &[u8]) -> Result<u64, Vec<u8>> {
    let (n, err) = parse_uint_go(s, 10, 64);
    err.map(|()| n).map_err(|e| e.error())
}

/// `*strconv.NumError.Error()`: `strconv.<func>: parsing <Quote(num)>: <why>`.
// go-std: strconv.NumError.Error L1 (strconv.Quote via gofmt %q)
fn num_error_text(func: &str, num: &[u8], why: &str) -> Vec<u8> {
    let mut m = format!("strconv.{func}: parsing ").into_bytes();
    m.extend_from_slice(&crate::gofmt::sprintf(
        "%q",
        &[crate::gofmt::Arg::go_string(num.to_vec())],
    ));
    m.extend_from_slice(b": ");
    m.extend_from_slice(why.as_bytes());
    m
}

/// `strconv.ParseUint(s, 10, 64)`, Go-shaped (`M-ERRVAL`): on `ErrRange` the value is
/// `math.MaxUint64`, on `ErrSyntax` 0; the error is the `*strconv.NumError` text with
/// `func` (`"ParseUint"`, or the caller's name when Go rewrites `NumError.Func`).
// glue: parse_uint_go(s, 10, 64) with the error as text and NumError.Func rewritten
pub fn parse_uint(s: &[u8], func: &'static str) -> (u64, Result<(), Vec<u8>>) {
    let (n, err) = parse_uint_go(s, 10, 64);
    (
        n,
        err.map_err(|mut e| {
            e.func = func;
            e.error()
        }),
    )
}

/// `strconv.ParseInt(s, 10, 64)`, Go-shaped (`M-ERRVAL`): on `ErrRange` the value is
/// `math.MaxInt64` / `math.MinInt64`, on `ErrSyntax` 0. `func` is `"ParseInt"` (or `"Atoi"`
/// from [`atoi`]).
// go-std: strconv.ParseInt L1 (base 10, bitSize 64)
pub fn parse_int(s: &[u8], func: &str) -> (i64, Result<(), Vec<u8>>) {
    if s.is_empty() {
        return (0, Err(num_error_text(func, s, "invalid syntax")));
    }
    let (neg, body) = match s[0] {
        b'+' => (false, &s[1..]),
        b'-' => (true, &s[1..]),
        _ => (false, s),
    };
    let (un, err) = parse_uint_go(body, 10, 64);
    if err.is_err() && un != u64::MAX {
        // A syntax error: Go reports the input with its sign.
        return (0, Err(num_error_text(func, s, "invalid syntax")));
    }
    let cutoff = 1u64 << 63;
    if !neg && un >= cutoff {
        return (i64::MAX, Err(num_error_text(func, s, "value out of range")));
    }
    if neg && un > cutoff {
        return (i64::MIN, Err(num_error_text(func, s, "value out of range")));
    }
    let n = un as i64;
    (if neg { n.wrapping_neg() } else { n }, Ok(()))
}

/// `strconv.Atoi(s)` on a 64-bit platform: the fast path for 1..18 bytes, else `ParseInt`
/// with `NumError.Func` rewritten to `"Atoi"`.
// go-std: strconv.Atoi L1 (intSize 64)
pub fn atoi(s: &[u8]) -> (i64, Result<(), Vec<u8>>) {
    if !s.is_empty() && s.len() < 19 {
        let mut body = s;
        if s[0] == b'-' || s[0] == b'+' {
            body = &s[1..];
            if body.is_empty() {
                return (0, Err(num_error_text("Atoi", s, "invalid syntax")));
            }
        }
        let mut n: i64 = 0;
        for &ch in body {
            let d = ch.wrapping_sub(b'0');
            if d > 9 {
                return (0, Err(num_error_text("Atoi", s, "invalid syntax")));
            }
            n = n * 10 + i64::from(d);
        }
        if s[0] == b'-' {
            n = -n;
        }
        return (n, Ok(()));
    }
    parse_int(s, "Atoi")
}

/// Why a `strconv` parse failed: the `Err` field of a `*strconv.NumError`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NumErrKind {
    /// `strconv.ErrSyntax`: "invalid syntax".
    Syntax,
    /// `strconv.ErrRange`: "value out of range".
    Range,
    /// `baseError`: "invalid base N".
    Base(i32),
    /// `bitSizeError`: "invalid bit size N".
    BitSize(i32),
}

/// `*strconv.NumError`: the failing function, the input (a Go string: bytes) and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NumError {
    /// The failing function (`"ParseUint"`, ...); callers may rewrite it, as Go code does.
    pub func: &'static str,
    /// The input.
    pub num: Vec<u8>,
    /// The reason.
    pub err: NumErrKind,
}

impl NumError {
    /// `Error()`: `strconv.<Func>: parsing <strconv.Quote(Num)>: <Err>`.
    // go-std: strconv.NumError.Error L1 (strconv.Quote via gofmt %q)
    pub fn error(&self) -> Vec<u8> {
        let why = match self.err {
            NumErrKind::Syntax => "invalid syntax".to_owned(),
            NumErrKind::Range => "value out of range".to_owned(),
            NumErrKind::Base(b) => format!("invalid base {b}"),
            NumErrKind::BitSize(b) => format!("invalid bit size {b}"),
        };
        num_error_text(self.func, &self.num, &why)
    }
}

impl std::fmt::Display for NumError {
    // glue: fmt of an error value prints Error()
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&String::from_utf8_lossy(&self.error()))
    }
}

impl std::error::Error for NumError {}

/// `strconv.ParseUint(s, base, bitSize)`: like `ParseInt` but for unsigned numbers; a sign
/// prefix is not permitted. `base` 0 takes the base from the prefix (`0b`, `0o`, `0x`, or a
/// leading `0` for octal) and allows underscores as digit separators. `bitSize` 0 is `int`
/// (64 bits). On a range error the value is the largest value of the size (`M-ERRVAL`), on
/// any other error 0.
// go-std: strconv.ParseUint L1
pub fn parse_uint_go(s: &[u8], base: i32, bit_size: i32) -> (u64, Result<(), NumError>) {
    const FN_PARSE_UINT: &str = "ParseUint";
    let err = |kind| NumError {
        func: FN_PARSE_UINT,
        num: s.to_vec(),
        err: kind,
    };
    if s.is_empty() {
        return (0, Err(err(NumErrKind::Syntax)));
    }
    let lower = |c: u8| c | (b'x' - b'X');

    let base0 = base == 0;
    let s0 = s;
    let mut s = s;
    let mut base = base;
    if (2..=36).contains(&base) {
        // valid base; nothing to do
    } else if base == 0 {
        // Look for octal, hex prefix.
        base = 10;
        if s[0] == b'0' {
            if s.len() >= 3 && lower(s[1]) == b'b' {
                base = 2;
                s = &s[2..];
            } else if s.len() >= 3 && lower(s[1]) == b'o' {
                base = 8;
                s = &s[2..];
            } else if s.len() >= 3 && lower(s[1]) == b'x' {
                base = 16;
                s = &s[2..];
            } else {
                base = 8;
                s = &s[1..];
            }
        }
    } else {
        return (0, Err(err(NumErrKind::Base(base))));
    }

    let mut bit_size = bit_size;
    if bit_size == 0 {
        bit_size = 64; // IntSize
    } else if !(0..=64).contains(&bit_size) {
        return (0, Err(err(NumErrKind::BitSize(bit_size))));
    }

    // Cutoff is the smallest number such that cutoff*base > maxUint64.
    let cutoff = u64::MAX / base as u64 + 1;
    // Go: uint64(1)<<uint(bitSize) - 1 (a shift by 64 is 0, so the result is MaxUint64).
    let max_val = 1u64
        .checked_shl(bit_size as u32)
        .unwrap_or(0)
        .wrapping_sub(1);

    let mut underscores = false;
    let mut n: u64 = 0;
    for &c in s {
        let d = if c == b'_' && base0 {
            underscores = true;
            continue;
        } else if c.is_ascii_digit() {
            c - b'0'
        } else if lower(c).is_ascii_lowercase() {
            lower(c) - b'a' + 10
        } else {
            return (0, Err(err(NumErrKind::Syntax)));
        };

        if i32::from(d) >= base {
            return (0, Err(err(NumErrKind::Syntax)));
        }

        if n >= cutoff {
            // n*base overflows
            return (max_val, Err(err(NumErrKind::Range)));
        }
        n *= base as u64;

        let n1 = n.wrapping_add(u64::from(d));
        if n1 < n || n1 > max_val {
            // n+d overflows
            return (max_val, Err(err(NumErrKind::Range)));
        }
        n = n1;
    }

    if underscores && !underscore_ok(s0) {
        return (0, Err(err(NumErrKind::Syntax)));
    }

    (n, Ok(()))
}

/// `underscoreOK` reports whether the underscores in s are allowed. Checking them in this
/// one function lets all the parsers skip over them simply. Underscore must appear only
/// between digits or between a base prefix and a digit.
// go-std: strconv.underscoreOK L1
pub fn underscore_ok(s: &[u8]) -> bool {
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

#[cfg(test)]
mod tests {
    use super::{NumErrKind, parse_uint_go};

    #[test]
    fn parse_uint_matches_go() {
        // Expected values and errors of Go 1.25 strconv.ParseUint(s, base, bitSize).
        use NumErrKind::*;
        for (s, base, bits, want, err) in [
            ("", 10, 64, 0, Some(Syntax)),
            ("0", 0, 64, 0, None),
            ("12", 10, 0, 12, None),
            ("0x10", 0, 64, 16, None),
            ("0X1f", 0, 64, 31, None),
            ("0o17", 0, 64, 15, None),
            ("017", 0, 64, 15, None),
            ("08", 0, 64, 0, Some(Syntax)),
            ("0x", 0, 64, 0, Some(Syntax)),
            ("0b_1", 0, 64, 1, None),
            ("1_2", 0, 64, 12, None),
            ("_12", 0, 64, 0, Some(Syntax)),
            ("1__2", 0, 64, 0, Some(Syntax)),
            ("1_2", 10, 64, 0, Some(Syntax)),
            ("-1", 10, 64, 0, Some(Syntax)),
            ("zz", 36, 64, 1295, None),
            ("18446744073709551615", 10, 64, u64::MAX, None),
            ("18446744073709551616", 10, 64, u64::MAX, Some(Range)),
            ("256", 10, 8, 255, Some(Range)),
            ("11111111", 2, 8, 255, None),
            ("10", 37, 64, 0, Some(Base(37))),
            ("10", 1, 64, 0, Some(Base(1))),
            ("1", 10, 65, 0, Some(BitSize(65))),
        ] {
            let (n, e) = parse_uint_go(s.as_bytes(), base, bits);
            assert_eq!(
                (n, e.as_ref().err().map(|e| e.err)),
                (want, err),
                "{s:?} {base} {bits}"
            );
        }
        let msg = |s: &str, base, bits| {
            String::from_utf8(
                parse_uint_go(s.as_bytes(), base, bits)
                    .1
                    .unwrap_err()
                    .error(),
            )
            .unwrap()
        };
        assert_eq!(
            msg("256", 10, 8),
            "strconv.ParseUint: parsing \"256\": value out of range"
        );
        assert_eq!(
            msg("a\n", 10, 64),
            "strconv.ParseUint: parsing \"a\\n\": invalid syntax"
        );
        assert_eq!(
            msg("1", 37, 64),
            "strconv.ParseUint: parsing \"1\": invalid base 37"
        );
        assert_eq!(
            msg("1", 10, -1),
            "strconv.ParseUint: parsing \"1\": invalid bit size -1"
        );
    }
}
