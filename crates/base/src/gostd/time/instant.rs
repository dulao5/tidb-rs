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
    fn lookup(&self) -> super::ZoneInfo {
        match &self.loc {
            Some(l) => l.lookup(self.unix_sec()),
            None => utc().lookup(self.unix_sec()),
        }
    }

    /// absSec returns the time t as an absolute seconds, adjusted by the zone offset.
    /// It is called when computing a presentation property like Month or Hour.
    /// We'd rather call it abs, but there are linknames to abs that make that problematic.
    /// See timeAbs below.
    // go-std: time.Time.absSec L1 (without the lookup cache)
    fn abs_sec(&self) -> u64 {
        let mut sec = self.unix_sec();
        if let Some(l) = &self.loc {
            sec = sec.wrapping_add(i64::from(l.lookup(sec).offset));
        }
        (sec as u64).wrapping_add(UNIX_TO_ABSOLUTE as u64)
    }

    /// locabs is a combination of the Zone and abs methods,
    /// extracting both return values from a single zone lookup.
    // go-std: time.Time.locabs L1 (without the lookup cache)
    fn locabs(&self) -> (String, i32, u64) {
        let mut sec = self.unix_sec();
        let (name, offset) = match &self.loc {
            Some(l) => {
                let z = l.lookup(sec);
                sec = sec.wrapping_add(i64::from(z.offset));
                (z.name, z.offset)
            }
            None => ("UTC".to_owned(), 0),
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
