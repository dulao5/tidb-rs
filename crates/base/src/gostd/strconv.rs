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
// go-std: strconv.ParseUint L1 (base 10, bitSize 64)
// go-std: strconv.NumError.Error L1 (strconv.Quote via gofmt %q)
pub fn parse_uint10(s: &[u8]) -> Result<u64, Vec<u8>> {
    let num_err = |why: &str| {
        let mut m = b"strconv.ParseUint: parsing ".to_vec();
        m.extend_from_slice(&crate::gofmt::sprintf(
            "%q",
            &[crate::gofmt::Arg::go_string(s.to_vec())],
        ));
        m.extend_from_slice(b": ");
        m.extend_from_slice(why.as_bytes());
        m
    };
    if s.is_empty() {
        return Err(num_err("invalid syntax"));
    }
    let mut n: u64 = 0;
    for &c in s {
        if !c.is_ascii_digit() {
            return Err(num_err("invalid syntax"));
        }
        // Go returns ErrRange at the first overflowing digit, before later syntax errors.
        match n
            .checked_mul(10)
            .and_then(|v| v.checked_add(u64::from(c - b'0')))
        {
            Some(v) => n = v,
            None => return Err(num_err("value out of range")),
        }
    }
    Ok(n)
}
