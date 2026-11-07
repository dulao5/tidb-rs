//! go-file: $GOROOT/src/time/zoneinfo.go
//! go-file: $GOROOT/src/time/zoneinfo_read.go
//! go-file: $GOROOT/src/time/zoneinfo_unix.go
//!
//! Go's `time.Location`: time zones loaded from the system tz database (TZif files) and
//! the POSIX TZ rule string at their end, with Go's lookup and DST resolution. TiDB keeps
//! `time.Location`s in `types.Context` and the session; DST behaviour (gaps, overlaps, the
//! zone abbreviations in `Time.String`) must be Go's, so this is a port, not chrono-tz
//! (whose compiled-in tz database and gap handling differ). See `specs/D01.3-time/spec.md`.
//!
//! Not ported: the `time/tzdata` (embedded) and `$GOROOT/lib/time/zoneinfo.zip` fallbacks
//! (TiDB links neither; a name missing from the system database is `unknown time zone`).
//! `$ZONEINFO` naming a zip file is supported, as in Go.

#![allow(clippy::needless_late_init)]

use std::sync::{Arc, LazyLock};

use super::{SECONDS_PER_DAY, SECONDS_PER_HOUR, SECONDS_PER_MINUTE, days_before, days_in, is_leap};

/// `time.Location`. Shared as `Arc<Location>`; Go compares `*Location` pointers (e.g.
/// `tz != time.UTC`), so identity is `Arc::ptr_eq` (see [`is_utc`]).
#[derive(Debug, Clone)]
pub struct Location {
    name: String,
    zone: Vec<Zone>,
    tx: Vec<ZoneTrans>,
    /// The TZ string from the end of the TZif file, for times after the last transition.
    extend: String,

    // Most lookups will be for the current time.
    // To avoid the binary search through tx, keep a
    // static one-element cache that gives the correct
    // zone for the time when the Location was created.
    // if cacheStart <= t < cacheEnd,
    // lookup can return cacheZone.
    // The units for cacheStart and cacheEnd are seconds
    // since January 1, 1970 UTC, to match the argument
    // to lookup.
    cache_start: i64,
    cache_end: i64,
    cache_zone: Option<Zone>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Zone {
    name: String,
    offset: i32,
    is_dst: bool,
}

#[derive(Debug, Clone, Copy)]
struct ZoneTrans {
    when: i64,
    index: u8,
}

const ALPHA: i64 = i64::MIN;
const OMEGA: i64 = i64::MAX;

/// The result of `Location.lookup` (the name borrows from the Location).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZoneInfo<'a> {
    pub name: &'a str,
    pub offset: i32,
    pub start: i64,
    pub end: i64,
    pub is_dst: bool,
}

static UTC: LazyLock<Arc<Location>> = LazyLock::new(|| {
    Arc::new(Location {
        name: "UTC".to_owned(),
        zone: Vec::new(),
        tx: Vec::new(),
        extend: String::new(),
        cache_start: 0,
        cache_end: 0,
        cache_zone: None,
    })
});

static LOCAL: LazyLock<Arc<Location>> = LazyLock::new(|| Arc::new(init_local()));

/// `time.UTC`.
// go-std: time.UTC L1
pub fn utc() -> Arc<Location> {
    Arc::clone(&UTC)
}

/// `time.Local`, initialised on first use from `$TZ` / `/etc/localtime` like Go.
// go-std: time.Local L1
pub fn local() -> Arc<Location> {
    Arc::clone(&LOCAL)
}

/// `loc == time.UTC` (pointer comparison in Go).
// glue: Go *Location pointer equality with time.UTC
pub fn is_utc(loc: &Arc<Location>) -> bool {
    Arc::ptr_eq(loc, &UTC)
}

/// `loc == time.Local`.
// glue: Go *Location pointer equality with time.Local
pub fn is_local(loc: &Arc<Location>) -> bool {
    Arc::ptr_eq(loc, &LOCAL)
}

impl Location {
    /// String returns a descriptive name for the time zone information,
    /// corresponding to the name argument to LoadLocation or FixedZone.
    // go-std: time.Location.String L1
    pub fn string(&self) -> &str {
        &self.name
    }

    /// lookup returns information about the time zone in use at an instant in time
    /// expressed as seconds since January 1, 1970 00:00:00 UTC.
    // go-std: time.Location.lookup L1
    pub fn lookup(&self, sec: i64) -> ZoneInfo<'_> {
        if self.zone.is_empty() {
            return ZoneInfo {
                name: "UTC",
                offset: 0,
                start: ALPHA,
                end: OMEGA,
                is_dst: false,
            };
        }

