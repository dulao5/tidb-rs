//! go-file: $GOROOT/src/time/time.go
//! go-file: $GOROOT/src/time/format.go
//!
//! Go's `time` package (Go 1.25), the parts TiDB's `pkg/types` time code uses: `Location`
//! (zoneinfo.rs), `Time` and its calendar arithmetic (instant.rs), `Duration` and its
//! `String`, and `time.ParseDuration`.

use crate::gofmt::{self, Arg};

mod instant;
mod zoneinfo;

pub use instant::{Month, Time, Weekday, date, now, unix, weekday_string};
pub use zoneinfo::{
    Location, TzError, ZoneInfo, fixed_zone, is_local, is_utc, load_location,
    load_location_from_tzdata, load_tzinfo_from_dir_or_zip, local, utc,
};

/// `time.Duration` (nanoseconds).
pub type Duration = i64;

/// `time.Nanosecond`.
pub const NANOSECOND: Duration = 1;
/// `time.Microsecond`.
pub const MICROSECOND: Duration = 1000 * NANOSECOND;
/// `time.Millisecond`.
pub const MILLISECOND: Duration = 1000 * MICROSECOND;
/// `time.Second`.
pub const SECOND: Duration = 1000 * MILLISECOND;
/// `time.Minute`.
pub const MINUTE: Duration = 60 * SECOND;
/// `time.Hour`.
pub const HOUR: Duration = 60 * MINUTE;

const MIN_DURATION: Duration = i64::MIN;
const MAX_DURATION: Duration = i64::MAX;

const SECONDS_PER_MINUTE: i64 = 60;
const SECONDS_PER_HOUR: i64 = 60 * SECONDS_PER_MINUTE;
const SECONDS_PER_DAY: i64 = 24 * SECONDS_PER_HOUR;

/// Days from March 1 through end of year
const MARCH_THRU_DECEMBER: u32 = 31 + 30 + 31 + 30 + 31 + 31 + 30 + 31 + 30 + 31;

/// absoluteYears is the number of years we subtract from internal time to get absolute time.
/// This value must be 0 mod 400, and it defines the “absolute zero instant”
/// mentioned in the “Computations on Times” comment above: March 1, -absoluteYears.
/// Dates before the absolute epoch will not compute correctly,
/// but otherwise the value can be changed as needed.
const ABSOLUTE_YEARS: u64 = 292277022400;

/// The year of the zero Time.
/// Assumed by the unixToInternal computation below.
const ABSOLUTE_TO_INTERNAL: i64 =
    -((ABSOLUTE_YEARS as i64 * 3652425 / 10000 + MARCH_THRU_DECEMBER as i64) * SECONDS_PER_DAY);
const INTERNAL_TO_ABSOLUTE: i64 = -ABSOLUTE_TO_INTERNAL;

/// The year of the zero Unix time.
const UNIX_TO_INTERNAL: i64 = (1969 * 365 + 1969 / 4 - 1969 / 100 + 1969 / 400) * SECONDS_PER_DAY;
const INTERNAL_TO_UNIX: i64 = -UNIX_TO_INTERNAL;

const ABSOLUTE_TO_UNIX: i64 = ABSOLUTE_TO_INTERNAL + INTERNAL_TO_UNIX;
const UNIX_TO_ABSOLUTE: i64 = UNIX_TO_INTERNAL + INTERNAL_TO_ABSOLUTE;

/// `absDays`: days since the absolute epoch (March 1, -292277022400).
#[derive(Clone, Copy, Debug)]
struct AbsDays(u64);

/// absSeconds.days: the days of an absolute time in seconds.
// go-std: time.absSeconds.days L1
fn abs_seconds_days(abs: u64) -> u64 {
    abs / SECONDS_PER_DAY as u64
}

// glue: constructor of the absDays newtype
fn abs_days(d: u64) -> AbsDays {
    AbsDays(d)
}

