//! go-file: pkg/util/parser/parser.go
//!
//! Small parser combinators over Go strings (bytes). `pkg/util` is ported on demand into
//! the lowest crate that uses it (N0 §3): the time code (D01.3) is the only user so far.
//! The errors are only tested for presence by every caller, so they are `()`.

use crate::helper::Pattern;

/// ErrPatternNotMatch represents an error that patterns doesn't match.
// glue: the package variable ErrPatternNotMatch
pub(crate) fn err_pattern_not_match() -> tidb_base::Error {
    tidb_base::errors::new("Pattern not match")
}

/// Match matches the `pat` at least `times`, and returns the match, the rest and the error
// go: Match L1
pub(crate) fn match_pat(buf: &[u8], pat: Pattern, times: usize) -> Result<(&[u8], &[u8]), ()> {
    let mut i = 0;
    while i < buf.len() && pat(buf[i]) {
        i += 1;
    }
    if i < times {
        return Err(());
    }
    Ok((&buf[..i], &buf[i..]))
}

/// MatchOne matches only one time with pat
// go: MatchOne L1
pub(crate) fn match_one(buf: &[u8], pat: Pattern) -> Result<&[u8], ()> {
    if buf.is_empty() || !pat(buf[0]) {
        return Err(());
    }
    Ok(&buf[1..])
}

/// AnyPunct matches an arbitrary punctuation
// go: AnyPunct L1
pub(crate) fn any_punct(buf: &[u8]) -> Result<&[u8], ()> {
    match_one(buf, |b| is_punct_rune(char::from(b)))
}

/// AnyChar matches an arbitrary character
// go: AnyChar L1
#[allow(dead_code)]
pub(crate) fn any_char(buf: &[u8]) -> Result<&[u8], ()> {
    match_one(buf, |_| true)
}

/// Char matches a character: c
// go: Char L1
pub(crate) fn char(buf: &[u8], c: u8) -> Result<&[u8], ()> {
    if buf.is_empty() || buf[0] != c {
        return Err(());
    }
    Ok(&buf[1..])
}

/// Space matches at least `times` spaces
// go: Space L1
pub(crate) fn space(buf: &[u8], times: usize) -> Result<&[u8], ()> {
    let (_, rest) = match_pat(buf, |c| is_space_rune(char::from(c)), times)?;
    Ok(rest)
}

/// Space0 matches at least 0 space.
// go: Space0 L1
pub(crate) fn space0(buf: &[u8]) -> &[u8] {
    match space(buf, 0) {
        Ok(rest) => rest,
        Err(()) => unreachable!("Space(buf, 0) cannot fail"),
    }
}

/// Digit matches at least `times` digits
// go: Digit L1
pub(crate) fn digit(buf: &[u8], times: usize) -> Result<(&[u8], &[u8]), ()> {
    match_pat(buf, |c| tidb_base::gostd::is_digit(char::from(c)), times)
}

/// Number matches a series of digits and convert it to an int
// go: Number L1
pub(crate) fn number(str: &[u8]) -> Result<(isize, &[u8]), ()> {
    let (digits, rest) = digit(str, 1)?;
    let (num, err) = crate::gostd::atoi(digits);
    err.map_err(|_| ())?;
    Ok((num, rest))
}

/// `unicode.IsSpace`.
// go-std: unicode.IsSpace L1
pub(crate) fn is_space_rune(r: char) -> bool {
    // This property isn't the same as Z; special-case it.
    if (r as u32) <= 0xFF {
        return matches!(
            r,
            '\t' | '\n' | '\x0B' | '\x0C' | '\r' | ' ' | '\u{85}' | '\u{A0}'
        );
    }
    // unicode.White_Space beyond Latin-1.
    matches!(
        r as u32,
        0x1680 | 0x2000..=0x200a | 0x2028 | 0x2029 | 0x202f | 0x205f | 0x3000
    )
}

/// `unicode.IsPunct`: category P.
// go-std: unicode.IsPunct L1
pub(crate) fn is_punct_rune(r: char) -> bool {
    use tidb_base::gostd::unicode_tables::{Pc, Pd, Pe, Pf, Pi, Po, Ps};
    tidb_base::gostd::is_one_of(&[&Pc, &Pd, &Ps, &Pe, &Pi, &Pf, &Po], r)
}

/// `unicode.IsNumber`: category N.
// go-std: unicode.IsNumber L1
pub(crate) fn is_number_rune(r: char) -> bool {
    use tidb_base::gostd::unicode_tables::{Nd, Nl, No};
    tidb_base::gostd::is_one_of(&[&Nd, &Nl, &No], r)
}