        if let Some(zone) = &self.cache_zone
            && self.cache_start <= sec
            && sec < self.cache_end
        {
            return ZoneInfo {
                name: &zone.name,
                offset: zone.offset,
                start: self.cache_start,
                end: self.cache_end,
                is_dst: zone.is_dst,
            };
        }
        self.lookup_uncached(sec)
    }

    /// `lookup` without the cache check (the rest of Go's `lookup`); public so that tests
    /// can check that the cache answers as the search does.
    // go-std: time.Location.lookup L2 (the part after the cache check)
    #[doc(hidden)]
    pub fn lookup_uncached(&self, sec: i64) -> ZoneInfo<'_> {
        if self.zone.is_empty() {
            return ZoneInfo {
                name: "UTC",
                offset: 0,
                start: ALPHA,
                end: OMEGA,
                is_dst: false,
            };
        }

        if self.tx.is_empty() || sec < self.tx[0].when {
            let zone = &self.zone[self.lookup_first_zone()];
            let end = if self.tx.is_empty() {
                OMEGA
            } else {
                self.tx[0].when
            };
            return ZoneInfo {
                name: &zone.name,
                offset: zone.offset,
                start: ALPHA,
                end,
                is_dst: zone.is_dst,
            };
        }

        // Binary search for entry with largest time <= sec.
        // Not using sort.Search to avoid dependencies.
        let tx = &self.tx;
        let mut end = OMEGA;
        let mut lo = 0usize;
        let mut hi = tx.len();
        while hi - lo > 1 {
            let m = (lo + hi) >> 1;
            let lim = tx[m].when;
            if sec < lim {
                end = lim;
                hi = m;
            } else {
                lo = m;
            }
        }
        let zone = &self.zone[usize::from(tx[lo].index)];
        let start = tx[lo].when;

        // If we're at the end of the known zone transitions,
        // try the extend string.
        if lo == tx.len() - 1
            && !self.extend.is_empty()
            && let Some(z) = tzset(&self.extend, start, sec)
        {
            return z;
        }
        ZoneInfo {
            name: &zone.name,
            offset: zone.offset,
            start,
            end,
            is_dst: zone.is_dst,
        }
    }

    /// lookupFirstZone returns the index of the time zone to use for times
    /// before the first transition time, or when there are no transition
    /// times.
    // go-std: time.Location.lookupFirstZone L1
    fn lookup_first_zone(&self) -> usize {
        // Case 1.
        if !self.first_zone_used() {
            return 0;
        }

        // Case 2.
        if !self.tx.is_empty() && self.zone[usize::from(self.tx[0].index)].is_dst {
            let mut zi = i64::from(self.tx[0].index) - 1;
            while zi >= 0 {
                if !self.zone[zi as usize].is_dst {
                    return zi as usize;
                }
                zi -= 1;
            }
        }

        // Case 3.
        for (zi, z) in self.zone.iter().enumerate() {
            if !z.is_dst {
                return zi;
            }
        }

        // Case 4.
        0
    }

    /// firstZoneUsed reports whether the first zone is used by some
    /// transition.
    // go-std: time.Location.firstZoneUsed L1
    fn first_zone_used(&self) -> bool {
        self.tx.iter().any(|tx| tx.index == 0)
    }
}

/// FixedZone returns a Location that always uses
/// the given zone name and offset (seconds east of UTC).
// go-std: time.FixedZone L2 (the cache of unnamed whole-hour zones is not kept: it only
// shares pointers, and no caller compares fixed-zone pointers)
pub fn fixed_zone(name: &str, offset: i32) -> Arc<Location> {
    Arc::new(Location {
        name: name.to_owned(),
        zone: vec![Zone {
            name: name.to_owned(),
            offset,
            is_dst: false,
        }],
        tx: vec![ZoneTrans {
            when: ALPHA,
            index: 0,
        }],
        extend: String::new(),
        cache_start: ALPHA,
        cache_end: OMEGA,
        cache_zone: Some(Zone {
            name: name.to_owned(),
            offset,
            is_dst: false,
        }),
    })
}

