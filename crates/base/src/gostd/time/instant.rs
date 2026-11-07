//! go-file: $GOROOT/src/time/time.go
//! go-file: $GOROOT/src/time/format.go
//!
//! `time.Time`. The monotonic clock reading of `time.Now()` is not kept: no ported caller
//! subtracts two `Now()` values, and every other operation strips it (Go `stripMono`), so
//! the wall clock alone gives Go's results.

use std::sync::Arc;

use super::{
    ABSOLUTE_TO_UNIX, AbsDays, Duration, INTERNAL_TO_UNIX, Location, MAX_DURATION, MIN_DURATION,
    SECOND, SECONDS_PER_DAY, SECONDS_PER_HOUR, SECONDS_PER_MINUTE, UNIX_TO_ABSOLUTE,
    UNIX_TO_INTERNAL, abs_clock, abs_seconds_days, date_to_abs_days, less_than_half, utc,
};

/// `time.Month` (January = 1).
pub type Month = i64;
/// `time.Weekday` (Sunday = 0).
pub type Weekday = i64;

/// `time.Time`: seconds since January 1, year 1 00:00:00 UTC (Go's internal `sec()`),
/// nanoseconds in `[0, 1e9)`, and the location (`None` is UTC, as Go's `nil` `loc`).
#[derive(Clone, Debug)]
pub struct Time {
    sec: i64,
    nsec: i32,
    loc: Option<Arc<Location>>,
}

const DAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

/// String returns the English name of the day ("Sunday", "Monday", ...).
// go-std: time.Weekday.String L2 (out-of-range days are not produced by any caller)
pub fn weekday_string(d: Weekday) -> &'static str {
    DAYS[d as usize]
}

/// unixTime: the Time of Unix seconds `sec` and `nsec` nanoseconds, in Local.
// go-std: time.unixTime L2 (the location is set by the callers)
fn unix_time(sec: i64, nsec: i32) -> Time {
    Time {
        sec: sec.wrapping_add(UNIX_TO_INTERNAL),
        nsec,
        loc: None,
    }
}

/// Unix returns the local Time corresponding to the given Unix time,
/// sec seconds and nsec nanoseconds since January 1, 1970 UTC.
// go-std: time.Unix L2 (the result is in UTC instead of Local; callers set the location)
pub fn unix(mut sec: i64, mut nsec: i64) -> Time {
    if !(0..1_000_000_000).contains(&nsec) {
        let n = nsec / 1_000_000_000;
        sec += n;
        nsec -= n * 1_000_000_000;
        if nsec < 0 {
            nsec += 1_000_000_000;
            sec -= 1;
        }
    }
    unix_time(sec, nsec as i32)
}

/// Now returns the current local time.
// go-std: time.Now L2 (wall clock only, see the file comment)
pub fn now() -> Time {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let mut t = unix_time(d.as_secs() as i64, d.subsec_nanos() as i32);
    t.set_loc(super::local());
    t
}

/// norm returns nhi, nlo such that
///
///    hi * base + lo == nhi * base + nlo
///    0 <= nlo < base
// go-std: time.norm L1
fn norm(mut hi: i64, mut lo: i64, base: i64) -> (i64, i64) {
    if lo < 0 {
        let n = (-lo - 1) / base + 1;
        hi -= n;
        lo += n * base;
    }
    if lo >= base {
        let n = lo / base;
        hi += n;
        lo -= n * base;
    }
    (hi, lo)
}

