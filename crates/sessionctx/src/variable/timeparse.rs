//! `time.ParseInLocation` and `Time.Format` for the two layouts of `checkTimeSystemVar`
//! (`"15:04"` and `"15:04 -0700"`), with Go's error messages (`time.ParseError`).
//!
//! The Go time package port in tidb-base has no layout parser (D01.3 needed none); this is
//! the part of `time.parse` these layouts reach: `stdHour`, `stdZeroMinute`, `stdNumTZ` and
//! literal text, the range checks, and the "local zone with the same offset" rule.

use std::sync::Arc;

use tidb_base::gostd::time::{self, Location, Time};

/// A parsed time of day: the clock and its location.
pub(crate) struct DayTime {
    hour: i64,
    min: i64,
    loc: Arc<Location>,
}

/// `time.quote`: Go's quoting of a value inside a ParseError message.
// go-std: time.quote L1
fn quote(s: &str) -> String {
    const LOWERHEX: &[u8] = b"0123456789abcdef";
    let mut buf = String::with_capacity(s.len() + 2);
    buf.push('"');
    for (i, c) in s.char_indices() {
        let width = c.len_utf8();
        let invalid = c == '\u{FFFD}' && !s[i..].starts_with('\u{FFFD}');
        if (c as u32) >= 0x80 || (c as u32) < 0x20 || invalid {
            // This means you are asking us to parse a time.Duration or
            // time.Location with unprintable or non-ASCII characters in it.
            // We don't expect to hit this case very often. We could try to
            // reproduce strconv.Quote's behavior with full fidelity but
            // given how rarely we expect to hit these edge cases, speed and
            // conciseness are better.
            for &b in &s.as_bytes()[i..i + width] {
                buf.push_str("\\x");
                buf.push(LOWERHEX[(b >> 4) as usize] as char);
                buf.push(LOWERHEX[(b & 0xF) as usize] as char);
            }
        } else {
            if c == '"' || c == '\\' {
                buf.push('\\');
            }
            buf.push(c);
        }
    }
    buf.push('"');
    buf
}

/// `ParseError.Error`.
// go-std: time.ParseError.Error L1
fn parse_error(layout: &str, value: &str, layout_elem: &str, value_elem: &str, message: &str) -> String {
    if message.is_empty() {
        return format!(
            "parsing time {} as {}: cannot parse {} as {}",
            quote(value),
            quote(layout),
            quote(value_elem),
            quote(layout_elem)
        );
    }
    format!("parsing time {}{}", quote(value), message)
}

fn is_digit(s: &[u8], i: usize) -> bool {
    s.len() > i && s[i].is_ascii_digit()
}

/// `getnum`: one or two digits (two when `fixed`).
// go-std: time.getnum L1
fn getnum(s: &str, fixed: bool) -> Result<(i64, &str), ()> {
    let b = s.as_bytes();
    if !is_digit(b, 0) {
        return Err(());
    }
    if !is_digit(b, 1) {
        if fixed {
            return Err(());
        }
        return Ok(((b[0] - b'0') as i64, &s[1..]));
    }
    Ok((((b[0] - b'0') as i64) * 10 + (b[1] - b'0') as i64, &s[2..]))
}

fn cutspace(s: &str) -> &str {
    s.trim_start_matches(' ')
}

/// `skip` removes the given prefix from value, treating runs of space characters as equivalent.
// go-std: time.skip L1
fn skip<'a>(mut value: &'a str, mut prefix: &str) -> Result<&'a str, &'a str> {
    while !prefix.is_empty() {
        if prefix.as_bytes()[0] == b' ' {
            if !value.is_empty() && value.as_bytes()[0] != b' ' {
                return Err(value);
            }
            prefix = cutspace(prefix);
            value = cutspace(value);
            continue;
        }
        if value.is_empty() || value.as_bytes()[0] != prefix.as_bytes()[0] {
            return Err(value);
        }
        prefix = &prefix[1..];
        value = &value[1..];
    }
    Ok(value)
}

#[derive(Clone, Copy, PartialEq)]
enum Std {
    Hour,
    ZeroMinute,
    NumTZ,
}