/// tzset takes a timezone string like the one found in the TZ environment
/// variable, the time of the last time zone transition expressed as seconds
/// since January 1, 1970 00:00:00 UTC, and a time expressed the same way.
/// We call this a tzset string since in C the function tzset reads TZ.
/// The return values are as for lookup, plus ok which reports whether the
/// parse succeeded.
// go-std: time.tzset L1
fn tzset(s: &str, last_tx_sec: i64, sec: i64) -> Option<ZoneInfo<'_>> {
    let (mut std_name, s) = tzset_name(s)?;
    let (std_offset, s) = tzset_offset(s)?;
    let mut std_offset = -std_offset;

    if s.is_empty() || s.as_bytes()[0] == b',' {
        // No daylight savings time.
        return Some(ZoneInfo {
            name: std_name,
            offset: std_offset,
            start: last_tx_sec,
            end: OMEGA,
            is_dst: false,
        });
    }

    let (mut dst_name, s) = tzset_name(s)?;
    let (mut dst_offset, mut s) = if s.is_empty() || s.as_bytes()[0] == b',' {
        (std_offset + SECONDS_PER_HOUR as i32, s)
    } else {
        let (off, rest) = tzset_offset(s)?;
        (-off, rest) // as with stdOffset, above
    };

    if s.is_empty() {
        // Default DST rules per tzcode.
        s = ",M3.2.0,M11.1.0";
    }
    // The TZ definition does not mention ';' here but tzcode accepts it.
    let b0 = s.as_bytes()[0];
    if b0 != b',' && b0 != b';' {
        return None;
    }
    s = &s[1..];

    let (start_rule, s) = tzset_rule(s)?;
    if s.is_empty() || s.as_bytes()[0] != b',' {
        return None;
    }
    let (end_rule, s) = tzset_rule(&s[1..])?;
    if !s.is_empty() {
        return None;
    }

    // Compute start of year in seconds since Unix epoch,
    // and seconds since then to get to sec.
    let (year, yday) = super::abs_days(super::abs_seconds_days(
        (sec as u64).wrapping_add(super::UNIX_TO_ABSOLUTE as u64),
    ))
    .year_yday();
    let ysec = (yday - 1) * SECONDS_PER_DAY + sec % SECONDS_PER_DAY;
    let ystart = sec - ysec;

    let mut start_sec = i64::from(tzrule_time(year, &start_rule, std_offset));
    let mut end_sec = i64::from(tzrule_time(year, &end_rule, dst_offset));
    let (mut dst_is_dst, mut std_is_dst) = (true, false);
    // Note: this is a flipping of "DST" and "STD" while retaining the labels
    // This happens in southern hemispheres. The labelling here thus is a little
    // inconsistent with the goal.
    if end_sec < start_sec {
        std::mem::swap(&mut start_sec, &mut end_sec);
        std::mem::swap(&mut std_name, &mut dst_name);
        std::mem::swap(&mut std_offset, &mut dst_offset);
        std::mem::swap(&mut std_is_dst, &mut dst_is_dst);
    }

    // The start and end values that we return are accurate
    // close to a daylight savings transition, but are otherwise
    // just the start and end of the year. That suffices for
    // the only caller that cares, which is Date.
    Some(if ysec < start_sec {
        ZoneInfo {
            name: std_name,
            offset: std_offset,
            start: ystart,
            end: start_sec + ystart,
            is_dst: std_is_dst,
        }
    } else if ysec >= end_sec {
        ZoneInfo {
            name: std_name,
            offset: std_offset,
            start: end_sec + ystart,
            end: ystart + 365 * SECONDS_PER_DAY,
            is_dst: std_is_dst,
        }
    } else {
        ZoneInfo {
            name: dst_name,
            offset: dst_offset,
            start: start_sec + ystart,
            end: end_sec + ystart,
            is_dst: dst_is_dst,
        }
    })
}

/// tzsetName returns the timezone name at the start of the tzset string s,
/// and the remainder of s, and reports whether the parsing is OK.
// go-std: time.tzsetName L1
fn tzset_name(s: &str) -> Option<(&str, &str)> {
    if s.is_empty() {
        return None;
    }
    if !s.starts_with('<') {
        for (i, r) in s.char_indices() {
            match r {
                '0'..='9' | ',' | '-' | '+' => {
                    if i < 3 {
                        return None;
                    }
                    return Some((&s[..i], &s[i..]));
                }
                _ => {}
            }
        }
        if s.len() < 3 {
            return None;
        }
        Some((s, ""))
    } else {
        for (i, r) in s.char_indices() {
            if r == '>' {
                return Some((&s[1..i], &s[i + 1..]));
            }
        }
        None
    }
}

/// tzsetOffset returns the timezone offset at the start of the tzset string s,
/// and the remainder of s, and reports whether the parsing is OK.
/// The timezone offset is returned as a number of seconds.
// go-std: time.tzsetOffset L1
fn tzset_offset(s: &str) -> Option<(i32, &str)> {
    if s.is_empty() {
        return None;
    }
    let mut s = s;
    let mut neg = false;
    if let Some(rest) = s.strip_prefix('+') {
        s = rest;
    } else if let Some(rest) = s.strip_prefix('-') {
        s = rest;
        neg = true;
    }

    // The tzdata code permits values up to 24 * 7 here,
    // although POSIX does not.
    let (hours, rest) = tzset_num(s, 0, 24 * 7)?;
    s = rest;
    let mut off = hours * SECONDS_PER_HOUR as i32;
    if s.is_empty() || s.as_bytes()[0] != b':' {
        if neg {
            off = -off;
        }
        return Some((off, s));
    }

    let (mins, rest) = tzset_num(&s[1..], 0, 59)?;
    s = rest;
    off += mins * SECONDS_PER_MINUTE as i32;
    if s.is_empty() || s.as_bytes()[0] != b':' {
        if neg {
            off = -off;
        }
        return Some((off, s));
    }

    let (secs, rest) = tzset_num(&s[1..], 0, 59)?;
    s = rest;
    off += secs;

    if neg {
        off = -off;
    }
    Some((off, s))
}