/// Date returns the Time corresponding to
///
///    yyyy-mm-dd hh:mm:ss + nsec nanoseconds
///
/// in the appropriate zone for that time in the given location.
///
/// The month, day, hour, min, sec, and nsec values may be outside
/// their usual ranges and will be normalized during the conversion.
/// For example, October 32 converts to November 1.
///
/// A daylight savings time transition skips or repeats times.
/// For example, in the United States, March 13, 2011 2:15am never occurred,
/// while November 6, 2011 1:15am occurred twice. In such cases, the
/// choice of time zone, and therefore the time, is not well-defined.
/// Date returns a time that is correct in one of the two zones involved
/// in the transition, but it does not guarantee which.
// go-std: time.Date L1
#[allow(clippy::too_many_arguments)]
pub fn date(
    year: i64,
    month: Month,
    day: i64,
    hour: i64,
    min: i64,
    sec: i64,
    nsec: i64,
    loc: &Arc<Location>,
) -> Time {
    // Normalize month, overflowing into year.
    let m = month - 1;
    let (year, m) = norm(year, m, 12);
    let month = m + 1;

    // Normalize nsec, sec, min, hour, overflowing into day.
    let (sec, nsec) = norm(sec, nsec, 1_000_000_000);
    let (min, sec) = norm(min, sec, 60);
    let (hour, min) = norm(hour, min, 60);
    let (day, hour) = norm(day, hour, 24);

    // Convert to absolute time and then Unix time.
    let mut unix = (date_to_abs_days(year, month, day).0 as i64)
        .wrapping_mul(SECONDS_PER_DAY)
        .wrapping_add(hour * SECONDS_PER_HOUR + min * SECONDS_PER_MINUTE + sec)
        .wrapping_add(ABSOLUTE_TO_UNIX);

    // Look for zone offset for expected time, so we can adjust to UTC.
    // The lookup function expects UTC, so first we pass unix in the
    // hope that it will not be too close to a zone transition,
    // and then adjust if it is.
    let z = loc.lookup(unix);
    let mut offset = z.offset;
    if offset != 0 {
        let utc = unix - i64::from(offset);
        // If utc is valid for the time zone we found, then we have the right offset.
        // If not, we get the correct offset by looking up utc in the location.
        if utc < z.start || utc >= z.end {
            offset = loc.lookup(utc).offset;
        }
        unix -= i64::from(offset);
    }

    let mut t = unix_time(unix, nsec as i32);
    t.set_loc(Arc::clone(loc));
    t
}

impl Time {
    // go-std: time.Time.setLoc L1
    fn set_loc(&mut self, loc: Arc<Location>) {
        if super::is_utc(&loc) {
            self.loc = None;
        } else {
            self.loc = Some(loc);
        }
    }

    // go-std: time.Time.unixSec L1
    fn unix_sec(&self) -> i64 {
        self.sec.wrapping_add(INTERNAL_TO_UNIX)
    }

    /// addSec adds d seconds to the time.
    // go-std: time.Time.addSec L1
    fn add_sec(&mut self, d: i64) {
        // Check if the sum of t.ext and d overflows and handle it properly.
        let sum = self.sec.wrapping_add(d);
        if (sum > self.sec) == (d > 0) {
            self.sec = sum;
        } else if d > 0 {
            self.sec = i64::MAX;
        } else {
            self.sec = -i64::MAX;
        }
    }