/// dateToAbsDays takes a standard year/month/day and returns the
/// number of days from the absolute epoch to that day.
/// The days argument can be out of range and in particular can be negative.
// go-std: time.dateToAbsDays L1
fn date_to_abs_days(year: i64, month: i64, day: i64) -> AbsDays {
    // See “Computations on Times” comment above.
    let mut amonth = month as u32;
    let mut jan_feb: u32 = 0;
    if amonth < 3 {
        jan_feb = 1;
    }
    amonth = amonth.wrapping_add(12 * jan_feb);
    let y = (year as u64)
        .wrapping_sub(u64::from(jan_feb))
        .wrapping_add(ABSOLUTE_YEARS);

    // For amonth is in the range [3,14], we want:
    //
    //    ayday := (153*amonth - 457) / 5
    //
    // (See the “Computations on Times” comment above
    // as well as Neri and Schneider, section 7.)
    //
    // That is equivalent to:
    //
    //    ayday := (979*amonth - 2919) >> 5
    //
    // and the latter form uses a couple fewer instructions,
    // so use it, saving a few cycles.
    // See Neri and Schneider, section 8.3
    // for more about this optimization.
    //
    // (Note that there is no saved division, because the compiler
    // implements / 5 without division in all cases.)
    let ayday = (979u32.wrapping_mul(amonth).wrapping_sub(2919)) >> 5;

    let century = y / 100;
    let cyear = (y % 100) as u32;
    let cday = 1461 * cyear / 4;
    let centurydays = 146097u64.wrapping_mul(century) / 4;

    AbsDays(
        centurydays
            .wrapping_add((i64::from(cday + ayday).wrapping_add(day).wrapping_sub(1)) as u64),
    )
}

impl AbsDays {
    /// split splits days into century, cyear, ayday.
    // go-std: time.absDays.split L1
    fn split(self) -> (u64, u32, u32) {
        // See “Computations on Times” comment above.
        let d = 4u64.wrapping_mul(self.0).wrapping_add(3);
        let century = d / 146097;

        // This should be
        //    cday := uint32(d % 146097) / 4
        //    cd := 4*cday + 3
        // which is to say
        //    cday := uint32(d % 146097) >> 2
        //    cd := cday<<2 + 3
        // but of course (x>>2<<2)+3 == x|3,
        // so do that instead.
        let cd = (d % 146097) as u32 | 3;

        // For cd in the range [0, 146097] (100 years), we want:
        //
        //    cyear := (4 cdays + 3) / 1461
        //    yday := (4 cdays + 3) % 1461 / 4
        //
        // (See the “Computations on Times” comment above
        // as well as Neri and Schneider, section 7.)
        //
        // That is equivalent to:
        //
        //    cyear := (2939745 cdays) >> 32
        //    yday := (2939745 cdays) & 0xFFFFFFFF / 2939745 / 4
        //
        // so do that instead, saving a few cycles.
        // See Neri and Schneider, section 8.3
        // for more about this optimization.
        let prod = 2939745u64 * u64::from(cd);
        let hi = (prod >> 32) as u32;
        let lo = prod as u32;
        let cyear = hi;
        let ayday = lo / 2939745 / 4;
        (century, cyear, ayday)
    }

    /// date converts days into standard year, month, day.
    // go-std: time.absDays.date L1
    fn date(self) -> (i64, i64, i64) {
        let (century, cyear, ayday) = self.split();
        let (amonth, day) = ayday_split(ayday);
        let jan_feb = ayday_jan_feb(ayday);
        let year = century_year(century, cyear, jan_feb);
        let month = i64::from(amonth) - i64::from(jan_feb) * 12;
        (year, month, day)
    }

    /// yearYday converts days into the standard year and 1-based yday.
    // go-std: time.absDays.yearYday L1
    fn year_yday(self) -> (i64, i64) {
        let (century, cyear, ayday) = self.split();
        let jan_feb = ayday_jan_feb(ayday);
        let year = century_year(century, cyear, jan_feb);
        let yday = ayday_yday(ayday, jan_feb, century_leap(century, cyear));
        (year, yday)
    }

    /// weekday returns the day of the week specified by days.
    // go-std: time.absDays.weekday L1
    fn weekday(self) -> i64 {
        // March 1 of the absolute year, like March 1 of 2000, was a Wednesday.
        (self.0.wrapping_add(3) % 7) as i64
    }
}