/// ruleKind is the kinds of rules that can be seen in a tzset string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuleKind {
    Julian,
    Doy,
    MonthWeekDay,
}

/// rule is a rule read from a tzset string.
#[derive(Debug, Clone, Copy)]
struct Rule {
    kind: RuleKind,
    day: i32,
    week: i32,
    mon: i32,
    time: i32, // transition time
}

/// tzsetRule parses a rule from a tzset string.
/// It returns the rule, and the remainder of the string, and reports success.
// go-std: time.tzsetRule L1
fn tzset_rule(s: &str) -> Option<(Rule, &str)> {
    let mut r = Rule {
        kind: RuleKind::Julian,
        day: 0,
        week: 0,
        mon: 0,
        time: 0,
    };
    if s.is_empty() {
        return None;
    }
    let mut s = s;
    if let Some(rest) = s.strip_prefix('J') {
        let (jday, rest) = tzset_num(rest, 1, 365)?;
        s = rest;
        r.kind = RuleKind::Julian;
        r.day = jday;
    } else if let Some(rest) = s.strip_prefix('M') {
        let (mon, rest) = tzset_num(rest, 1, 12)?;
        if rest.is_empty() || rest.as_bytes()[0] != b'.' {
            return None;
        }
        let (week, rest) = tzset_num(&rest[1..], 1, 5)?;
        if rest.is_empty() || rest.as_bytes()[0] != b'.' {
            return None;
        }
        let (day, rest) = tzset_num(&rest[1..], 0, 6)?;
        s = rest;
        r.kind = RuleKind::MonthWeekDay;
        r.day = day;
        r.week = week;
        r.mon = mon;
    } else {
        let (day, rest) = tzset_num(s, 0, 365)?;
        s = rest;
        r.kind = RuleKind::Doy;
        r.day = day;
    }

    if s.is_empty() || s.as_bytes()[0] != b'/' {
        r.time = 2 * SECONDS_PER_HOUR as i32; // 2am is the default
        return Some((r, s));
    }

    let (offset, s) = tzset_offset(&s[1..])?;
    r.time = offset;

    Some((r, s))
}

/// tzsetNum parses a number from a tzset string.
/// It returns the number, and the remainder of the string, and reports success.
/// The number must be between min and max.
// go-std: time.tzsetNum L1
fn tzset_num(s: &str, min: i32, max: i32) -> Option<(i32, &str)> {
    if s.is_empty() {
        return None;
    }
    let mut num: i32 = 0;
    for (i, r) in s.char_indices() {
        if !r.is_ascii_digit() {
            if i == 0 || num < min {
                return None;
            }
            return Some((num, &s[i..]));
        }
        num *= 10;
        num += r as i32 - '0' as i32;
        if num > max {
            return None;
        }
    }
    if num < min {
        return None;
    }
    Some((num, ""))
}

/// tzruleTime takes a year, a rule, and a timezone offset,
/// and returns the number of seconds since the start of the year
/// that the rule takes effect.
// go-std: time.tzruleTime L1
fn tzrule_time(year: i64, r: &Rule, off: i32) -> i32 {
    let s: i32;
    match r.kind {
        RuleKind::Julian => {
            let mut v = (r.day - 1) * SECONDS_PER_DAY as i32;
            if is_leap(year) && r.day >= 60 {
                v += SECONDS_PER_DAY as i32;
            }
            s = v;
        }
        RuleKind::Doy => {
            s = r.day * SECONDS_PER_DAY as i32;
        }
        RuleKind::MonthWeekDay => {
            // Zeller's Congruence.
            let m1 = (r.mon + 9) % 12 + 1;
            let mut yy0 = year;
            if r.mon <= 2 {
                yy0 -= 1;
            }
            let yy1 = yy0 / 100;
            let yy2 = yy0 % 100;
            let mut dow =
                ((26 * i64::from(m1) - 2) / 10 + 1 + yy2 + yy2 / 4 + yy1 / 4 - 2 * yy1) % 7;
            if dow < 0 {
                dow += 7;
            }
            // Now dow is the day-of-week of the first day of r.mon.
            // Get the day-of-month of the first "dow" day.
            let mut d = i64::from(r.day) - dow;
            if d < 0 {
                d += 7;
            }
            for _ in 1..r.week {
                if d + 7 >= i64::from(days_in(r.mon, year)) {
                    break;
                }
                d += 7;
            }
            d += i64::from(days_before(r.mon));
            if is_leap(year) && r.mon > 2 {
                d += 1;
            }
            s = d as i32 * SECONDS_PER_DAY as i32;
        }
    }

    s + r.time - off
}