    // glue: the zone lookup Location of t (UTC for None)
    fn lookup(&self) -> super::ZoneInfo<'_> {
        match &self.loc {
            Some(l) => l.lookup(self.unix_sec()),
            None => super::ZoneInfo {
                name: "UTC",
                offset: 0,
                start: i64::MIN,
                end: i64::MAX,
                is_dst: false,
            },
        }
    }

    /// absSec returns the time t as an absolute seconds, adjusted by the zone offset.
    /// It is called when computing a presentation property like Month or Hour.
    /// We'd rather call it abs, but there are linknames to abs that make that problematic.
    /// See timeAbs below.
    // go-std: time.Time.absSec L1 (the cache check is inside Location::lookup)
    fn abs_sec(&self) -> u64 {
        let mut sec = self.unix_sec();
        if let Some(l) = &self.loc {
            sec = sec.wrapping_add(i64::from(l.lookup(sec).offset));
        }
        (sec as u64).wrapping_add(UNIX_TO_ABSOLUTE as u64)
    }

    /// locabs is a combination of the Zone and abs methods,
    /// extracting both return values from a single zone lookup.
    // go-std: time.Time.locabs L1 (the cache check is inside Location::lookup)
    fn locabs(&self) -> (&str, i32, u64) {
        let mut sec = self.unix_sec();
        let (name, offset) = match &self.loc {
            Some(l) => {
                let z = l.lookup(sec);
                sec = sec.wrapping_add(i64::from(z.offset));
                (z.name, z.offset)
            }
            None => ("UTC", 0),
        };
        (
            name,
            offset,
            (sec as u64).wrapping_add(UNIX_TO_ABSOLUTE as u64),
        )
    }

    /// Date returns the year, month, and day in which t occurs.
    // go-std: time.Time.Date L1
    pub fn date(&self) -> (i64, Month, i64) {
        AbsDays(abs_seconds_days(self.abs_sec())).date()
    }

    /// Year returns the year in which t occurs.
    // go-std: time.Time.Year L1
    pub fn year(&self) -> i64 {
        self.date().0
    }

    /// Month returns the month of the year specified by t.
    // go-std: time.Time.Month L1
    pub fn month(&self) -> Month {
        self.date().1
    }

    /// Day returns the day of the month specified by t.
    // go-std: time.Time.Day L1
    pub fn day(&self) -> i64 {
        self.date().2
    }

    /// Weekday returns the day of the week specified by t.
    // go-std: time.Time.Weekday L1
    pub fn weekday(&self) -> Weekday {
        AbsDays(abs_seconds_days(self.abs_sec())).weekday()
    }

    /// Clock returns the hour, minute, and second within the day specified by t.
    // go-std: time.Time.Clock L1
    pub fn clock(&self) -> (i64, i64, i64) {
        abs_clock(self.abs_sec())
    }

    /// Nanosecond returns the nanosecond offset within the second specified by t,
    /// in the range [0, 999999999].
    // go-std: time.Time.Nanosecond L1
    pub fn nanosecond(&self) -> i64 {
        i64::from(self.nsec)
    }

    /// YearDay returns the day of the year specified by t, in the range [1,365] for non-leap years,
    /// and [1,366] in leap years.
    // go-std: time.Time.YearDay L1
    pub fn year_day(&self) -> i64 {
        AbsDays(abs_seconds_days(self.abs_sec())).year_yday().1
    }

    /// Location returns the time zone information associated with t.
    // go-std: time.Time.Location L1
    pub fn location(&self) -> Arc<Location> {
        match &self.loc {
            Some(l) => Arc::clone(l),
            None => utc(),
        }
    }

    /// In returns a copy of t representing the same time instant, but
    /// with the copy's location information set to loc for display
    /// purposes.
    // go-std: time.Time.In L1
    pub fn in_loc(&self, loc: &Arc<Location>) -> Time {
        let mut t = self.clone();
        t.set_loc(Arc::clone(loc));
        t
    }

    /// Unix returns t as a Unix time, the number of seconds elapsed
    /// since January 1, 1970 UTC.
    // go-std: time.Time.Unix L1
    pub fn unix(&self) -> i64 {
        self.unix_sec()
    }

    /// ZoneBounds returns the bounds of the time zone in effect at time t.
    /// The zone begins at start and the next zone begins at end.
    /// If the zone begins at the beginning of time, start will be returned as a zero Time.
    /// If the zone goes on forever, end will be returned as a zero Time.
    /// The Location of the returned times will be the same as t.
    // go-std: time.Time.ZoneBounds L1
    pub fn zone_bounds(&self) -> (Time, Time) {
        let z = self.lookup();
        let mut start = Time::zero();
        let mut end = Time::zero();
        if z.start != i64::MIN {
            start = unix_time(z.start, 0);
            start.loc = self.loc.clone();
        }
        if z.end != i64::MAX {
            end = unix_time(z.end, 0);
            end.loc = self.loc.clone();
        }
        (start, end)
    }

    /// The zero Time (January 1, year 1, 00:00:00.000000000 UTC).
    // glue: Go's zero value time.Time{}
    pub fn zero() -> Time {
        Time {
            sec: 0,
            nsec: 0,
            loc: None,
        }
    }

    /// After reports whether the time instant t is after u.
    // go-std: time.Time.After L1
    pub fn after(&self, u: &Time) -> bool {
        self.sec > u.sec || self.sec == u.sec && self.nsec > u.nsec
    }

    /// Before reports whether the time instant t is before u.
    // go-std: time.Time.Before L1
    pub fn before(&self, u: &Time) -> bool {
        self.sec < u.sec || self.sec == u.sec && self.nsec < u.nsec
    }

    /// Equal reports whether t and u represent the same time instant.
    // go-std: time.Time.Equal L1
    pub fn equal(&self, u: &Time) -> bool {
        self.sec == u.sec && self.nsec == u.nsec
    }

    /// Add returns the time t+d.
    // go-std: time.Time.Add L1
    pub fn add(&self, d: Duration) -> Time {
        let mut t = self.clone();
        let mut dsec = d / 1_000_000_000;
        let mut nsec = t.nsec + (d % 1_000_000_000) as i32;
        if nsec >= 1_000_000_000 {
            dsec += 1;
            nsec -= 1_000_000_000;
        } else if nsec < 0 {
            dsec -= 1;
            nsec += 1_000_000_000;
        }
        t.nsec = nsec;
        t.add_sec(dsec);
        t
    }

    /// Sub returns the duration t-u. If the result exceeds the maximum (or minimum)
    /// value that can be stored in a Duration, the maximum (or minimum) duration
    /// will be returned.
    // go-std: time.Time.Sub L1
    pub fn sub(&self, u: &Time) -> Duration {
        let d = self
            .sec
            .wrapping_sub(u.sec)
            .wrapping_mul(SECOND)
            .wrapping_add(i64::from(self.nsec - u.nsec));
        // Check for overflow or underflow.
        if u.add(d).equal(self) {
            d // d is correct
        } else if self.before(u) {
            MIN_DURATION // t - u is negative out of range
        } else {
            MAX_DURATION // t - u is positive out of range
        }
    }

    /// AddDate returns the time corresponding to adding the
    /// given number of years, months, and days to t.
    // go-std: time.Time.AddDate L1
    pub fn add_date(&self, years: i64, months: i64, days: i64) -> Time {
        let (year, month, day) = self.date();
        let (hour, min, sec) = self.clock();
        date(
            year + years,
            month + months,
            day + days,
            hour,
            min,
            sec,
            i64::from(self.nsec),
            &self.location(),
        )
    }

    /// Truncate returns the result of rounding t down to a multiple of d (since the zero time).
    // go-std: time.Time.Truncate L1
    pub fn truncate(&self, d: Duration) -> Time {
        if d <= 0 {
            return self.clone();
        }
        let (_, r) = div(self, d);
        self.add(-r)
    }

    /// Round returns the result of rounding t to the nearest multiple of d (since the zero time).
    /// The rounding behavior for halfway values is to round up.
    // go-std: time.Time.Round L1
    pub fn round(&self, d: Duration) -> Time {
        if d <= 0 {
            return self.clone();
        }
        let (_, r) = div(self, d);
        if less_than_half(r, d) {
            return self.add(-r);
        }
        self.add(d - r)
    }

    /// String returns the time formatted using the format string
    ///
    ///    "2006-01-02 15:04:05.999999999 -0700 MST"
    // go-std: time.Time.String L2 (Format with this one layout; no monotonic reading)
    pub fn string(&self) -> String {
        let (name, offset, abs) = self.locabs();
        let (year, month, day) = AbsDays(abs_seconds_days(abs)).date();
        let (hour, min, sec) = abs_clock(abs);
        let mut b: Vec<u8> = Vec::with_capacity(48);
        append_int(&mut b, year, 4);
        b.push(b'-');
        append_int(&mut b, month, 2);
        b.push(b'-');
        append_int(&mut b, day, 2);
        b.push(b' ');
        append_int(&mut b, hour, 2);
        b.push(b':');
        append_int(&mut b, min, 2);
        b.push(b':');
        append_int(&mut b, sec, 2);
        // stdFracSecond9: ".999999999"
        if self.nsec != 0 {
            b.push(b'.');
            append_int(&mut b, i64::from(self.nsec), 9);
            while b.last() == Some(&b'0') {
                b.pop();
            }
        }
        b.push(b' ');
        // stdNumTZ: "-0700"
        let mut zone = offset / 60; // convert to minutes
        if zone < 0 {
            b.push(b'-');
            zone = -zone;
        } else {
            b.push(b'+');
        }
        append_int(&mut b, i64::from(zone / 60), 2);
        append_int(&mut b, i64::from(zone % 60), 2);
        b.push(b' ');
        // stdTZ: "MST"
        if !name.is_empty() {
            b.extend_from_slice(name.as_bytes());
        } else {
            // No time zone known for this time, but we must print one.
            // Use the -0700 format.
            let mut zone = offset / 60; // convert to minutes
            if zone < 0 {
                b.push(b'-');
                zone = -zone;
            } else {
                b.push(b'+');
            }
            append_int(&mut b, i64::from(zone / 60), 2);
            append_int(&mut b, i64::from(zone % 60), 2);
        }
        String::from_utf8_lossy(&b).into_owned()
    }
}