/// `time.ParseInLocation(layout, value, loc)` for `"15:04"` / `"15:04 -0700"`.
// go-std: time.ParseInLocation L2 (only the layout elements of the two checkTimeSystemVar layouts)
pub(crate) fn parse_day_time(layout: &str, value: &str, loc: &Arc<Location>) -> Result<DayTime, String> {
    // The layouts, split by nextStdChunk: (prefix, std, std string).
    let chunks: &[(&str, Std, &str)] = if layout == "15:04" {
        &[("", Std::Hour, "15"), (":", Std::ZeroMinute, "04")]
    } else {
        debug_assert_eq!(layout, "15:04 -0700");
        &[("", Std::Hour, "15"), (":", Std::ZeroMinute, "04"), (" ", Std::NumTZ, "-0700")]
    };
    let alayout = layout;
    let avalue = value;
    let mut value = value;
    let (mut hour, mut min) = (0i64, 0i64);
    let mut zone_offset: Option<i64> = None;
    for &(prefix, std, stdstr) in chunks {
        value = match skip(value, prefix) {
            Ok(v) => v,
            Err(v) => return Err(parse_error(alayout, avalue, prefix, v, "")),
        };
        let hold = value;
        let mut range_err = "";
        let mut bad = false;
        match std {
            Std::Hour => match getnum(value, false) {
                Ok((h, rest)) => {
                    hour = h;
                    value = rest;
                    if !(0..24).contains(&hour) {
                        range_err = "hour";
                    }
                }
                Err(()) => bad = true,
            },
            Std::ZeroMinute => match getnum(value, true) {
                Ok((m, rest)) => {
                    min = m;
                    value = rest;
                    if !(0..60).contains(&min) {
                        range_err = "minute";
                    }
                }
                Err(()) => bad = true,
            },
            Std::NumTZ => {
                if value.len() < 5 {
                    bad = true;
                } else {
                    let b = value.as_bytes();
                    let two = |x: &[u8]| -> Result<i64, ()> {
                        if x[0].is_ascii_digit() && x[1].is_ascii_digit() {
                            Ok(((x[0] - b'0') as i64) * 10 + (x[1] - b'0') as i64)
                        } else {
                            Err(())
                        }
                    };
                    // getnum(hour, true), getnum(min, true) on the byte slices value[1:3], value[3:5].
                    let parsed = two(&b[1..3]).and_then(|hr| two(&b[3..5]).map(|mm| (hr, mm)));
                    match parsed {
                        Ok((hr, mm)) => {
                            // b[1..5] are ASCII digits, so byte 5 is a character boundary.
                            value = &value[5..];
                            // The range test use > rather than >=,
                            // as some people do write offsets of 24 hours
                            // or 60 minutes or 60 seconds.
                            if hr > 24 {
                                range_err = "time zone offset hour";
                            }
                            if mm > 60 {
                                range_err = "time zone offset minute";
                            }
                            let mut off = (hr * 60 + mm) * 60;
                            match b[0] {
                                b'+' => {}
                                b'-' => off = -off,
                                _ => bad = true,
                            }
                            zone_offset = Some(off);
                        }
                        Err(()) => bad = true,
                    }
                }
            }
        }
        if !range_err.is_empty() {
            return Err(parse_error(
                alayout,
                avalue,
                stdstr,
                value,
                &format!(": {range_err} out of range"),
            ));
        }
        if bad {
            return Err(parse_error(alayout, avalue, stdstr, hold, ""));
        }
    }
    if !value.is_empty() {
        return Err(parse_error(alayout, avalue, "", value, &format!(": extra text: {}", quote(value))));
    }
    match zone_offset {
        Some(off) => {
            // Year 0, January 1 (the defaults of time.parse), in UTC, shifted by the offset.
            let t = time::date(0, 1, 1, hour, min, 0, 0, &time::utc());
            let sec = t.unix() - off;
            // Look for local zone with the given offset.
            // If that zone was in effect at the given time, use it.
            let local_off = loc.lookup(sec).offset as i64;
            if local_off == off {
                return Ok(DayTime { hour, min, loc: loc.clone() });
            }
            // Otherwise create fake zone to record offset.
            Ok(DayTime { hour, min, loc: time::fixed_zone("", off as i32) })
        }
        None => Ok(DayTime { hour, min, loc: loc.clone() }),
    }
}

/// `checkTimeSystemVar`'s result: the clock on today's date in its location, formatted as
/// `FullDayTimeFormat` (`"15:04 -0700"`).
// go-std: time.Time.Format L2 ("15:04 -0700" only)
pub(crate) fn format_full_day_time_today(t: &DayTime) -> String {
    let now: Time = time::now();
    let d = time::date(now.year(), now.month(), now.day(), t.hour, t.min, 0, 0, &t.loc);
    let (h, m, _) = d.clock();
    let offset = d.location().lookup(d.unix()).offset as i64;
    let zone = offset / 60; // convert to minutes
    let (sign, absz) = if zone < 0 { ('-', -zone) } else { ('+', zone) };
    format!("{h:02}:{m:02} {sign}{:02}{:02}", absz / 60, absz % 60)
}