/// split splits ayday into absolute month and standard (1-based) day-in-month.
// go-std: time.absYday.split L1
fn ayday_split(ayday: u32) -> (u32, i64) {
    // See “Computations on Times” comment above.
    //
    // For yday in the range [0,366],
    //
    //    amonth := (5 yday + 461) / 153
    //    mday := (5 yday + 461) % 153 / 5
    //
    // is equivalent to:
    //
    //    amonth = (2141 yday + 197913) >> 16
    //    mday = (2141 yday + 197913) & 0xFFFF / 2141
    //
    // so do that instead, saving a few cycles.
    // See Neri and Schneider, section 8.4.
    let d = 2141 * ayday + 197913;
    (d >> 16, 1 + i64::from((d & 0xFFFF) / 2141))
}

/// janFeb returns 1 if the March 1-based ayday is in January or February, 0 otherwise.
// go-std: time.absYday.janFeb L1
fn ayday_jan_feb(ayday: u32) -> u32 {
    // See “Computations on Times” comment above.
    u32::from(ayday >= MARCH_THRU_DECEMBER)
}

/// leap returns 1 if (century, cyear) is a leap year, 0 otherwise.
// go-std: time.absCentury.leap L1
fn century_leap(century: u64, cyear: u32) -> i64 {
    // See “Computations on Times” comment above.
    let y4ok = i64::from(cyear.is_multiple_of(4));
    let y100ok = i64::from(cyear != 0);
    let y400ok = i64::from(century.is_multiple_of(4));
    y4ok & (y100ok | y400ok)
}

/// year returns the standard year for (century, cyear, janFeb).
// go-std: time.absCentury.year L1
fn century_year(century: u64, cyear: u32, jan_feb: u32) -> i64 {
    // See “Computations on Times” comment above.
    (century.wrapping_mul(100).wrapping_sub(ABSOLUTE_YEARS) as i64)
        + i64::from(cyear)
        + i64::from(jan_feb)
}

/// yday returns the standard 1-based yday for (ayday, janFeb, leap).
// go-std: time.absYday.yday L1
fn ayday_yday(ayday: u32, jan_feb: u32, leap: i64) -> i64 {
    // See “Computations on Times” comment above.
    i64::from(ayday) + (1 + 31 + 28) + (leap & !i64::from(jan_feb)) - 365 * i64::from(jan_feb)
}

/// clock: hour, min, sec of an absolute time in seconds.
// go-std: time.absSeconds.clock L1
fn abs_clock(abs: u64) -> (i64, i64, i64) {
    let mut sec = (abs % SECONDS_PER_DAY as u64) as i64;
    let hour = sec / SECONDS_PER_HOUR;
    sec -= hour * SECONDS_PER_HOUR;
    let min = sec / SECONDS_PER_MINUTE;
    sec -= min * SECONDS_PER_MINUTE;
    (hour, min, sec)
}

/// isLeap: is year a leap year?
// go-std: time.isLeap L1
fn is_leap(year: i64) -> bool {
    // year%4 == 0 && (year%100 != 0 || year%400 == 0)
    // Bottom 2 bits must be clear.
    // For multiples of 25, bottom 4 bits must be clear.
    // Thanks to Cassio Neri for this trick.
    let mask = if year % 25 != 0 { 3 } else { 0xf };
    year & mask == 0
}

/// daysBefore returns the number of days in a non-leap year before month m.
/// daysBefore(December+1) returns 365.
/// It panics if m is not in the range [January, December+1].
// go-std: time.daysBefore L1
fn days_before(m: i32) -> i32 {
    let adj = if m >= 3 { -2 } else { 0 };
    // With the -2 adjustment after February,
    // we need to compute the running sum of:
    //    0  31  30  31  30  31  30  31  31  30  31  30  31
    // which is:
    //    0  31  61  92 122 153 183 214 245 275 306 336 367
    // This is almost exactly 367/12×(m-1) except for the
    // occasonal off-by-one suggesting there may be an
    // integer approximation of the form (a×m + b)/c.
    // A brute force search over small a, b, c finds that
    // (214×m - 211) / 7 computes the function perfectly.
    (214 * m - 211) / 7 + adj
}

// go-std: time.daysIn L1
fn days_in(m: i32, year: i64) -> i32 {
    if m == 2 {
        if is_leap(year) {
            return 29;
        }
        return 28;
    }
    // With the special case of February eliminated, the pattern is
    //    31 30 31 30 31 30 31 31 30 31 30 31
    // Adding m&1 produces the basic alternation;
    // adding (m>>3)&1 inverts the alternation starting in August.
    30 + ((m + (m >> 3)) & 1)
}