impl Default for Time {
    // glue: Go's zero value time.Time{}
    fn default() -> Time {
        Time::zero()
    }
}

impl PartialEq for Time {
    /// Go's `==` on `time.Time`: the same instant representation and the same `*Location`.
    // glue: Go struct equality of time.Time (wall, ext, loc pointer)
    fn eq(&self, other: &Time) -> bool {
        self.sec == other.sec
            && self.nsec == other.nsec
            && match (&self.loc, &other.loc) {
                (None, None) => true,
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                _ => false,
            }
    }
}

impl Time {
    /// `appendFormatRFC3339` with nanoseconds: `2006-01-02T15:04:05.999999999Z07:00`.
    // go-std: time.Time.appendFormatRFC3339 L1 (nanos = true)
    fn append_format_rfc3339_nano(&self, b: &mut Vec<u8>) {
        let (_, offset, abs) = self.locabs();
        let (year, month, day) = AbsDays(abs_seconds_days(abs)).date();
        append_int(b, year, 4);
        b.push(b'-');
        append_int(b, month, 2);
        b.push(b'-');
        append_int(b, day, 2);
        b.push(b'T');
        let (hour, min, sec) = abs_clock(abs);
        append_int(b, hour, 2);
        b.push(b':');
        append_int(b, min, 2);
        b.push(b':');
        append_int(b, sec, 2);
        // appendNano with stdFracSecond9: trailing zeros trimmed, no '.' if all zero.
        if self.nsec != 0 {
            b.push(b'.');
            append_int(b, i64::from(self.nsec), 9);
            while b.last() == Some(&b'0') {
                b.pop();
            }
        }
        if offset == 0 {
            b.push(b'Z');
            return;
        }
        let mut zone = offset / 60; // convert to minutes
        if zone < 0 {
            b.push(b'-');
            zone = -zone;
        } else {
            b.push(b'+');
        }
        append_int(b, i64::from(zone / 60), 2);
        b.push(b':');
        append_int(b, i64::from(zone % 60), 2);
    }