/// `time: invalid location name`.
const ERR_LOCATION: &str = "time: invalid location name";
/// `malformed time zone information`.
const ERR_BAD_DATA: &str = "malformed time zone information";

/// LoadLocation returns the Location with the given name.
///
/// If the name is "" or "UTC", LoadLocation returns UTC.
/// If the name is "Local", LoadLocation returns Local.
///
/// Otherwise, the name is taken to be a location name corresponding to a file
/// in the IANA Time Zone database, such as "America/New_York".
///
/// The error is Go's message.
// go-std: time.LoadLocation L2 (system tz database only; see the file comment)
pub fn load_location(name: &str) -> Result<Arc<Location>, String> {
    if name.is_empty() || name == "UTC" {
        return Ok(utc());
    }
    if name == "Local" {
        return Ok(local());
    }
    if contains_dot_dot(name) || name.starts_with('/') || name.starts_with('\\') {
        // No valid IANA Time Zone name contains a single dot,
        // much less dot dot. Likewise, none begin with a slash.
        return Err(ERR_LOCATION.to_owned());
    }

    let mut first_err: Option<String> = None;
    let zoneinfo: &str = &ZONEINFO;
    if !zoneinfo.is_empty() {
        match load_tzinfo_from_dir_or_zip(zoneinfo, name) {
            Ok(data) => match load_location_from_tzdata(name, &data) {
                Ok(z) => return Ok(Arc::new(z)),
                Err(e) => first_err = Some(e),
            },
            Err(TzError::Other(e)) => first_err = Some(e),
            Err(TzError::NotFound) => {}
        }
    }
    match load_location_sources(name, PLATFORM_ZONE_SOURCES) {
        Ok(z) => Ok(Arc::new(z)),
        Err(e) => Err(first_err.unwrap_or(e)),
    }
}

/// `$ZONEINFO`, read once per process like Go (`zoneinfoOnce`).
static ZONEINFO: LazyLock<String> = LazyLock::new(|| std::env::var("ZONEINFO").unwrap_or_default());

/// The error of reading tz data: Go's `syscall.ENOENT` (try the next source) or another
/// error (its message).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TzError {
    /// `syscall.ENOENT`.
    NotFound,
    /// Any other error, with Go's message.
    Other(String),
}

/// containsDotDot reports whether s contains "..".
// go-std: time.containsDotDot L1
fn contains_dot_dot(s: &str) -> bool {
    s.contains("..")
}

/// Many systems use /usr/share/zoneinfo, Solaris 2 has
/// /usr/share/lib/zoneinfo, IRIX 6 has /usr/lib/locale/TZ,
/// NixOS has /etc/zoneinfo.
// go-std: time.platformZoneSources L1
const PLATFORM_ZONE_SOURCES: &[&str] = &[
    "/usr/share/zoneinfo/",
    "/usr/share/lib/zoneinfo/",
    "/usr/lib/locale/TZ/",
    "/etc/zoneinfo",
];

/// loadTzinfoFromDirOrZip returns the contents of the file with the given name
/// in dir. dir can either be an uncompressed zip file, or a directory.
// go-std: time.loadTzinfoFromDirOrZip L1
pub fn load_tzinfo_from_dir_or_zip(dir: &str, name: &str) -> Result<Vec<u8>, TzError> {
    if dir.len() > 4 && dir.ends_with(".zip") {
        return load_tzinfo_from_zip(dir, name);
    }
    let path = if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    };
    read_file(&path)
}

/// `os.ReadFile`-like read of a tz file (Go `readFile`; the 10 MB limit is not checked).
// go-std: time.readFile L2 (no maxFileSize check)
fn read_file(path: &str) -> Result<Vec<u8>, TzError> {
    std::fs::read(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            TzError::NotFound
        } else {
            TzError::Other(go_io_err(&e))
        }
    })
}

/// get4 returns the little-endian 32-bit value at the start of b.
// go-std: time.get4 L1
fn get4(b: &[u8]) -> usize {
    if b.len() < 4 {
        return 0;
    }
    usize::from(b[0]) | usize::from(b[1]) << 8 | usize::from(b[2]) << 16 | usize::from(b[3]) << 24
}

/// get2 returns the little-endian 16-bit value at the start of b.
// go-std: time.get2 L1
fn get2(b: &[u8]) -> usize {
    if b.len() < 2 {
        return 0;
    }
    usize::from(b[0]) | usize::from(b[1]) << 8
}

