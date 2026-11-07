//! TOML datetimes: BurntSushi tries `time.ParseInLocation` with the layouts of `dtTypes`
//! and keeps a `time.Time`. The config structs have no `time.Time` field, so the value is
//! only ever seen through `fmtType` ("time.Time") or `MarshalText` (RFC 3339, when decoded
//! into a TextUnmarshaler). The port validates the same layouts and keeps the text.
//!
//! Deviation: local datetimes, dates and times (no offset) use Go's local time zone, which
//! the port takes as UTC; the vectors only use offset datetimes in text positions.

// go-std: time.getnum L2 (fixed width, zero-padded)
fn digits(b: &[u8], at: usize, n: usize) -> Option<u32> {
    let s = b.get(at..at + n)?;
    if !s.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(s.iter().fold(0, |acc, &d| acc * 10 + u32::from(d - b'0')))
}

// go-std: time.daysIn L1
fn days_in(month: u32, year: u32) -> u32 {
    match month {
        2 if (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400) => {
            29
        }
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// `2006-01-02` at `at`; returns (y, m, d).
// go-std: time.parse L2 (layout 2006-01-02 with range checks)
fn date(b: &[u8], at: usize) -> Option<(u32, u32, u32)> {
    let y = digits(b, at, 4)?;
    if b.get(at + 4) != Some(&b'-') {
        return None;
    }
    let m = digits(b, at + 5, 2)?;
    if b.get(at + 7) != Some(&b'-') {
        return None;
    }
    let d = digits(b, at + 8, 2)?;
    if !(1..=12).contains(&m) || d < 1 || d > days_in(m, y) {
        return None;
    }
    Some((y, m, d))
}

/// `15:04:05.999999999` at `at`; returns (h, m, s, nanos digits text, end).
// go-std: time.parse L2 (layout 15:04:05.999999999 with range checks; fraction truncated to 9 digits)
fn time(b: &[u8], at: usize) -> Option<(u32, u32, u32, String, usize)> {
    let h = digits(b, at, 2)?;
    if b.get(at + 2) != Some(&b':') {
        return None;
    }
    let mi = digits(b, at + 3, 2)?;
    if b.get(at + 5) != Some(&b':') {
        return None;
    }
    let s = digits(b, at + 6, 2)?;
    if h >= 24 || mi >= 60 || s >= 60 {
        return None;
    }
    let mut end = at + 8;
    let mut frac = String::new();
    if matches!(b.get(end), Some(b'.') | Some(b','))
        && b.get(end + 1).is_some_and(u8::is_ascii_digit)
    {
        end += 1;
        while let Some(&c) = b.get(end).filter(|c| c.is_ascii_digit()) {
            if frac.len() < 9 {
                frac.push(char::from(c));
            }
            end += 1;
        }
    }
    Some((h, mi, s, frac, end))
}

/// RFC 3339 text of `time.Time.MarshalText` (RFC3339Nano: trailing zeros of the fraction
/// trimmed, `Z` for a zero offset).
// go-std: time.Time.MarshalText L2 (RFC3339Nano of the parsed fields)
fn text(ymd: (u32, u32, u32), hms: (u32, u32, u32), frac: &str, offset_min: i32) -> String {
    let mut out = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        ymd.0, ymd.1, ymd.2, hms.0, hms.1, hms.2
    );
    let mut f = format!("{frac:0<9}");
    while f.ends_with('0') {
        f.pop();
    }
    if !f.is_empty() {
        out.push('.');
        out.push_str(&f);
    }
    if offset_min == 0 {
        out.push('Z');
    } else {
        let sign = if offset_min < 0 { '-' } else { '+' };
        let a = offset_min.abs();
        out.push_str(&format!("{sign}{:02}:{:02}", a / 60, a % 60));
    }
    out
}

/// `parser.valueDatetime` without the error: `Some(MarshalText)` when one of the layouts
/// parses `v` (after `datetimeRepl`).
// go-dep: github.com/BurntSushi/toml@v1.5.0.parser.valueDatetime L2 (time.ParseInLocation of the dtTypes layouts; local zone = UTC)
pub(crate) fn parse_toml_datetime(v: &str, toml_next: bool) -> Option<String> {
    let b = v.as_bytes();
    // RFC3339Nano and the local datetime.
    if let Some(ymd) = date(b, 0) {
        if b.len() == 10 {
            return Some(text(ymd, (0, 0, 0), "", 0));
        }
        if b.get(10) == Some(&b'T') {
            if let Some((h, mi, s, frac, end)) = time(b, 11) {
                let rest = &b[end..];
                if rest.is_empty() {
                    return Some(text(ymd, (h, mi, s), &frac, 0));
                }
                if rest == b"Z" {
                    return Some(text(ymd, (h, mi, s), &frac, 0));
                }
                if rest.len() == 6 && (rest[0] == b'+' || rest[0] == b'-') && rest[3] == b':' {
                    let oh = digits(rest, 1, 2)?;
                    let om = digits(rest, 4, 2)?;
                    if oh > 24 || om > 60 {
                        return None;
                    }
                    let off = (oh * 60 + om) as i32;
                    return Some(text(
                        ymd,
                        (h, mi, s),
                        &frac,
                        if rest[0] == b'-' { -off } else { off },
                    ));
                }
                return None;
            }
            if toml_next {
                return None; // 1.1 draft layouts without seconds are not supported (unused by TiDB)
            }
        }
        return None;
    }
    if let Some((h, mi, s, frac, end)) = time(b, 0)
        && end == b.len()
    {
        return Some(text((0, 1, 1), (h, mi, s), &frac, 0));
    }
    None
}