    /// `appendStrictRFC3339`: the RFC 3339 form, or an error when it would not be valid RFC
    /// 3339 (the year is not 4 digits wide, or the zone offset is 24 hours or more).
    // go-std: time.Time.appendStrictRFC3339 L1
    pub fn append_strict_rfc3339(&self) -> Result<String, String> {
        let mut b = Vec::new();
        self.append_format_rfc3339_nano(&mut b);
        let num2 = |b: &[u8]| 10 * (i32::from(b[0]) - 48) + (i32::from(b[1]) - 48);
        if b[4] != b'-' {
            return Err("year outside of range [0,9999]".to_owned());
        }
        if *b.last().unwrap_or(&0) != b'Z' {
            let c = b[b.len() - 6];
            if c.is_ascii_digit() || num2(&b[b.len() - 5..]) >= 24 {
                return Err("timezone hour outside of range [0,23]".to_owned());
            }
        }
        Ok(String::from_utf8_lossy(&b).into_owned())
    }

    /// `parseStrictRFC3339` (`Time.UnmarshalJSON`'s parser): the RFC 3339 fast path, else
    /// `Parse(RFC3339, s)`. Go 1.25 disables the strict re-checks after `Parse` (go.dev/issue
    /// 54580, `case true: return t, nil`), so what `Parse` accepts is accepted (a one-digit
    /// hour, a comma before the fraction, a `+24:00` offset).
    // go-std: time.parseStrictRFC3339 L1
    pub fn parse_strict_rfc3339(s: &str) -> Result<Time, String> {
        match parse_rfc3339(s.as_bytes()) {
            Some(t) => Ok(t),
            None => parse_layout_rfc3339(s.as_bytes()),
        }
    }
}

/// `parseRFC3339`: the fast path of RFC 3339 parsing. The zone of a non-`Z` time is a fixed
/// zone with the parsed offset (Go uses `Local` when its offset matches, which prints the
/// same).
// go-std: time.parseRFC3339 L2 (fixed zone instead of Local)
fn parse_rfc3339(b: &[u8]) -> Option<Time> {
    {
        let fail = || None;
        let mut ok = true;
        let mut parse_uint = |s: &[u8], min: i64, max: i64| -> i64 {
            let mut x: i64 = 0;
            for &c in s {
                if !c.is_ascii_digit() {
                    ok = false;
                    return min;
                }
                x = x * 10 + i64::from(c - b'0');
            }
            if x < min || max < x {
                ok = false;
                return min;
            }
            x
        };
        if b.len() < "2006-01-02T15:04:05".len() {
            return fail();
        }
        let year = parse_uint(&b[0..4], 0, 9999);
        let month = parse_uint(&b[5..7], 1, 12);
        let day = parse_uint(&b[8..10], 1, i64::from(super::days_in(month as i32, year)));
        let hour = parse_uint(&b[11..13], 0, 23);
        let min = parse_uint(&b[14..16], 0, 59);
        let sec = parse_uint(&b[17..19], 0, 59);
        if !ok || !(b[4] == b'-' && b[7] == b'-' && b[10] == b'T' && b[13] == b':' && b[16] == b':')
        {
            return fail();
        }
        let mut rest = &b[19..];
        let mut nsec: i64 = 0;
        if rest.len() >= 2 && rest[0] == b'.' && rest[1].is_ascii_digit() {
            let mut n = 2;
            while n < rest.len() && rest[n].is_ascii_digit() {
                n += 1;
            }
            // parseNanoseconds: the first 9 digits, scaled.
            let digits = &rest[1..n];
            let mut v: i64 = 0;
            for (i, &c) in digits.iter().enumerate() {
                if i == 9 {
                    break;
                }
                v = v * 10 + i64::from(c - b'0');
            }
            for _ in digits.len().min(9)..9 {
                v *= 10;
            }
            nsec = v;
            rest = &rest[n..];
        }
        let mut t = date(year, month, day, hour, min, sec, nsec, &super::utc());
        if !(rest.len() == 1 && rest[0] == b'Z') {
            if rest.len() != "-07:00".len() {
                return fail();
            }
            let mut ok2 = true;
            let mut pu = |s: &[u8], min: i64, max: i64| -> i64 {
                let mut x: i64 = 0;
                for &c in s {
                    if !c.is_ascii_digit() {
                        ok2 = false;
                        return min;
                    }
                    x = x * 10 + i64::from(c - b'0');
                }
                if x < min || max < x {
                    ok2 = false;
                    return min;
                }
                x
            };
            let hr = pu(&rest[1..3], 0, 23);
            let mm = pu(&rest[4..6], 0, 59);
            if !ok2 || !((rest[0] == b'-' || rest[0] == b'+') && rest[3] == b':') {
                return fail();
            }
            let mut zone_offset = (hr * 60 + mm) * 60;
            if rest[0] == b'-' {
                zone_offset = -zone_offset;
            }
            t.add_sec(-zone_offset);
            t.set_loc(super::fixed_zone("", zone_offset as i32));
        }
        Some(t)
    }
}