/// loadTzinfoFromZip returns the contents of the file with the given name
/// in the given uncompressed zip file.
// go-std: time.loadTzinfoFromZip L2 (the file is read whole instead of with pread; the same bytes are examined)
fn load_tzinfo_from_zip(zipfile: &str, name: &str) -> Result<Vec<u8>, TzError> {
    let data = read_file(zipfile)?;
    let corrupt = || TzError::Other(format!("corrupt zip file {zipfile}"));
    // preadn reads exactly len(buf) bytes at off; a negative off is from the end.
    let preadn = |n: usize, off: isize| -> Option<&[u8]> {
        let start = if off < 0 {
            data.len().checked_sub(off.unsigned_abs())?
        } else {
            off as usize
        };
        data.get(start..start.checked_add(n)?)
    };

    const ZECHEADER: usize = 0x06054b50;
    const ZCHEADER: usize = 0x02014b50;
    const ZTAILSIZE: usize = 22;

    const ZHEADERSIZE: usize = 30;
    const ZHEADER: usize = 0x04034b50;

    let buf = preadn(ZTAILSIZE, -(ZTAILSIZE as isize)).ok_or_else(corrupt)?;
    if get4(buf) != ZECHEADER {
        return Err(corrupt());
    }
    let n = get2(&buf[10..]);
    let size = get4(&buf[12..]);
    let off = get4(&buf[16..]);

    let mut buf = preadn(size, off as isize).ok_or_else(corrupt)?;

    for _ in 0..n {
        // zip entry layout:
        //	0	magic[4]
        //	4	madevers[1]
        //	5	madeos[1]
        //	6	extvers[1]
        //	7	extos[1]
        //	8	flags[2]
        //	10	meth[2]
        //	12	modtime[2]
        //	14	moddate[2]
        //	16	crc[4]
        //	20	csize[4]
        //	24	uncsize[4]
        //	28	namelen[2]
        //	30	xlen[2]
        //	32	fclen[2]
        //	34	disknum[2]
        //	36	iattr[2]
        //	38	eattr[4]
        //	42	off[4]
        //	46	name[namelen]
        //	46+namelen+xlen+fclen - next header
        //
        if get4(buf) != ZCHEADER {
            break;
        }
        let meth = get2(&buf[10..]);
        let size = get4(&buf[24..]);
        let namelen = get2(&buf[28..]);
        let mut xlen = get2(&buf[30..]);
        let fclen = get2(&buf[32..]);
        let off = get4(&buf[42..]);
        let zname = &buf[46..46 + namelen];
        buf = &buf[46 + namelen + xlen + fclen..];
        if zname != name.as_bytes() {
            continue;
        }
        if meth != 0 {
            return Err(TzError::Other(format!(
                "unsupported compression for {name} in {zipfile}"
            )));
        }

        // zip per-file header layout:
        //	0	magic[4]
        //	4	extvers[1]
        //	5	extos[1]
        //	6	flags[2]
        //	8	meth[2]
        //	10	modtime[2]
        //	12	moddate[2]
        //	14	crc[4]
        //	18	csize[4]
        //	22	uncsize[4]
        //	26	namelen[2]
        //	28	xlen[2]
        //	30	name[namelen]
        //	30+namelen+xlen - file data
        //
        let hdr = preadn(ZHEADERSIZE + namelen, off as isize).ok_or_else(corrupt)?;
        if get4(hdr) != ZHEADER
            || get2(&hdr[8..]) != meth
            || get2(&hdr[26..]) != namelen
            || &hdr[30..30 + namelen] != name.as_bytes()
        {
            return Err(corrupt());
        }
        xlen = get2(&hdr[28..]);

        let body = preadn(size, (off + 30 + namelen + xlen) as isize).ok_or_else(corrupt)?;
        return Ok(body.to_vec());
    }

    Err(TzError::NotFound)
}

// glue: Go's syscall error text for a failed tz file read (rare: permission errors)
fn go_io_err(e: &std::io::Error) -> String {
    e.to_string()
}

/// loadLocation returns the Location with the given name from one of
/// the specified sources.
// go-std: time.loadLocation L2 (no embedded-tzdata or $GOROOT fallback)
fn load_location_sources(name: &str, sources: &[&str]) -> Result<Location, String> {
    let mut first_err: Option<String> = None;
    for source in sources {
        match load_tzinfo_from_dir_or_zip(source, name) {
            Ok(data) => match load_location_from_tzdata(name, &data) {
                Ok(z) => return Ok(z),
                Err(e) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
            },
            Err(TzError::Other(e)) => {
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
            Err(TzError::NotFound) => {}
        }
    }
    if let Some(e) = first_err {
        return Err(e);
    }
    Err(format!("unknown time zone {name}"))
}

/// dataIO: a cursor over the TZif bytes.
struct DataIo<'a> {
    p: &'a [u8],
    error: bool,
}