/// String returns a string representing the duration in the form "72h3m0.5s".
/// Leading zero units are omitted. As a special case, durations less than one
/// second format use a smaller unit (milli-, micro-, or nanoseconds) to ensure
/// that the leading digit is non-zero. The zero duration formats as 0s.
// go-std: time.Duration.String L1 (with time.Duration.format, fmtFrac, fmtInt)
pub fn duration_string(d: Duration) -> String {
    let mut buf = [0u8; 32];
    let mut w = buf.len();
    let mut u = d as u64;
    let neg = d < 0;
    if neg {
        u = u.wrapping_neg();
    }

    if u < SECOND as u64 {
        // Special case: if duration is smaller than a second,
        // use smaller units, like 1.2ms
        let prec;
        w -= 1;
        buf[w] = b's';
        w -= 1;
        if u == 0 {
            buf[w] = b'0';
            return String::from_utf8_lossy(&buf[w..]).into_owned();
        } else if u < MICROSECOND as u64 {
            // print nanoseconds
            prec = 0;
            buf[w] = b'n';
        } else if u < MILLISECOND as u64 {
            // print microseconds
            prec = 3;
            // U+00B5 'µ' micro sign == 0xC2 0xB5
            w -= 1; // Need room for two bytes.
            buf[w..w + 2].copy_from_slice("µ".as_bytes());
        } else {
            // print milliseconds
            prec = 6;
            buf[w] = b'm';
        }
        (w, u) = fmt_frac(&mut buf, w, u, prec);
        w = fmt_int(&mut buf, w, u);
    } else {
        w -= 1;
        buf[w] = b's';

        (w, u) = fmt_frac(&mut buf, w, u, 9);

        // u is now integer seconds
        w = fmt_int(&mut buf, w, u % 60);
        u /= 60;

        // u is now integer minutes
        if u > 0 {
            w -= 1;
            buf[w] = b'm';
            w = fmt_int(&mut buf, w, u % 60);
            u /= 60;

            // u is now integer hours
            // Stop at hours because days can be different lengths.
            if u > 0 {
                w -= 1;
                buf[w] = b'h';
                w = fmt_int(&mut buf, w, u);
            }
        }
    }

    if neg {
        w -= 1;
        buf[w] = b'-';
    }

    String::from_utf8_lossy(&buf[w..]).into_owned()
}

/// fmtFrac formats the fraction of v/10**prec (e.g., ".12345") into the
/// tail of buf, omitting trailing zeros. It omits the decimal
/// point too when the fraction is 0. It returns the index where the
/// output bytes begin and the value v/10**prec.
// go-std: time.fmtFrac L1
fn fmt_frac(buf: &mut [u8], mut w: usize, mut v: u64, prec: usize) -> (usize, u64) {
    // Omit trailing zeros up to and including decimal point.
    let mut print = false;
    for _ in 0..prec {
        let digit = v % 10;
        print = print || digit != 0;
        if print {
            w -= 1;
            buf[w] = digit as u8 + b'0';
        }
        v /= 10;
    }
    if print {
        w -= 1;
        buf[w] = b'.';
    }
    (w, v)
}

/// fmtInt formats v into the tail of buf.
/// It returns the index where the output begins.
// go-std: time.fmtInt L1
fn fmt_int(buf: &mut [u8], mut w: usize, mut v: u64) -> usize {
    if v == 0 {
        w -= 1;
        buf[w] = b'0';
    } else {
        while v > 0 {
            w -= 1;
            buf[w] = (v % 10) as u8 + b'0';
            v /= 10;
        }
    }
    w
}

/// lessThanHalf reports whether x+x < y but avoids overflow,
/// assuming x and y are both positive (Duration is signed).
// go-std: time.lessThanHalf L1
fn less_than_half(x: Duration, y: Duration) -> bool {
    (x as u64).wrapping_add(x as u64) < y as u64
}

/// Abs returns the absolute value of d.
/// As a special case, math.MinInt64 is converted to math.MaxInt64.
// go-std: time.Duration.Abs L1
pub fn duration_abs(d: Duration) -> Duration {
    if d >= 0 {
        d
    } else if d == MIN_DURATION {
        MAX_DURATION
    } else {
        -d
    }
}