/// The layout `time.RFC3339`.
const RFC3339: &str = "2006-01-02T15:04:05Z07:00";

/// The layout elements of `RFC3339` that `parse` handles (`nextStdChunk`).
#[derive(Clone, Copy)]
enum RfcStd {
    LongYear,
    ZeroMonth,
    ZeroDay,
    Hour,
    ZeroMinute,
    ZeroSecond,
    Iso8601ColonTz,
}

/// `parse(RFC3339, value, UTC, Local)`, i.e. `time.Parse(time.RFC3339, value)`: the general
/// layout parser walked over the chunks of the RFC 3339 layout (prefix, element, element
/// text), with Go's error values (`ParseError`).
// go-std: time.parse L2 (the RFC3339 layout only; a numeric zone is a fixed zone, not Local)
fn parse_layout_rfc3339(avalue: &[u8]) -> Result<Time, String> {
    // ParseError.Error.
    let cannot = |layout_elem: &str, value_elem: &[u8]| {
        format!(
            "parsing time {} as {}: cannot parse {} as {}",
            time_quote(avalue),
            time_quote(RFC3339.as_bytes()),
            time_quote(value_elem),
            time_quote(layout_elem.as_bytes())
        )
    };
    let message = |msg: &str| format!("parsing time {}{msg}", time_quote(avalue));
    const CHUNKS: [(&str, RfcStd, &str); 7] = [
        ("", RfcStd::LongYear, "2006"),
        ("-", RfcStd::ZeroMonth, "01"),
        ("-", RfcStd::ZeroDay, "02"),
        ("T", RfcStd::Hour, "15"),
        (":", RfcStd::ZeroMinute, "04"),
        (":", RfcStd::ZeroSecond, "05"),
        ("", RfcStd::Iso8601ColonTz, "Z07:00"),
    ];
    let mut value = avalue;
    let (mut year, mut month, mut day) = (0i64, -1i64, -1i64);
    let (mut hour, mut min, mut sec, mut nsec) = (0i64, 0i64, 0i64, 0i64);
    let mut utc_zone = false;
    let mut zone_offset: i64 = -1;
    for (prefix, std, stdstr) in CHUNKS {
        value = match skip(value, prefix.as_bytes()) {
            Ok(v) => v,
            Err(v) => return Err(cannot(prefix, v)),
        };
        let hold = value;
        let mut range_err = "";
        let mut bad = false;
        match std {
            RfcStd::LongYear => {
                if value.len() < 4 || !is_digit(value, 0) {
                    bad = true;
                } else {
                    let p = &value[..4];
                    value = &value[4..];
                    match atoi(p) {
                        Some(x) => year = x,
                        None => bad = true,
                    }
                }
            }
            RfcStd::ZeroMonth => match getnum(value, true) {
                Some((n, rest)) => {
                    month = n;
                    value = rest;
                    if month <= 0 || 12 < month {
                        range_err = "month";
                    }
                }
                None => bad = true,
            },
            RfcStd::ZeroDay => match getnum(value, true) {
                Some((n, rest)) => {
                    day = n;
                    value = rest;
                }
                None => bad = true,
            },
            RfcStd::Hour => match getnum(value, false) {
                Some((n, rest)) => {
                    hour = n;
                    value = rest;
                    if 24 <= hour {
                        range_err = "hour";
                    }
                }
                None => bad = true,
            },
            RfcStd::ZeroMinute => match getnum(value, true) {
                Some((n, rest)) => {
                    min = n;
                    value = rest;
                    if 60 <= min {
                        range_err = "minute";
                    }
                }
                None => bad = true,
            },
            RfcStd::ZeroSecond => match getnum(value, true) {
                Some((n, rest)) => {
                    sec = n;
                    value = rest;
                    if 60 <= sec {
                        range_err = "second";
                    } else if value.len() >= 2
                        && (value[0] == b'.' || value[0] == b',')
                        && is_digit(value, 1)
                    {
                        // A fractional second the layout does not have (the next chunk
                        // is the zone, not stdFracSecond0/9): parseNanoseconds.
                        let mut n = 2;
                        while n < value.len() && is_digit(value, n) {
                            n += 1;
                        }
                        let nbytes = n.min(10);
                        let mut ns = atoi(&value[1..nbytes]).unwrap_or(0);
                        for _ in nbytes..10 {
                            ns *= 10;
                        }
                        nsec = ns;
                        value = &value[n..];
                    }
                }
                None => bad = true,
            },
            RfcStd::Iso8601ColonTz => {
                if !value.is_empty() && value[0] == b'Z' {
                    value = &value[1..];
                    utc_zone = true;
                } else if value.len() < 6 || value[3] != b':' {
                    bad = true;
                } else {
                    let (sign, hourb, minb) = (value[0], &value[1..3], &value[4..6]);
                    value = &value[6..];
                    let hr = getnum(hourb, true);
                    let mm = if hr.is_some() {
                        getnum(minb, true)
                    } else {
                        None
                    };
                    let (h, m) = (hr.map_or(0, |x| x.0), mm.map_or(0, |x| x.0));
                    bad = mm.is_none();
                    if h > 24 {
                        range_err = "time zone offset hour";
                    }
                    if m > 60 {
                        range_err = "time zone offset minute";
                    }
                    zone_offset = (h * 60 + m) * 60;
                    match sign {
                        b'+' => {}
                        b'-' => zone_offset = -zone_offset,
                        _ => bad = true,
                    }
                }
            }
        }
        if !range_err.is_empty() {
            return Err(message(&format!(": {range_err} out of range")));
        }
        if bad {
            return Err(cannot(stdstr, hold));
        }
    }
    if !value.is_empty() {
        return Err(message(&format!(": extra text: {}", time_quote(value))));
    }
    if day < 1 || day > i64::from(super::days_in(month as i32, year)) {
        return Err(message(": day out of range"));
    }
    if utc_zone {
        return Ok(date(year, month, day, hour, min, sec, nsec, &super::utc()));
    }
    let mut t = date(year, month, day, hour, min, sec, nsec, &super::utc());
    t.add_sec(-zone_offset);
    t.set_loc(super::fixed_zone("", zone_offset as i32));
    Ok(t)
}

