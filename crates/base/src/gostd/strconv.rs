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