// go-std: time.quote L2 (strconv-style quoting via fmt %q)
fn quote(s: &str) -> String {
    String::from_utf8_lossy(&gofmt::sprintf("%q", &[Arg::from(s.to_owned())])).into_owned()
}

// go-std: time.leadingInt L1
fn leading_int(s: &[u8]) -> Result<(u64, &[u8]), ()> {
    let mut x: u64 = 0;
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if !c.is_ascii_digit() {
            break;
        }
        if x > (1 << 63) / 10 {
            return Err(());
        }
        x = x * 10 + u64::from(c - b'0');
        if x > 1 << 63 {
            return Err(());
        }
        i += 1;
    }
    Ok((x, &s[i..]))
}

// go-std: time.leadingFraction L1
fn leading_fraction(s: &[u8]) -> (u64, f64, &[u8]) {
    let mut i = 0;
    let mut x: u64 = 0;
    let mut scale = 1f64;
    let mut overflow = false;
    while i < s.len() {
        let c = s[i];
        if !c.is_ascii_digit() {
            break;
        }
        i += 1;
        if overflow {
            continue;
        }
        if x > ((1u64 << 63) - 1) / 10 {
            overflow = true;
            continue;
        }
        let y = x * 10 + u64::from(c - b'0');
        if y > 1 << 63 {
            overflow = true;
            continue;
        }
        x = y;
        scale *= 10.0;
    }
    (x, scale, &s[i..])
}

/// `time.ParseDuration` parses a duration string; the result is nanoseconds. The error is
/// Go's message.
// go-std: time.ParseDuration L1
pub fn parse_duration(orig: &str) -> Result<i64, String> {
    let invalid = || format!("time: invalid duration {}", quote(orig));
    let mut s = orig.as_bytes();
    let mut d: u64 = 0;
    let mut neg = false;
    if let Some(&c) = s.first()
        && (c == b'-' || c == b'+')
    {
        neg = c == b'-';
        s = &s[1..];
    }
    if s == b"0" {
        return Ok(0);
    }
    if s.is_empty() {
        return Err(invalid());
    }
    while !s.is_empty() {
        let (mut f, mut scale) = (0u64, 1f64);
        if !(s[0] == b'.' || s[0].is_ascii_digit()) {
            return Err(invalid());
        }
        let pl = s.len();
        let (mut v, rest) = leading_int(s).map_err(|()| invalid())?;
        s = rest;
        let pre = pl != s.len();
        let mut post = false;
        if !s.is_empty() && s[0] == b'.' {
            s = &s[1..];
            let pl = s.len();
            let (ff, sc, rest) = leading_fraction(s);
            f = ff;
            scale = sc;
            s = rest;
            post = pl != s.len();
        }
        if !pre && !post {
            return Err(invalid());
        }
        let i = s
            .iter()
            .position(|&c| c == b'.' || c.is_ascii_digit())
            .unwrap_or(s.len());
        if i == 0 {
            return Err(format!("time: missing unit in duration {}", quote(orig)));
        }
        let u = &s[..i];
        s = &s[i..];
        let unit: u64 = match u {
            b"ns" => 1,
            b"us" | b"\xc2\xb5s" | b"\xce\xbcs" => 1_000, // us, µs (U+00B5), μs (U+03BC)
            b"ms" => 1_000_000,
            b"s" => 1_000_000_000,
            b"m" => 60_000_000_000,
            b"h" => 3_600_000_000_000,
            _ => {
                let q =
                    String::from_utf8_lossy(&gofmt::sprintf("%q", &[Arg::go_string(u.to_vec())]))
                        .into_owned();
                return Err(format!(
                    "time: unknown unit {q} in duration {}",
                    quote(orig)
                ));
            }
        };
        if v > (1 << 63) / unit {
            return Err(invalid());
        }
        v *= unit;
        if f > 0 {
            v += (f as f64 * (unit as f64 / scale)) as u64;
            if v > 1 << 63 {
                return Err(invalid());
            }
        }
        d += v;
        if d > 1 << 63 {
            return Err(invalid());
        }
    }
    if neg {
        return Ok((d as i64).wrapping_neg());
    }
    if d > (1 << 63) - 1 {
        return Err(invalid());
    }
    Ok(d as i64)
}