/// `isDigit(s, i)`.
// go-std: time.isDigit L1
fn is_digit(s: &[u8], i: usize) -> bool {
    s.get(i).is_some_and(u8::is_ascii_digit)
}

/// `getnum`: one or two digits (`fixed`: exactly two); `None` is `errBad`.
// go-std: time.getnum L1
fn getnum(s: &[u8], fixed: bool) -> Option<(i64, &[u8])> {
    if !is_digit(s, 0) {
        return None;
    }
    if !is_digit(s, 1) {
        if fixed {
            return None;
        }
        return Some((i64::from(s[0] - b'0'), &s[1..]));
    }
    Some((
        i64::from(s[0] - b'0') * 10 + i64::from(s[1] - b'0'),
        &s[2..],
    ))
}

/// `skip`: removes the layout prefix from the value (the RFC 3339 layout has no spaces);
/// `Err` carries the value at the mismatch.
// go-std: time.skip L2 (no space handling: the RFC3339 layout has no spaces)
fn skip<'a>(mut value: &'a [u8], mut prefix: &[u8]) -> Result<&'a [u8], &'a [u8]> {
    while !prefix.is_empty() {
        if value.is_empty() || value[0] != prefix[0] {
            return Err(value);
        }
        prefix = &prefix[1..];
        value = &value[1..];
    }
    Ok(value)
}

/// `atoi`: an optionally signed decimal integer without trailing text (`leadingInt`'s
/// overflow check at 2^63); `None` is `errAtoi`.
// go-std: time.atoi L1
// go-std: time.leadingInt L1
fn atoi(mut s: &[u8]) -> Option<i64> {
    let mut neg = false;
    if !s.is_empty() && (s[0] == b'-' || s[0] == b'+') {
        neg = s[0] == b'-';
        s = &s[1..];
    }
    let mut x: u64 = 0;
    let mut i = 0;
    while i < s.len() && s[i].is_ascii_digit() {
        if x > (1 << 63) / 10 {
            return None;
        }
        x = x * 10 + u64::from(s[i] - b'0');
        if x > 1 << 63 {
            return None;
        }
        i += 1;
    }
    if i < s.len() {
        return None;
    }
    let x = x as i64; // int(q): 2^63 wraps, as in Go
    Some(if neg { x.wrapping_neg() } else { x })
}