impl<'a> DataIo<'a> {
    // go-std: time.dataIO.read L1
    fn read(&mut self, n: usize) -> &'a [u8] {
        if self.p.len() < n {
            self.p = &[];
            self.error = true;
            return &[];
        }
        let (p, rest) = self.p.split_at(n);
        self.p = rest;
        p
    }

    // go-std: time.dataIO.big4 L1
    fn big4(&mut self) -> Option<u32> {
        let p = self.read(4);
        if p.len() < 4 {
            self.error = true;
            return None;
        }
        Some(u32::from_be_bytes([p[0], p[1], p[2], p[3]]))
    }

    // go-std: time.dataIO.big8 L1
    fn big8(&mut self) -> Option<u64> {
        let n1 = self.big4();
        let n2 = self.big4();
        match (n1, n2) {
            (Some(a), Some(b)) => Some((u64::from(a) << 32) | u64::from(b)),
            _ => {
                self.error = true;
                None
            }
        }
    }

    // go-std: time.dataIO.byte L1
    fn byte(&mut self) -> Option<u8> {
        let p = self.read(1);
        if p.is_empty() {
            self.error = true;
            return None;
        }
        Some(p[0])
    }

    // go-std: time.dataIO.rest L1
    fn rest(&mut self) -> &'a [u8] {
        let r = self.p;
        self.p = &[];
        r
    }
}

/// Make a string by stopping at the first NUL
// go-std: time.byteString L1
fn byte_string(p: &[u8]) -> String {
    let p = match p.iter().position(|&b| b == 0) {
        Some(i) => &p[..i],
        None => p,
    };
    String::from_utf8_lossy(p).into_owned()
}

/// LoadLocationFromTZData returns a Location with the given name
/// initialized from the IANA Time Zone database-formatted data.
/// The data should be in the format of a standard IANA time zone file
/// (for example, the content of /etc/localtime on Unix systems).
// go-std: time.LoadLocationFromTZData L1
pub fn load_location_from_tzdata(name: &str, data: &[u8]) -> Result<Location, String> {
    let bad = || ERR_BAD_DATA.to_owned();
    let mut d = DataIo {
        p: data,
        error: false,
    };

    // 4-byte magic "TZif"
    if d.read(4) != b"TZif" {
        return Err(bad());
    }

    // 1-byte version, then 15 bytes of padding
    let version;
    let p = d.read(16);
    if p.len() != 16 {
        return Err(bad());
    }
    match p[0] {
        0 => version = 1,
        b'2' => version = 2,
        b'3' => version = 3,
        _ => return Err(bad()),
    }

    // six big-endian 32-bit integers:
    //    number of UTC/local indicators
    //    number of standard/wall indicators
    //    number of leap seconds
    //    number of transition times
    //    number of local time zones
    //    number of characters of time zone abbrev strings
    const NUTC_LOCAL: usize = 0;
    const NSTD_WALL: usize = 1;
    const NLEAP: usize = 2;
    const NTIME: usize = 3;
    const NZONE: usize = 4;
    const NCHAR: usize = 5;
    let mut n = [0usize; 6];
    for v in &mut n {
        let nn = d.big4().ok_or_else(bad)?;
        *v = nn as usize;
    }

    // If we have version 2 or 3, then the data is first written out
    // in a 32-bit format, then written out again in a 64-bit format.
    // Skip the 32-bit format and read the 64-bit one, as it can
    // describe a broader range of dates.

    let mut is64 = false;
    if version > 1 {
        // Skip the 32-bit data.
        let mut skip = n[NTIME] * 4
            + n[NTIME]
            + n[NZONE] * 6
            + n[NCHAR]
            + n[NLEAP] * 8
            + n[NSTD_WALL]
            + n[NUTC_LOCAL];
        // Skip the version 2 header that we just read.
        skip += 4 + 16;
        d.read(skip);

        is64 = true;

        // Read the counts again, they can differ.
        for v in &mut n {
            let nn = d.big4().ok_or_else(bad)?;
            *v = nn as usize;
        }
    }

    let size = if is64 { 8 } else { 4 };

    // Transition times.
    let mut txtimes = DataIo {
        p: d.read(n[NTIME] * size),
        error: false,
    };

    // Time zone indices for transition times.
    let txzones = d.read(n[NTIME]);

    // Zone info structures
    let mut zonedata = DataIo {
        p: d.read(n[NZONE] * 6),
        error: false,
    };

    // Time zone abbreviations.
    let abbrev = d.read(n[NCHAR]);

    // Leap-second time pairs
    d.read(n[NLEAP] * (size + 4));

    // Whether tx times associated with local time types
    // are specified as standard time or wall time.
    let _isstd = d.read(n[NSTD_WALL]);

    // Whether tx times associated with local time types
    // are specified as UTC or local time.
    let _isutc = d.read(n[NUTC_LOCAL]);

    if d.error {
        // ran out of data
        return Err(bad());
    }

    let mut extend = String::new();
    let rest = d.rest();
    if rest.len() > 2 && rest[0] == b'\n' && rest[rest.len() - 1] == b'\n' {
        extend = String::from_utf8_lossy(&rest[1..rest.len() - 1]).into_owned();
    }

    // Now we can build up a useful data structure.
    // First the zone information.
    //    utcoff[4] isdst[1] nameindex[1]
    let nzone = n[NZONE];
    if nzone == 0 {
        // Reject tzdata files with no zones. There's nothing useful in them.
        // This also avoids a panic later when we add and then use a fake transition (golang.org/issue/29437).
        return Err(bad());
    }
    let mut zones = Vec::with_capacity(nzone);
    for _ in 0..nzone {
        let n = zonedata.big4().ok_or_else(bad)?;
        let offset = n as i32;
        let b = zonedata.byte().ok_or_else(bad)?;
        let is_dst = b != 0;
        let b = zonedata.byte().ok_or_else(bad)?;
        if usize::from(b) >= abbrev.len() {
            return Err(bad());
        }
        zones.push(Zone {
            name: byte_string(&abbrev[usize::from(b)..]),
            offset,
            is_dst,
        });
    }

    // Now the transition time info.
    let mut tx = Vec::with_capacity(n[NTIME]);
    for &zi in txzones.iter().take(n[NTIME]) {
        let when = if !is64 {
            i64::from(txtimes.big4().ok_or_else(bad)? as i32)
        } else {
            txtimes.big8().ok_or_else(bad)? as i64
        };
        if usize::from(zi) >= zones.len() {
            return Err(bad());
        }
        tx.push(ZoneTrans { when, index: zi });
    }

    if tx.is_empty() {
        // Build fake transition to cover all time.
        // This happens in fixed locations like "Etc/GMT0".
        tx.push(ZoneTrans {
            when: ALPHA,
            index: 0,
        });
    }

    let mut l = Location {
        name: name.to_owned(),
        zone: zones,
        tx,
        extend,
        cache_start: 0,
        cache_end: 0,
        cache_zone: None,
    };

    // Fill in the cache with information about right now,
    // since that will be the most common lookup.
    let sec = runtime_now();
    for i in 0..l.tx.len() {
        if l.tx[i].when <= sec && (i + 1 == l.tx.len() || sec < l.tx[i + 1].when) {
            l.cache_start = l.tx[i].when;
            l.cache_end = OMEGA;
            l.cache_zone = Some(l.zone[usize::from(l.tx[i].index)].clone());
            if i + 1 < l.tx.len() {
                l.cache_end = l.tx[i + 1].when;
            } else if !l.extend.is_empty()
                && let Some(z) = tzset(&l.extend, l.cache_start, sec)
            {
                let zone = Zone {
                    name: z.name.to_owned(),
                    offset: z.offset,
                    is_dst: z.is_dst,
                };
                let (start, end) = (z.start, z.end);
                l.cache_start = start;
                l.cache_end = end;
                // Go points cacheZone at the matching entry of l.zone (findZone) or a new
                // zone; both carry the same name, offset and isDST.
                l.cache_zone = Some(zone);
            }
            break;
        }
    }

    Ok(l)
}