/// `quote` of package time (used by `ParseError`): every byte of a non-ASCII or control
/// character, and every invalid byte, as `\xNN`; `"` and `\` escaped.
// go-std: time.quote L1
fn time_quote(s: &[u8]) -> String {
    let mut buf = String::with_capacity(s.len() + 2);
    buf.push('"');
    for &b in s {
        if !(b' '..0x80).contains(&b) {
            buf.push_str(&format!("\\x{b:02x}"));
        } else {
            if b == b'"' || b == b'\\' {
                buf.push('\\');
            }
            buf.push(b as char);
        }
    }
    buf.push('"');
    buf
}

/// appendInt appends the decimal form of x to b and returns the result.
/// If the decimal form (excluding sign) is shorter than width, the result is padded with leading 0's.
/// Duplicates functionality in strconv, but avoids dependency.
// go-std: time.appendInt L1
fn append_int(b: &mut Vec<u8>, x: i64, width: usize) {
    let mut u = x as u64;
    if x < 0 {
        b.push(b'-');
        u = x.unsigned_abs();
    }
    let digits = u.to_string();
    for _ in digits.len()..width {
        b.push(b'0');
    }
    b.extend_from_slice(digits.as_bytes());
}

/// div divides t by d and returns the quotient parity and remainder.
/// We don't use the quotient parity anymore (round half up instead of round to even)
/// but it's still here in case we change our minds.
// go-std: time.div L1
fn div(t: &Time, d: Duration) -> (i64, Duration) {
    let mut qmod2: i64;
    let r: Duration;
    let mut neg = false;
    let mut nsec = t.nsec;
    let mut sec = t.sec;
    if sec < 0 {
        // Operate on absolute value.
        neg = true;
        sec = sec.wrapping_neg();
        nsec = -nsec;
        if nsec < 0 {
            nsec += 1_000_000_000;
            sec -= 1; // sec >= 1 before the -- so safe
        }
    }

    if d < SECOND && SECOND % (d + d) == 0 {
        // Special case: d is even number of nanoseconds, divides Second.
        qmod2 = i64::from(nsec / d as i32) & 1;
        r = Duration::from(nsec % d as i32);
    } else if d % SECOND == 0 {
        // Special case: d is a multiple of Second.
        let d1 = d / SECOND;
        qmod2 = (sec / d1) & 1;
        r = (sec % d1) * SECOND + Duration::from(nsec);
    } else {
        // General case.
        // This could be faster if more cleverness were applied,
        // but it's really only here to avoid special case restrictions in the API.
        // No one will care about these cases.

        // Compute nanoseconds as 128-bit number.
        let sec = sec as u64;
        let mut tmp = (sec >> 32) * 1_000_000_000;
        let mut u1 = tmp >> 32;
        let mut u0 = tmp << 32;
        tmp = (sec & 0xFFFF_FFFF) * 1_000_000_000;
        let (mut u0x, mut nu0) = (u0, u0.wrapping_add(tmp));
        u0 = nu0;
        if u0 < u0x {
            u1 += 1;
        }
        (u0x, nu0) = (u0, u0.wrapping_add(nsec as u64));
        u0 = nu0;
        if u0 < u0x {
            u1 += 1;
        }

        // Compute remainder by subtracting r<<k for decreasing k.
        // Quotient parity is whether we subtract on last round.
        let mut d1 = d as u64;
        while d1 >> 63 != 1 {
            d1 <<= 1;
        }
        let mut d0: u64 = 0;
        loop {
            qmod2 = 0;
            if u1 > d1 || u1 == d1 && u0 >= d0 {
                // subtract
                qmod2 = 1;
                (u0x, nu0) = (u0, u0.wrapping_sub(d0));
                u0 = nu0;
                if u0 > u0x {
                    u1 = u1.wrapping_sub(1);
                }
                u1 = u1.wrapping_sub(d1);
            }
            if d1 == 0 && d0 == d as u64 {
                break;
            }
            d0 >>= 1;
            d0 |= (d1 & 1) << 63;
            d1 >>= 1;
        }
        r = u0 as Duration;
    }

    if neg && r != 0 {
        // If input was negative and not an exact multiple of d, we computed q, r such that
        //    q*d + r = -t
        // But the right answers are given by -(q-1), d-r:
        //    q*d + r = -t
        //    -q*d - r = t
        //    -(q-1)*d + (d - r) = t
        qmod2 ^= 1;
        return (qmod2, d - r);
    }
    (qmod2, r)
}