/// The current Unix second (Go `runtimeNow`, the wall-clock part).
// go-std: time.runtimeNow L2 (seconds only)
fn runtime_now() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64) - 1,
    }
}

/// initLocal: `time.Local` from `$TZ` or `/etc/localtime`.
// go-std: time.initLocal L1 (zoneinfo_unix.go)
fn init_local() -> Location {
    // consult $TZ to find the time zone to use.
    // no $TZ means use the system default /etc/localtime.
    // $TZ="" means use UTC.
    // $TZ="foo" or $TZ=":foo" if foo is an absolute path, then the file pointed
    // by foo will be used to initialize timezone; otherwise, file
    // /usr/share/zoneinfo/foo will be used.
    let named = |mut z: Location, name: &str| {
        z.name = name.to_owned();
        z
    };
    match std::env::var("TZ") {
        Err(_) => {
            if let Ok(z) = load_location_sources("localtime", &["/etc"]) {
                return named(z, "Local");
            }
        }
        Ok(tz) if !tz.is_empty() => {
            let tz = tz.strip_prefix(':').unwrap_or(&tz).to_owned();
            if tz.starts_with('/') {
                if let Ok(z) = load_location_sources(&tz, &[""]) {
                    let name = if tz == "/etc/localtime" { "Local" } else { &tz };
                    return named(z, name);
                }
            } else if !tz.is_empty()
                && tz != "UTC"
                && let Ok(z) = load_location_sources(&tz, PLATFORM_ZONE_SOURCES)
            {
                return z;
            }
        }
        Ok(_) => {}
    }
    // Fall back to UTC.
    Location {
        name: "UTC".to_owned(),
        zone: Vec::new(),
        tx: Vec::new(),
        extend: String::new(),
        cache_start: 0,
        cache_end: 0,
        cache_zone: None,
    }
}
