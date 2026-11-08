//! go-file: pkg/types/time.go
//!
//! MySQL DATETIME / TIMESTAMP / DATE (`Time`) and TIME (`Duration`). Go strings are byte
//! strings, so every string argument is `&[u8]` and every result string `Vec<u8>` (M-ERR).

#![allow(non_upper_case_globals, clippy::needless_late_init)]

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use tidb_base::gofmt::{Arg, sprintf};
use tidb_base::gostd::time::{self as gotime, Location};
use tidb_base::{Error, mysql, terror};

use crate::context::Context;
use crate::core_time::{
    CoreTime, ZeroCoreTime, calc_time_duration_diff, calc_time_from_sec, calc_time_time_diff,
    compare_time, get_date_from_daynr, mix_date_and_duration, secondsIn24Hour, timestamp_diff,
};
use crate::errors::{
    DateStr, DateTimeStr, ErrDatetimeFunctionOverflow, ErrIncorrectDatetimeValue,
    ErrInvalidYearFormat, ErrOverflow, ErrTimestampInDSTTransition, ErrTruncated,
    ErrTruncatedWrongVal, ErrWarnDataOutOfRange, ErrWrongValue, TimeStr, TimestampStr,
};
use crate::fsp::{DefaultFsp, MaxFsp, MinFsp, UnspecifiedFsp, align_frac, check_fsp, parse_frac};
use crate::gostd::{atoi, decode_rune, parse_int};
use crate::helper::{is_digit, is_punctuation};
use crate::mydecimal::{MyDecimal, decimal_mul, decimal_sub};
use crate::util_parser as parser;

/// Time format without fractional seconds precision.
pub const DateFormat: &str = "2006-01-02";
/// Time format without fractional seconds precision.
pub const TimeFormat: &str = "2006-01-02 15:04:05";
/// TimeFSPFormat is time format with fractional seconds precision.
pub const TimeFSPFormat: &str = "2006-01-02 15:04:05.000000";
/// UTCTimeFormat is used to parse and format gotime.
pub const UTCTimeFormat: &str = "2006-01-02 15:04:05 UTC";

/// MinYear is the minimum for mysql year type.
pub const MinYear: i16 = 1901;
/// MaxYear is the maximum for mysql year type.
pub const MaxYear: i16 = 2155;
/// MaxDuration is the maximum for duration.
pub const MaxDuration: i64 = 838 * 10000 + 59 * 100 + 59;
/// MinTime is the minimum for mysql time type.
pub const MinTime: gotime::Duration =
    -(838 * gotime::HOUR + 59 * gotime::MINUTE + 59 * gotime::SECOND);
/// MaxTime is the maximum for mysql time type.
pub const MaxTime: gotime::Duration =
    838 * gotime::HOUR + 59 * gotime::MINUTE + 59 * gotime::SECOND;
/// ZeroDatetimeStr is the string representation of a zero datetime.
pub const ZeroDatetimeStr: &str = "0000-00-00 00:00:00";
/// ZeroDateStr is the string representation of a zero date.
pub const ZeroDateStr: &str = "0000-00-00";

/// TimeMaxHour is the max hour for mysql time type.
pub const TimeMaxHour: isize = 838;
/// TimeMaxMinute is the max minute for mysql time type.
pub const TimeMaxMinute: isize = 59;
/// TimeMaxSecond is the max second for mysql time type.
pub const TimeMaxSecond: isize = 59;
/// TimeMaxValue is the maximum value for mysql time type.
pub const TimeMaxValue: isize = TimeMaxHour * 10000 + TimeMaxMinute * 100 + TimeMaxSecond;
/// TimeMaxValueSeconds is the maximum second value for mysql time type.
pub const TimeMaxValueSeconds: isize = TimeMaxHour * 3600 + TimeMaxMinute * 60 + TimeMaxSecond;

/// YearIndex is index of 'YEARS-MONTHS DAYS HOURS:MINUTES:SECONDS.MICROSECONDS' expr Format
pub const YearIndex: usize = 0;
/// MonthIndex is index of 'YEARS-MONTHS DAYS HOURS:MINUTES:SECONDS.MICROSECONDS' expr Format
pub const MonthIndex: usize = 1;
/// DayIndex is index of 'YEARS-MONTHS DAYS HOURS:MINUTES:SECONDS.MICROSECONDS' expr Format
pub const DayIndex: usize = 2;
/// HourIndex is index of 'YEARS-MONTHS DAYS HOURS:MINUTES:SECONDS.MICROSECONDS' expr Format
pub const HourIndex: usize = 3;
/// MinuteIndex is index of 'YEARS-MONTHS DAYS HOURS:MINUTES:SECONDS.MICROSECONDS' expr Format
pub const MinuteIndex: usize = 4;
/// SecondIndex is index of 'YEARS-MONTHS DAYS HOURS:MINUTES:SECONDS.MICROSECONDS' expr Format
pub const SecondIndex: usize = 5;
/// MicrosecondIndex is index of 'YEARS-MONTHS DAYS HOURS:MINUTES:SECONDS.MICROSECONDS' expr Format
pub const MicrosecondIndex: usize = 6;

/// YearMonthMaxCnt is max parameters count 'YEARS-MONTHS' expr Format allowed
pub const YearMonthMaxCnt: usize = 2;
/// DayHourMaxCnt is max parameters count 'DAYS HOURS' expr Format allowed
pub const DayHourMaxCnt: usize = 2;
/// DayMinuteMaxCnt is max parameters count 'DAYS HOURS:MINUTES' expr Format allowed
pub const DayMinuteMaxCnt: usize = 3;
/// DaySecondMaxCnt is max parameters count 'DAYS HOURS:MINUTES:SECONDS' expr Format allowed
pub const DaySecondMaxCnt: usize = 4;
/// DayMicrosecondMaxCnt is max parameters count 'DAYS HOURS:MINUTES:SECONDS.MICROSECONDS' expr Format allowed
pub const DayMicrosecondMaxCnt: usize = 5;
/// HourMinuteMaxCnt is max parameters count 'HOURS:MINUTES' expr Format allowed
pub const HourMinuteMaxCnt: usize = 2;
/// HourSecondMaxCnt is max parameters count 'HOURS:MINUTES:SECONDS' expr Format allowed
pub const HourSecondMaxCnt: usize = 3;
/// HourMicrosecondMaxCnt is max parameters count 'HOURS:MINUTES:SECONDS.MICROSECONDS' expr Format allowed
pub const HourMicrosecondMaxCnt: usize = 4;
/// MinuteSecondMaxCnt is max parameters count 'MINUTES:SECONDS' expr Format allowed
pub const MinuteSecondMaxCnt: usize = 2;
/// MinuteMicrosecondMaxCnt is max parameters count 'MINUTES:SECONDS.MICROSECONDS' expr Format allowed
pub const MinuteMicrosecondMaxCnt: usize = 3;
/// SecondMicrosecondMaxCnt is max parameters count 'SECONDS.MICROSECONDS' expr Format allowed
pub const SecondMicrosecondMaxCnt: usize = 2;
/// TimeValueCnt is parameters count 'YEARS-MONTHS DAYS HOURS:MINUTES:SECONDS.MICROSECONDS' expr Format
pub const TimeValueCnt: usize = 7;

/// ZeroDuration is the zero value for Duration type.
pub const ZeroDuration: Duration = Duration {
    duration: 0,
    fsp: DefaultFsp,
};

/// ZeroCoreTime is the zero value for Time type.
pub const ZeroTime: Time = Time { core: CoreTime(0) };

/// ZeroDatetime is the zero value for datetime Time.
pub static ZeroDatetime: LazyLock<Time> =
    LazyLock::new(|| new_time(ZeroCoreTime, mysql::TypeDatetime, DefaultFsp));

/// ZeroTimestamp is the zero value for timestamp Time.
pub static ZeroTimestamp: LazyLock<Time> =
    LazyLock::new(|| new_time(ZeroCoreTime, mysql::TypeTimestamp, DefaultFsp));

/// ZeroDate is the zero value for date Time.
pub static ZeroDate: LazyLock<Time> =
    LazyLock::new(|| new_time(ZeroCoreTime, mysql::TypeDate, DefaultFsp));

/// MinDatetime is the minimum for Golang Time type.
pub const MinDatetime: CoreTime = from_date(1, 1, 1, 0, 0, 0, 0);
/// MaxDatetime is the maximum for mysql datetime type.
pub const MaxDatetime: CoreTime = from_date(9999, 12, 31, 23, 59, 59, 999999);

/// BoundTimezone is the timezone for min and max timestamp.
// glue: the package variable BoundTimezone (= time.UTC)
pub fn bound_timezone() -> Arc<Location> {
    gotime::utc()
}

/// MinTimestamp is the minimum for mysql timestamp type.
pub static MinTimestamp: LazyLock<Time> = LazyLock::new(|| {
    new_time(
        from_date(1970, 1, 1, 0, 0, 1, 0),
        mysql::TypeTimestamp,
        DefaultFsp,
    )
});
/// MaxTimestamp is the maximum for mysql timestamp type.
pub static MaxTimestamp: LazyLock<Time> = LazyLock::new(|| {
    new_time(
        from_date(2038, 1, 19, 3, 14, 7, 999999),
        mysql::TypeTimestamp,
        DefaultFsp,
    )
});

/// WeekdayNames lists names of weekdays, which are used in builtin time function `dayname`.
pub const WeekdayNames: [&str; 7] = [
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
];

/// MonthNames lists names of months, which are used in builtin time function `monthname`.
pub const MonthNames: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// GoDurationDay is the gotime.Duration which equals to a Day.
pub const GoDurationDay: gotime::Duration = gotime::HOUR * 24;
/// GoDurationWeek is the gotime.Duration which equals to a Week.
pub const GoDurationWeek: gotime::Duration = GoDurationDay * 7;

/// FromGoTime translates time.Time to mysql time internal representation.
// go: FromGoTime L1
pub fn from_go_time(t: &gotime::Time) -> CoreTime {
    // Plus 500 nanosecond for rounding of the millisecond part.
    let t = t.add(500 * gotime::NANOSECOND);

    let (year, month, day) = t.date();
    let (hour, minute, second) = t.clock();
    let microsecond = t.nanosecond() / 1000;
    from_date(
        year as isize,
        month as isize,
        day as isize,
        hour as isize,
        minute as isize,
        second as isize,
        microsecond as isize,
    )
}

/// FromDate makes a internal time representation from the given date.
// go: FromDate L1 [M-INT]
pub const fn from_date(
    year: isize,
    month: isize,
    day: isize,
    hour: isize,
    minute: isize,
    second: isize,
    microsecond: isize,
) -> CoreTime {
    let mut v = ZeroCoreTime.0;
    v |= ((microsecond as u64) << microsecondBitFieldOffset) & microsecondBitFieldMask;
    v |= ((second as u64) << secondBitFieldOffset) & secondBitFieldMask;
    v |= ((minute as u64) << minuteBitFieldOffset) & minuteBitFieldMask;
    v |= ((hour as u64) << hourBitFieldOffset) & hourBitFieldMask;
    v |= ((day as u64) << dayBitFieldOffset) & dayBitFieldMask;
    v |= ((month as u64) << monthBitFieldOffset) & monthBitFieldMask;
    v |= ((year as u64) << yearBitFieldOffset) & yearBitFieldMask;
    CoreTime(v)
}

/// FromDateChecked makes a internal time representation from the given date with field overflow check.
// go: FromDateChecked L1 [M-INT]
pub fn from_date_checked(
    year: isize,
    month: isize,
    day: isize,
    hour: isize,
    minute: isize,
    second: isize,
    microsecond: isize,
) -> (CoreTime, bool) {
    if year as u64 >= (1 << yearBitFieldWidth)
        || month as u64 >= (1 << monthBitFieldWidth)
        || day as u64 >= (1 << dayBitFieldWidth)
        || hour as u64 >= (1 << hourBitFieldWidth)
        || minute as u64 >= (1 << minuteBitFieldWidth)
        || second as u64 >= (1 << secondBitFieldWidth)
        || microsecond as u64 >= (1 << microsecondBitFieldWidth)
    {
        return (ZeroCoreTime, false);
    }
    (
        from_date(year, month, day, hour, minute, second, microsecond),
        true,
    )
}

/// Time is the struct for handling datetime, timestamp and date.
///
/// Go embeds `coreTime` (the raw word, including the fsp/type bits) in `Time` and promotes
/// its methods; here the word is the field `core` and the promoted methods delegate.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Time {
    core: CoreTime,
}

/// Core time bit fields.
pub(crate) const yearBitFieldOffset: u64 = 50;
pub(crate) const yearBitFieldWidth: u64 = 14;
pub(crate) const monthBitFieldOffset: u64 = 46;
pub(crate) const monthBitFieldWidth: u64 = 4;
pub(crate) const dayBitFieldOffset: u64 = 41;
pub(crate) const dayBitFieldWidth: u64 = 5;
pub(crate) const hourBitFieldOffset: u64 = 36;
pub(crate) const hourBitFieldWidth: u64 = 5;
pub(crate) const minuteBitFieldOffset: u64 = 30;
pub(crate) const minuteBitFieldWidth: u64 = 6;
pub(crate) const secondBitFieldOffset: u64 = 24;
pub(crate) const secondBitFieldWidth: u64 = 6;
pub(crate) const microsecondBitFieldOffset: u64 = 4;
pub(crate) const microsecondBitFieldWidth: u64 = 20;

/// fspTt bit field.
/// `fspTt` format:
/// | fsp: 3 bits | type: 1 bit |
/// When `fsp` is valid (in range [0, 6]):
/// 1. `type` bit 0 represent `DateTime`
/// 2. `type` bit 1 represent `Timestamp`
///
/// Since s`Date` does not require `fsp`, we could use `fspTt` == 0b1110 to represent it.
const fspTtBitFieldOffset: u64 = 0;
const fspTtBitFieldWidth: u64 = 4;

pub(crate) const yearBitFieldMask: u64 = ((1 << yearBitFieldWidth) - 1) << yearBitFieldOffset;
pub(crate) const monthBitFieldMask: u64 = ((1 << monthBitFieldWidth) - 1) << monthBitFieldOffset;
pub(crate) const dayBitFieldMask: u64 = ((1 << dayBitFieldWidth) - 1) << dayBitFieldOffset;
pub(crate) const hourBitFieldMask: u64 = ((1 << hourBitFieldWidth) - 1) << hourBitFieldOffset;
pub(crate) const minuteBitFieldMask: u64 = ((1 << minuteBitFieldWidth) - 1) << minuteBitFieldOffset;
pub(crate) const secondBitFieldMask: u64 = ((1 << secondBitFieldWidth) - 1) << secondBitFieldOffset;
pub(crate) const microsecondBitFieldMask: u64 =
    ((1 << microsecondBitFieldWidth) - 1) << microsecondBitFieldOffset;
const fspTtBitFieldMask: u64 = ((1 << fspTtBitFieldWidth) - 1) << fspTtBitFieldOffset;

const fspTtForDate: u64 = 0b1110;
const fspBitFieldMask: u64 = 0b1110;
const coreTimeBitFieldMask: u64 = !fspTtBitFieldMask;

/// NewTime constructs time from core time, type and fsp.
// go: NewTime L1 [M-INT]
pub fn new_time(core_time: CoreTime, tp: u8, fsp: isize) -> Time {
    let mut t = ZeroTime;
    t.core.0 |= core_time.0 & coreTimeBitFieldMask;
    if tp == mysql::TypeDate {
        t.core.0 |= fspTtForDate;
        return t;
    }
    let fsp = if fsp == UnspecifiedFsp {
        DefaultFsp
    } else {
        fsp
    };
    t.core.0 |= (fsp as u64) << 1;
    if tp == mysql::TypeTimestamp {
        t.core.0 |= 1;
    }
    t
}

impl Time {
    /// `Time{ct}`: a Time whose raw word is `ct` (Go's composite literal of the embedded field).
    // glue: Go composite literal Time{coreTime}
    pub fn from_raw(ct: CoreTime) -> Time {
        Time { core: ct }
    }

    /// The raw word, the embedded `t.coreTime` (with the fsp/type bits).
    // glue: Go field access t.coreTime
    pub fn raw(&self) -> CoreTime {
        self.core
    }

    /// Clock returns the hour, minute, and second within the day specified by t.
    // go: Time.Clock L1
    pub fn clock(&self) -> (isize, isize, isize) {
        (self.hour(), self.minute(), self.second())
    }

    // go: Time.getFspTt L1
    fn get_fsp_tt(&self) -> u64 {
        self.core.0 & fspTtBitFieldMask
    }

    // go: Time.setFspTt L1
    fn set_fsp_tt(&mut self, fsp_tt: u64) {
        self.core.0 &= !fspTtBitFieldMask;
        self.core.0 |= fsp_tt;
    }

    /// Type returns type value.
    // go: Time.Type L1
    pub fn tp(&self) -> u8 {
        if self.get_fsp_tt() == fspTtForDate {
            return mysql::TypeDate;
        }
        if self.core.0 & 1 == 1 {
            return mysql::TypeTimestamp;
        }
        mysql::TypeDatetime
    }

    /// Fsp returns fsp value.
    // go: Time.Fsp L1
    pub fn fsp(&self) -> isize {
        let fsp_tt = self.get_fsp_tt();
        if fsp_tt == fspTtForDate {
            return 0;
        }
        (fsp_tt >> 1) as isize
    }

    /// SetType updates the type in Time.
    /// Only DateTime/Date/Timestamp is valid.
    // go: Time.SetType L1
    pub fn set_type(&mut self, tp: u8) {
        let mut fsp_tt = self.get_fsp_tt();
        if fsp_tt == fspTtForDate && tp != mysql::TypeDate {
            fsp_tt = 0;
        }
        match tp {
            mysql::TypeDate => fsp_tt = fspTtForDate,
            mysql::TypeTimestamp => fsp_tt |= 1,
            mysql::TypeDatetime => fsp_tt &= !1,
            _ => {}
        }
        self.set_fsp_tt(fsp_tt);
    }

    /// SetFsp updates the fsp in Time.
    // go: Time.SetFsp L1 [M-INT]
    pub fn set_fsp(&mut self, fsp: isize) {
        if self.get_fsp_tt() == fspTtForDate {
            return;
        }
        let fsp = if fsp == UnspecifiedFsp {
            DefaultFsp
        } else {
            fsp
        };
        self.core.0 &= !fspBitFieldMask;
        self.core.0 |= (fsp as u64) << 1;
    }

    /// CoreTime returns core time.
    // go: Time.CoreTime L1
    pub fn core_time(&self) -> CoreTime {
        CoreTime(self.core.0 & coreTimeBitFieldMask)
    }

    /// SetCoreTime updates core time.
    // go: Time.SetCoreTime L1
    pub fn set_core_time(&mut self, ct: CoreTime) {
        self.core.0 &= !coreTimeBitFieldMask;
        self.core.0 |= ct.0 & coreTimeBitFieldMask;
    }

    // Methods of the embedded CoreTime, promoted in Go.

    /// Year (promoted from CoreTime).
    // glue: promoted method CoreTime.Year
    pub fn year(&self) -> isize {
        self.core.year()
    }
    /// Month (promoted from CoreTime).
    // glue: promoted method CoreTime.Month
    pub fn month(&self) -> isize {
        self.core.month()
    }
    /// Day (promoted from CoreTime).
    // glue: promoted method CoreTime.Day
    pub fn day(&self) -> isize {
        self.core.day()
    }
    /// Hour (promoted from CoreTime).
    // glue: promoted method CoreTime.Hour
    pub fn hour(&self) -> isize {
        self.core.hour()
    }
    /// Minute (promoted from CoreTime).
    // glue: promoted method CoreTime.Minute
    pub fn minute(&self) -> isize {
        self.core.minute()
    }
    /// Second (promoted from CoreTime).
    // glue: promoted method CoreTime.Second
    pub fn second(&self) -> isize {
        self.core.second()
    }
    /// Microsecond (promoted from CoreTime).
    // glue: promoted method CoreTime.Microsecond
    pub fn microsecond(&self) -> isize {
        self.core.microsecond()
    }
    /// Weekday (promoted from CoreTime).
    // glue: promoted method CoreTime.Weekday
    pub fn weekday(&self) -> gotime::Weekday {
        self.core.weekday()
    }
    /// YearWeek (promoted from CoreTime).
    // glue: promoted method CoreTime.YearWeek
    pub fn year_week(&self, mode: isize) -> (isize, isize) {
        self.core.year_week(mode)
    }
    /// Week (promoted from CoreTime).
    // glue: promoted method CoreTime.Week
    pub fn week(&self, mode: isize) -> isize {
        self.core.week(mode)
    }
    /// YearDay (promoted from CoreTime).
    // glue: promoted method CoreTime.YearDay
    pub fn year_day(&self) -> isize {
        self.core.year_day()
    }
    /// GoTime (promoted from CoreTime).
    // glue: promoted method CoreTime.GoTime
    pub fn go_time(&self, loc: &Arc<Location>) -> (gotime::Time, Result<(), Error>) {
        self.core.go_time(loc)
    }
    /// AdjustedGoTime (promoted from CoreTime).
    // glue: promoted method CoreTime.AdjustedGoTime
    pub fn adjusted_go_time(&self, loc: &Arc<Location>) -> (gotime::Time, Result<(), Error>) {
        self.core.adjusted_go_time(loc)
    }
    /// IsLeapYear (promoted from CoreTime).
    // glue: promoted method CoreTime.IsLeapYear
    pub fn is_leap_year(&self) -> bool {
        self.core.is_leap_year()
    }

    /// ConvertTimeZone converts the time value from one timezone to another.
    /// The input time should be a valid timestamp.
    // go: Time.ConvertTimeZone L1
    pub fn convert_time_zone(
        &mut self,
        from: &Arc<Location>,
        to: &Arc<Location>,
    ) -> Result<(), Error> {
        if !self.is_zero() {
            let (raw, err) = self.go_time(from);
            err?;
            let converted = raw.in_loc(to);
            self.set_core_time(from_go_time(&converted));
        }
        Ok(())
    }

    /// String returns the time as `YYYY-MM-DD[ hh:mm:ss[.ffffff]]`.
    // go: Time.String L1 [M-NAME]
    pub fn string(&self) -> Vec<u8> {
        if self.tp() == mysql::TypeDate {
            // We control the format, so no error would occur.
            let (s, err) = date_format_result(self.date_format(b"%Y-%m-%d"));
            terror::log(err.err().as_ref());
            return s;
        }

        let (mut s, err) = date_format_result(self.date_format(b"%Y-%m-%d %H:%i:%s"));
        terror::log(err.err().as_ref());
        let fsp = self.fsp();
        if fsp > 0 {
            let tmp = format!(".{:06}", self.microsecond());
            s.extend_from_slice(&tmp.as_bytes()[..1 + fsp as usize]);
        }

        s
    }

    /// IsZero returns a boolean indicating whether the time is equal to ZeroCoreTime.
    // go: Time.IsZero L1
    pub fn is_zero(&self) -> bool {
        compare_time(self.core, ZeroCoreTime) == 0
    }

    /// InvalidZero returns a boolean indicating whether the month or day is zero.
    /// Several functions are strict when passed a DATE() function value as their argument and reject incomplete dates with a day part of zero:
    /// CONVERT_TZ(), DATE_ADD(), DATE_SUB(), DAYOFYEAR(), TIMESTAMPDIFF(),
    /// TO_DAYS(), TO_SECONDS(), WEEK(), WEEKDAY(), WEEKOFYEAR(), YEARWEEK().
    /// Mysql Doc: https://dev.mysql.com/doc/refman/5.7/en/date-and-time-functions.html
    // go: Time.InvalidZero L1
    pub fn invalid_zero(&self) -> bool {
        self.month() == 0 || self.day() == 0
    }

    /// ToNumber returns a formatted number.
    /// e.g,
    /// 2012-12-12 -> 20121212
    /// 2012-12-12T10:10:10 -> 20121212101010
    /// 2012-12-12T10:10:10.123456 -> 20121212101010.123456
    // go: Time.ToNumber L1
    pub fn to_number(&self) -> MyDecimal {
        let mut dec = MyDecimal::default();
        self.fill_number(&mut dec);
        dec
    }

    /// FillNumber is the same as ToNumber,
    /// but reuses input decimal instead of allocating one.
    // go: Time.FillNumber L1
    pub fn fill_number(&self, dec: &mut MyDecimal) {
        if self.is_zero() {
            dec.from_int(0);
            return;
        }

        // Fix issue #1046
        // Prevents from converting 2012-12-12 to 20121212000000
        let tf_str: &[u8] = if self.tp() == mysql::TypeDate {
            dateFormat
        } else {
            numberFormat
        };

        let (mut s, err) = date_format_result(self.date_format(tf_str));
        if err.is_err() {
            log::error!("never happen because we've control the format!; category=fatal");
        }

        let fsp = self.fsp();
        if fsp > 0 {
            let s1 = format!("{}.{:06}", String::from_utf8_lossy(&s), self.microsecond());
            let n = s.len() + fsp as usize + 1;
            s = s1.as_bytes()[..n].to_vec();
        }
        // We skip checking error here because time formatted string can be parsed certainly.
        let err = dec.from_string(&s);
        terror::log(err.err().as_ref());
    }

    /// Convert converts t with type tp.
    // go: Time.Convert L1 [M-ERRVAL]
    pub fn convert(&self, ctx: &Context, tp: u8) -> (Time, Result<(), Error>) {
        let mut t1 = *self;
        if self.tp() == tp || self.is_zero() {
            t1.set_type(tp);
            return (t1, Ok(()));
        }

        t1.set_type(tp);
        let err = t1.check(ctx);
        if tp == mysql::TypeTimestamp
            && let Err(e) = &err
            && ErrTimestampInDSTTransition.equal(e)
        {
            let (t_adj, adj_err) = t1.adjusted_go_time(&ctx.location());
            if adj_err.is_ok() {
                ctx.append_warning(e.clone());
                return (Time::from_raw(from_go_time(&t_adj)), Ok(()));
            }
        }
        (t1, err)
    }

    /// ConvertToDuration converts mysql datetime, timestamp and date to mysql time type.
    /// e.g,
    /// 2012-12-12T10:10:10 -> 10:10:10
    /// 2012-12-12 -> 0
    // go: Time.ConvertToDuration L1
    pub fn convert_to_duration(&self) -> Result<Duration, Error> {
        if self.is_zero() {
            return Ok(ZeroDuration);
        }

        let (hour, minute, second) = self.clock();
        let frac = self.microsecond() * 1000;

        let d = (hour * 3600 + minute * 60 + second) as i64 * gotime::SECOND + frac as i64;
        // TODO: check convert validation
        Ok(Duration {
            duration: d,
            fsp: self.fsp(),
        })
    }

    /// Compare returns an integer comparing the time instant t to o.
    /// If t is after o, returns 1, equal o, returns 0, before o, returns -1.
    // go: Time.Compare L1
    pub fn compare(&self, o: &Time) -> isize {
        compare_time(self.core, o.core)
    }

    /// CompareString is like Compare,
    /// but parses string to Time then compares.
    // go: Time.CompareString L1
    pub fn compare_string(&self, ctx: &Context, str: &[u8]) -> Result<isize, Error> {
        // use MaxFsp to parse the string
        let (o, err) = parse_time(ctx, str, self.tp(), MaxFsp);
        err?;

        Ok(self.compare(&o))
    }

    /// RoundFrac rounds the fraction part of a time-type value according to `fsp`.
    // go: Time.RoundFrac L1 [M-ERRVAL]
    pub fn round_frac(&self, ctx: &Context, fsp: isize) -> (Time, Result<(), Error>) {
        if self.tp() == mysql::TypeDate || self.is_zero() {
            // date type has no fsp
            return (*self, Ok(()));
        }

        let (fsp, err) = check_fsp(fsp);
        if let Err(e) = err {
            return (*self, Err(e));
        }

        if fsp == self.fsp() {
            // have same fsp
            return (*self, Ok(()));
        }

        let nt;
        let loc = ctx.location();
        let (t1, err) = self.go_time(&loc);
        if err.is_ok() {
            let t1 = round_time(&t1, fsp);
            nt = from_go_time(&t1);
        } else {
            // Take the hh:mm:ss part out to avoid handle month or day = 0.
            let (hour, minute, second, microsecond) = (
                self.hour(),
                self.minute(),
                self.second(),
                self.microsecond(),
            );
            let t1 = gotime::date(
                1,
                1,
                1,
                hour as i64,
                minute as i64,
                second as i64,
                microsecond as i64 * 1000,
                &loc,
            );
            let t2 = round_time(&t1, fsp);
            let (hour, minute, second) = t2.clock();
            let microsecond = t2.nanosecond() / 1000;

            // TODO: when hh:mm:ss overflow one day after rounding, it should be add to yy:mm:dd part,
            // but mm:dd may contain 0, it makes the code complex, so we ignore it here.
            if t2.day() - 1 > 0 {
                return (
                    *self,
                    Err(ErrWrongValue
                        .gen_with_stack_by_args(vec![Arg::from(TimeStr), go_str(self.string())])),
                );
            }
            let (ct, ok) = from_date_checked(
                self.year(),
                self.month(),
                self.day(),
                hour as isize,
                minute as isize,
                second as isize,
                microsecond as isize,
            );
            if !ok {
                return (
                    *self,
                    Err(ErrWrongValue
                        .gen_with_stack_by_args(vec![Arg::from(TimeStr), go_str(self.string())])),
                );
            }
            nt = ct;
        }

        (new_time(nt, self.tp(), fsp), Ok(()))
    }

    /// MarshalJSON implements Marshaler.MarshalJSON interface.
    // go: Time.MarshalJSON L1
    pub fn marshal_json(&self) -> Result<Vec<u8>, Error> {
        Ok(self.core.0.to_string().into_bytes())
    }

    /// UnmarshalJSON implements Unmarshaler.UnmarshalJSON interface.
    // go: Time.UnmarshalJSON L1
    pub fn unmarshal_json(&mut self, data: &[u8]) -> Result<(), Error> {
        match crate::gostd::json_unmarshal_uint64(data, self.core.0) {
            Ok(v) => {
                self.core.0 = v;
                Ok(())
            }
            Err(msg) => Err(Error::plain(msg)),
        }
    }

    /// ToPackedUint encodes Time to a packed uint64 value.
    ///
    /// ```text
    ///     1 bit  0
    ///    17 bits year*13+month   (year 0-9999, month 0-12)
    ///     5 bits day             (0-31)
    ///     5 bits hour            (0-23)
    ///     6 bits minute          (0-59)
    ///     6 bits second          (0-59)
    ///    24 bits microseconds    (0-999999)
    ///
    ///    Total: 64 bits = 8 bytes
    ///
    ///    0YYYYYYY.YYYYYYYY.YYdddddh.hhhhmmmm.mmssssss.ffffffff.ffffffff.ffffffff
    /// ```
    // go: Time.ToPackedUint L1 [M-INT]
    pub fn to_packed_uint(&self) -> Result<u64, Error> {
        let tm = *self;
        if self.is_zero() {
            return Ok(0);
        }
        let (year, month, day) = (tm.year(), tm.month(), tm.day());
        let (hour, minute, sec) = (tm.hour(), tm.minute(), tm.second());
        let ymd = (((year * 13 + month) << 5) | day) as u64;
        let hms = (hour << 12 | minute << 6 | sec) as u64;
        let micro = tm.microsecond() as u64;
        Ok(((ymd << 17 | hms) << 24) | micro)
    }

    /// FromPackedUint decodes Time from a packed uint64 value.
    // go: Time.FromPackedUint L1 [M-INT]
    pub fn from_packed_uint(&mut self, packed: u64) -> Result<(), Error> {
        if packed == 0 {
            self.set_core_time(ZeroCoreTime);
            return Ok(());
        }
        let ymdhms = packed >> 24;
        let ymd = ymdhms >> 17;
        let day = (ymd & ((1 << 5) - 1)) as isize;
        let ym = ymd >> 5;
        let month = (ym % 13) as isize;
        let year = (ym / 13) as isize;

        let hms = ymdhms & ((1 << 17) - 1);
        let second = (hms & ((1 << 6) - 1)) as isize;
        let minute = ((hms >> 6) & ((1 << 6) - 1)) as isize;
        let hour = (hms >> 12) as isize;
        let microsec = (packed % (1 << 24)) as isize;

        self.set_core_time(from_date(year, month, day, hour, minute, second, microsec));

        Ok(())
    }

    /// Check function checks whether t matches valid Time format.
    /// If allowZeroInDate is false, it returns ErrZeroDate when month or day is zero.
    /// FIXME: See https://dev.mysql.com/doc/refman/5.7/en/sql-mode.html#sqlmode_no_zero_in_date
    // go: Time.Check L1
    pub fn check(&self, ctx: &Context) -> Result<(), Error> {
        let allow_zero_in_date = ctx.flags().ignore_zero_in_date();
        let allow_invalid_date = ctx.flags().ignore_invalid_date_err();
        match self.tp() {
            mysql::TypeTimestamp => check_timestamp_type(self.core, &ctx.location()),
            mysql::TypeDatetime | mysql::TypeDate => {
                check_datetime_type(self.core, allow_zero_in_date, allow_invalid_date)
            }
            _ => Ok(()),
        }
    }

    /// Sub subtracts t1 from t, returns a duration value.
    /// Note that sub should not be done on different time types.
    // go: Time.Sub L1
    pub fn sub(&self, ctx: &Context, t1: &Time) -> Duration {
        let duration;
        if self.tp() == mysql::TypeTimestamp && t1.tp() == mysql::TypeTimestamp {
            let loc = ctx.location();
            let (a, err) = self.go_time(&loc);
            terror::log(err.err().as_ref());
            let (b, err) = t1.go_time(&loc);
            terror::log(err.err().as_ref());
            duration = a.sub(&b);
        } else {
            let (seconds, microseconds, neg) = calc_time_time_diff(self.core, t1.core, 1);
            let mut d = (seconds as i64)
                .wrapping_mul(1_000_000_000)
                .wrapping_add(microseconds as i64 * 1000);
            if neg {
                d = d.wrapping_neg();
            }
            duration = d;
        }

        let mut fsp = self.fsp();
        let fsp1 = t1.fsp();
        if fsp < fsp1 {
            fsp = fsp1;
        }
        Duration { duration, fsp }
    }

    /// Add adds d to t, returns the result time value.
    // go: Time.Add L1 [M-ERRVAL, M-INT]
    pub fn add(&self, ctx: &Context, d: Duration) -> (Time, Result<(), Error>) {
        let (seconds, microseconds, _) = calc_time_duration_diff(self.core, d);
        let days = seconds / secondsIn24Hour;
        let (year, month, day) = get_date_from_daynr(days as usize);
        let mut tm = ZeroTime;
        tm.core.set_year(year as u16);
        tm.core.set_month(month as u8);
        tm.core.set_day(day as u8);
        calc_time_from_sec(&mut tm.core, seconds % secondsIn24Hour, microseconds);
        if self.tp() == mysql::TypeDate {
            tm.core.set_hour(0);
            tm.core.set_minute(0);
            tm.core.set_second(0);
            tm.core.set_microsecond(0);
        }
        let mut fsp = self.fsp();
        if d.fsp > fsp {
            fsp = d.fsp;
        }
        let ret = new_time(tm.core, self.tp(), fsp);
        let err = ret.check(ctx);
        (ret, err)
    }

    /// DateFormat returns a textual representation of the time value formatted
    /// according to layout.
    /// See http://dev.mysql.com/doc/refman/5.7/en/date-and-time-functions.html#function_date-format
    // go: Time.DateFormat L1
    pub fn date_format(&self, layout: &[u8]) -> Result<Vec<u8>, Error> {
        let mut buf: Vec<u8> = Vec::with_capacity(layout.len() + 8);
        let mut in_pattern_match = false;
        let mut rest = layout;
        while !rest.is_empty() {
            let (r, n) = decode_rune(rest);
            let b = r.unwrap_or(char::REPLACEMENT_CHARACTER);
            rest = &rest[n..];
            if in_pattern_match {
                self.convert_date_format(b, &mut buf)?;
                in_pattern_match = false;
                continue;
            }

            // It's not in pattern match now.
            if b == '%' {
                in_pattern_match = true;
            } else {
                write_rune(&mut buf, b);
            }
        }
        Ok(buf)
    }

    // go: Time.convertDateFormat L1
    fn convert_date_format(&self, b: char, buf: &mut Vec<u8>) -> Result<(), Error> {
        match b {
            'b' => {
                let m = self.month();
                if m == 0 || m > 12 {
                    return Err(ErrWrongValue.gen_with_stack_by_args(vec![
                        Arg::from(TimeStr),
                        Arg::from(m.to_string()),
                    ]));
                }
                buf.extend_from_slice(&MonthNames[(m - 1) as usize].as_bytes()[..3]);
            }
            'M' => {
                let m = self.month();
                if m == 0 || m > 12 {
                    return Err(ErrWrongValue.gen_with_stack_by_args(vec![
                        Arg::from(TimeStr),
                        Arg::from(m.to_string()),
                    ]));
                }
                buf.extend_from_slice(MonthNames[(m - 1) as usize].as_bytes());
            }
            'm' => buf.extend_from_slice(&format_int_width_n(self.month(), 2)),
            'c' => buf.extend_from_slice(self.month().to_string().as_bytes()),
            'D' => {
                buf.extend_from_slice(self.day().to_string().as_bytes());
                buf.extend_from_slice(abbr_day_of_month(self.day()).as_bytes());
            }
            'd' => buf.extend_from_slice(&format_int_width_n(self.day(), 2)),
            'e' => buf.extend_from_slice(self.day().to_string().as_bytes()),
            'j' => buf.extend_from_slice(format!("{:03}", self.year_day()).as_bytes()),
            'H' => buf.extend_from_slice(&format_int_width_n(self.hour(), 2)),
            'k' => buf.extend_from_slice(self.hour().to_string().as_bytes()),
            'h' | 'I' => {
                let tt = self.hour();
                if tt % 12 == 0 {
                    buf.extend_from_slice(b"12");
                } else {
                    buf.extend_from_slice(&format_int_width_n(tt % 12, 2));
                }
            }
            'l' => {
                let tt = self.hour();
                if tt % 12 == 0 {
                    buf.extend_from_slice(b"12");
                } else {
                    buf.extend_from_slice((tt % 12).to_string().as_bytes());
                }
            }
            'i' => buf.extend_from_slice(&format_int_width_n(self.minute(), 2)),
            'p' => {
                let hour = self.hour();
                if hour / 12 % 2 == 0 {
                    buf.extend_from_slice(b"AM");
                } else {
                    buf.extend_from_slice(b"PM");
                }
            }
            'r' => {
                let h = self.hour() % 24;
                let (mi, s) = (self.minute(), self.second());
                let out = if h == 0 {
                    format!("{:02}:{:02}:{:02} AM", 12, mi, s)
                } else if h == 12 {
                    format!("{:02}:{:02}:{:02} PM", 12, mi, s)
                } else if h < 12 {
                    format!("{:02}:{:02}:{:02} AM", h, mi, s)
                } else {
                    format!("{:02}:{:02}:{:02} PM", h - 12, mi, s)
                };
                buf.extend_from_slice(out.as_bytes());
            }
            'T' => buf.extend_from_slice(
                format!(
                    "{:02}:{:02}:{:02}",
                    self.hour(),
                    self.minute(),
                    self.second()
                )
                .as_bytes(),
            ),
            'S' | 's' => buf.extend_from_slice(&format_int_width_n(self.second(), 2)),
            'f' => buf.extend_from_slice(format!("{:06}", self.microsecond()).as_bytes()),
            'U' => {
                let w = self.week(0);
                buf.extend_from_slice(&format_int_width_n(w, 2));
            }
            'u' => {
                let w = self.week(1);
                buf.extend_from_slice(&format_int_width_n(w, 2));
            }
            'V' => {
                let w = self.week(2);
                buf.extend_from_slice(&format_int_width_n(w, 2));
            }
            'v' => {
                let (_, w) = self.year_week(3);
                buf.extend_from_slice(&format_int_width_n(w, 2));
            }
            'a' => {
                let weekday = self.weekday();
                buf.extend_from_slice(abbrevWeekdayName[weekday as usize].as_bytes());
            }
            'W' => buf.extend_from_slice(gotime::weekday_string(self.weekday()).as_bytes()),
            'w' => buf.extend_from_slice(self.weekday().to_string().as_bytes()),
            'X' => {
                let (year, _) = self.year_week(2);
                if year < 0 {
                    buf.extend_from_slice(u64::from(u32::MAX).to_string().as_bytes());
                } else {
                    buf.extend_from_slice(&format_int_width_n(year, 4));
                }
            }
            'x' => {
                let (year, _) = self.year_week(3);
                if year < 0 {
                    buf.extend_from_slice(u64::from(u32::MAX).to_string().as_bytes());
                } else {
                    buf.extend_from_slice(&format_int_width_n(year, 4));
                }
            }
            'Y' => buf.extend_from_slice(&format_int_width_n(self.year(), 4)),
            'y' => {
                let str = format_int_width_n(self.year(), 4);
                buf.extend_from_slice(&str[2..]);
            }
            _ => write_rune(buf, b),
        }

        Ok(())
    }

    /// StrToDate converts date string according to format.
    /// See https://dev.mysql.com/doc/refman/5.7/en/date-and-time-functions.html#function_date-format
    // go: Time.StrToDate L1
    pub fn str_to_date(&mut self, type_ctx: &Context, date: &[u8], format: &[u8]) -> bool {
        let mut ctx: HashMap<&'static str, isize> = HashMap::new();
        let mut tm = ZeroCoreTime;
        let (success, warning) = str_to_date(&mut tm, date, format, &mut ctx);
        if !success {
            self.set_core_time(ZeroCoreTime);
            self.set_type(mysql::TypeDatetime);
            self.set_fsp(0);
            return false;
        }
        if mysql_time_fix(&mut tm, &ctx).is_err() {
            return false;
        }

        self.set_core_time(tm);
        self.set_type(mysql::TypeDatetime);
        if self.check(type_ctx).is_err() {
            return false;
        }
        if warning {
            // Only append this warning when success but still need warning.
            // Currently this only happens when `date` has extra characters at the end.
            type_ctx.append_warning(
                ErrTruncatedWrongVal.fast_gen_by_args(vec![Arg::from(DateTimeStr), go_str(date)]),
            );
        }
        true
    }
}

// glue: Go fmt.Stringer-free string argument (a Go string, bytes)
fn go_str(b: impl Into<Vec<u8>>) -> Arg {
    Arg::go_string(b.into())
}

// glue: `str, err := t.DateFormat(...)` keeping both results (Go returns "" with the error)
fn date_format_result(r: Result<Vec<u8>, Error>) -> (Vec<u8>, Result<(), Error>) {
    match r {
        Ok(s) => (s, Ok(())),
        Err(e) => (Vec::new(), Err(e)),
    }
}

/// `bytes.Buffer.WriteRune`: UTF-8 of `r` (U+FFFD for an invalid byte, as Go's range loop yields).
// go-std: bytes.Buffer.WriteRune L1
fn write_rune(buf: &mut Vec<u8>, r: char) {
    let mut tmp = [0u8; 4];
    buf.extend_from_slice(r.encode_utf8(&mut tmp).as_bytes());
}

/// CurrentTime returns current time with type tp.
// go: CurrentTime L1
pub fn current_time(tp: u8) -> Time {
    new_time(from_go_time(&gotime::now()), tp, 0)
}

const numberFormat: &[u8] = b"%Y%m%d%H%i%s";
const dateFormat: &[u8] = b"%Y%m%d";

/// roundTime rounds the time value according to digits count specified by fsp.
// go: roundTime L1 [M-F2I]
fn round_time(t: &gotime::Time, fsp: isize) -> gotime::Time {
    // [M-F2I] gotime.Duration(math.Pow10(9 - fsp)): an exact small integer
    let d = tidb_base::gostd::conv::f64_to_i64(crate::fsp::pow10(9 - fsp));
    t.round(d)
}

/// GetFsp gets the fsp of a string.
// go: GetFsp L1
pub fn get_fsp(s: &[u8]) -> isize {
    let index = get_frac_index(s);
    let fsp = if index < 0 {
        0
    } else {
        s.len() as isize - index - 1
    };
    fsp.min(6)
}

/// GetFracIndex finds the last '.' for get fracStr, index = -1 means fracStr not found.
/// but for format like '2019.01.01 00:00:00', the index should be -1.
///
/// It will not be affected by the time zone suffix.
/// For format like '2020-01-01 12:00:00.123456+05:00' and `2020-01-01 12:00:00.123456-05:00`, the index should be 19.
/// related issue https://github.com/pingcap/tidb/issues/35291 and https://github.com/pingcap/tidb/issues/49555
// go: GetFracIndex L1
pub fn get_frac_index(s: &[u8]) -> isize {
    let (tz_index, ..) = get_timezone(s);
    let end: isize = if tz_index != -1 {
        tz_index - 1
    } else {
        s.len() as isize - 1
    };
    let mut index = -1;
    let mut i = end;
    while i >= 0 {
        let c = s[i as usize];
        if c != b'+' && c != b'-' && is_punctuation(c) {
            if c == b'.' {
                index = i;
            }
            break;
        }
        i -= 1;
    }

    index
}

/// RoundFrac rounds fractional seconds precision with new fsp and returns a new one.
/// We will use the “round half up” rule, e.g, >= 0.5 -> 1, < 0.5 -> 0,
/// so 2011:11:11 10:10:10.888888 round 0 -> 2011:11:11 10:10:11
/// and 2011:11:11 10:10:10.111111 round 0 -> 2011:11:11 10:10:10
// go: RoundFrac L1 [M-ERRVAL, M-F2I]
pub fn round_frac(t: &gotime::Time, fsp: isize) -> (gotime::Time, Result<(), Error>) {
    let (_, err) = check_fsp(fsp);
    if let Err(e) = err {
        return (t.clone(), Err(e));
    }
    // [M-F2I]
    let d = tidb_base::gostd::conv::f64_to_i64(crate::fsp::pow10(9 - fsp));
    (t.round(d.wrapping_mul(gotime::NANOSECOND)), Ok(()))
}

/// TruncateFrac truncates fractional seconds precision with new fsp and returns a new one.
/// 2011:11:11 10:10:10.888888 round 0 -> 2011:11:11 10:10:10
/// 2011:11:11 10:10:10.111111 round 0 -> 2011:11:11 10:10:10
// go: TruncateFrac L1 [M-ERRVAL, M-F2I]
pub fn truncate_frac(t: &gotime::Time, fsp: isize) -> (gotime::Time, Result<(), Error>) {
    let (_, err) = check_fsp(fsp);
    if let Err(e) = err {
        return (t.clone(), Err(e));
    }
    // [M-F2I]
    let d = tidb_base::gostd::conv::f64_to_i64(crate::fsp::pow10(9 - fsp));
    (t.truncate(d.wrapping_mul(gotime::NANOSECOND)), Ok(()))
}

/// TimestampDiff returns t2 - t1 where t1 and t2 are date or datetime expressions.
/// The unit for the result (an integer) is given by the unit argument.
/// The legal values for unit are "YEAR" "QUARTER" "MONTH" "DAY" "HOUR" "SECOND" and so on.
// go: TimestampDiff L1
pub fn timestamp_diff_of(unit: &[u8], t1: Time, t2: Time) -> i64 {
    timestamp_diff(unit, t1.core, t2.core)
}

/// ParseDateFormat parses a formatted date string and returns separated components.
// go: ParseDateFormat L1
pub fn parse_date_format(format: &[u8]) -> Option<Vec<&[u8]>> {
    let format = trim_space_go(format);

    if format.is_empty() {
        return None;
    }

    // Date format must start with number.
    if !is_digit(format[0]) {
        return None;
    }

    let mut start = 0;
    // Initialize `seps` with capacity of 6. The input `format` is typically
    // a date time of the form time.DateTime, which has 6 numeric parts
    // (the fractional second part is usually removed by `splitDateTime`).
    // Setting `seps`'s capacity to 6 avoids reallocation in this common case.
    let mut seps: Vec<&[u8]> = Vec::with_capacity(6);

    let mut i = 1;
    while i + 1 < format.len() {
        if is_valid_separator(format[i], seps.len()) {
            let prev_parts = seps.len();
            seps.push(&format[start..i]);
            start = i + 1;

            // consume further consecutive separators
            let mut j = i + 1;
            while j < format.len() {
                if !is_valid_separator(format[j], prev_parts) {
                    break;
                }

                start += 1;
                i += 1;
                j += 1;
            }

            i += 1;
            continue;
        }

        if !is_digit(format[i]) {
            return None;
        }
        i += 1;
    }

    seps.push(&format[start..]);
    Some(seps)
}

/// `strings.TrimSpace` of a Go string (bytes; Unicode white space as Go defines it).
// go-std: strings.TrimSpace L1
fn trim_space_go(s: &[u8]) -> &[u8] {
    crate::gostd::trim_space(s)
}

/// helper for date part splitting, punctuation characters are valid separators anywhere,
/// while space and 'T' are valid separators only between date and time.
// go: isValidSeparator L1
fn is_valid_separator(c: u8, prev_parts: usize) -> bool {
    if is_punctuation(c) {
        return true;
    }

    // for https://github.com/pingcap/tidb/issues/32232
    if prev_parts == 2
        && (c == b'T'
            || c == b' '
            || c == b'\t'
            || c == b'\n'
            || c == 0x0b
            || c == 0x0c
            || c == b'\r')
    {
        return true;
    }

    if prev_parts > 4 && !is_digit(c) {
        return true;
    }
    false
}

/// validIdxCombinations: the (h, m) offsets from the end for each combination of the
/// positions of `Z`, the sign and the separator (see GetTimezone).
// glue: the package map validIdxCombinations as a match
fn valid_idx_combinations(k: isize) -> Option<(isize, isize)> {
    match k {
        100 => Some((0, 0)), // 23:59:59Z
        30 => Some((2, 0)),  // 23:59:59+08
        50 => Some((4, 2)),  // 23:59:59+0800
        63 => Some((5, 2)),  // 23:59:59+08:00
        // postgres supports the following additional syntax that deviates from ISO8601, although we won't support it
        // currently, it will be fairly easy to add in the current parsing framework
        // 23:59:59Z+08
        // 23:59:59Z+08:00
        _ => None,
    }
}

/// The result of GetTimezone: (idx, tzSign, tzHour, tzSep, tzMinute).
pub type TimezoneParts<'a> = (isize, &'a [u8], &'a [u8], &'a [u8], &'a [u8]);

/// GetTimezone parses the trailing timezone information of a given time string literal. If idx = -1 is returned, it
/// means timezone information not found, otherwise it indicates the index of the starting index of the timezone
/// information. If the timezone contains sign, hour part and/or minute part, it will be returned as is, otherwise an
/// empty string will be returned.
///
/// Supported syntax:
///
///    MySQL compatible: ((?P<tz_sign>[-+])(?P<tz_hour>[0-9]{2}):(?P<tz_minute>[0-9]{2})){0,1}$, see
///      https://dev.mysql.com/doc/refman/8.0/en/time-zone-support.html and https://dev.mysql.com/doc/refman/8.0/en/datetime.html
///      the first link specified that timezone information should be in "[H]H:MM, prefixed with a + or -" while the
///      second link specified that for string literal, "hour values less than than 10, a leading zero is required.".
///    ISO-8601: Z|((((?P<tz_sign>[-+])(?P<tz_hour>[0-9]{2})(:(?P<tz_minute>[0-9]{2}){0,1}){0,1})|((?P<tz_minute>[0-9]{2}){0,1}){0,1}))$
///      see https://www.cl.cam.ac.uk/~mgk25/iso-time.html
// go: GetTimezone L1
pub fn get_timezone(lit: &[u8]) -> TimezoneParts<'_> {
    let (mut idx, mut zidx, mut sidx, mut spidx): (isize, isize, isize, isize) = (-1, -1, -1, -1);
    // idx is for the position of the starting of the timezone information
    // zidx is for the z symbol
    // sidx is for the sign
    // spidx is for the separator
    let l = lit.len() as isize;
    // the following loop finds the first index of Z, sign, and separator from backwards.
    let mut i = l - 1;
    while 0 <= i {
        let c = lit[i as usize];
        if c == b'Z' {
            zidx = i;
            break;
        }
        if sidx == -1 && (c == b'-' || c == b'+') {
            sidx = i;
        }
        if spidx == -1 && c == b':' {
            spidx = i;
        }
        i -= 1;
    }
    // we could enumerate all valid combinations of these values and look it up in a table, see validIdxCombinations
    // zidx can be -1 (23:59:59+08:00), l-1 (23:59:59Z)
    // sidx can be -1, l-3, l-5, l-6
    // spidx can be -1, l-3
    let mut k = 0;
    if l - zidx == 1 {
        k += 100;
    }
    let t = l - sidx;
    if t == 3 || t == 5 || t == 6 {
        k += t * 10;
    }
    if l - spidx == 3 {
        k += 3;
    }
    let none: TimezoneParts<'_> = (-1, b"", b"", b"", b"");
    if let Some((vh, vm)) = valid_idx_combinations(k) {
        let (hidx, midx) = (l - vh, l - vm);
        let valid = |v: &[u8]| v[0].is_ascii_digit() && v[1].is_ascii_digit();
        let (mut tz_sign, mut tz_hour, mut tz_sep, mut tz_minute): (&[u8], &[u8], &[u8], &[u8]) =
            (b"", b"", b"", b"");
        if sidx != -1 {
            tz_sign = &lit[sidx as usize..sidx as usize + 1];
            idx = sidx;
        }
        if zidx != -1 {
            idx = zidx;
        }
        if (l - spidx) == 3 {
            tz_sep = &lit[spidx as usize..spidx as usize + 1];
        }
        if vh != 0 {
            tz_hour = &lit[hidx as usize..hidx as usize + 2];
            if !valid(tz_hour) {
                return none;
            }
        }
        if vm != 0 {
            tz_minute = &lit[midx as usize..midx as usize + 2];
            if !valid(tz_minute) {
                return none;
            }
        }
        return (idx, tz_sign, tz_hour, tz_sep, tz_minute);
    }
    none
}

/// The result of splitDateTime.
struct SplitDateTime<'a> {
    seps: Option<Vec<&'a [u8]>>,
    frac_str: &'a [u8],
    has_tz: bool,
    tz_sign: &'a [u8],
    tz_hour: &'a [u8],
    tz_sep: &'a [u8],
    tz_minute: &'a [u8],
    truncated: bool,
}

/// See https://dev.mysql.com/doc/refman/5.7/en/date-and-time-literals.html.
/// splitDateTime splits the string literal into 3 parts, date & time, FSP(Fractional Seconds Precision) and time zone.
/// For FSP, The only delimiter recognized between a date & time part and a fractional seconds part is the decimal point,
/// therefore we could look from backwards at the literal to find the index of the decimal point.
/// For time zone, the possible delimiter could be +/- (w.r.t. MySQL 8.0, see
/// https://dev.mysql.com/doc/refman/8.0/en/datetime.html) and Z/z (w.r.t. ISO 8601, see section Time zone in
/// https://www.cl.cam.ac.uk/~mgk25/iso-time.html). We also look from backwards for the delimiter, see GetTimezone.
// go: splitDateTime L1
fn split_date_time(format: &[u8]) -> SplitDateTime<'_> {
    let (mut tz_index, tz_sign, tz_hour, tz_sep, tz_minute) = get_timezone(format);
    let mut format = format;
    let mut has_tz = false;
    let mut truncated = false;
    let mut frac_str: &[u8] = b"";
    if tz_index > 0 {
        has_tz = true;
        while tz_index > 0 && is_punctuation(format[tz_index as usize - 1]) {
            // In case of multiple separators, e.g. 2020-10--10
            tz_index -= 1;
        }
        format = &format[..tz_index as usize];
    }
    let mut frac_index = get_frac_index(format);
    if frac_index > 0 {
        // Only contain digits
        let mut frac_end = frac_index as usize + 1;
        while frac_end < format.len() && is_digit(format[frac_end]) {
            frac_end += 1;
        }
        truncated = frac_end != format.len();
        frac_str = &format[frac_index as usize + 1..frac_end];
        while frac_index > 0 && is_punctuation(format[frac_index as usize - 1]) {
            // In case of multiple separators, e.g. 2020-10..10
            frac_index -= 1;
        }
        format = &format[..frac_index as usize];
    }
    let seps = parse_date_format(format);
    SplitDateTime {
        seps,
        frac_str,
        has_tz,
        tz_sign,
        tz_hour,
        tz_sep,
        tz_minute,
        truncated,
    }
}

/// `fmt.Sscanf(s, "%Nd%Md...", ...)` for the digit-only strings `parseDatetime` scans:
/// each verb reads at most its width of ASCII digits (after an optional sign). Returns
/// Go's error when the input runs out at a verb (`io.EOF`, "EOF") or a verb finds no
/// digits ("expected integer"); the destinations of the verbs not reached keep their values.
/// `trailing_space` is a space after the last verb (`"%2d "`).
// go-std: fmt.Sscanf L2 (only %Nd verbs, an optional trailing space verb, on [0-9]* input)
fn sscanf_digits(
    s: &[u8],
    widths: &[usize],
    trailing_space: bool,
    out: &mut [&mut isize],
) -> Result<(), Error> {
    let mut pos = 0;
    for (&w, dst) in widths.iter().zip(out.iter_mut()) {
        if pos >= s.len() {
            return Err(Error::plain("EOF"));
        }
        let mut end = pos;
        let mut limit = pos + w;
        if s[end] == b'+' || s[end] == b'-' {
            end += 1;
        }
        let digits_start = end;
        limit = limit.min(s.len());
        while end < limit && s[end].is_ascii_digit() {
            end += 1;
        }
        if end == digits_start {
            return Err(Error::plain("expected integer"));
        }
        let (v, _) = atoi(&s[pos..end]);
        **dst = v;
        pos = end;
    }
    // A space at the end of the format matches zero or more spaces, so the next input rune
    // must be a space or the end (fmt's ss.advance).
    if trailing_space && pos < s.len() && !crate::util_parser::is_space_rune(char::from(s[pos])) {
        return Err(Error::plain("expected space in input to match format"));
    }
    Ok(())
}

/// See https://dev.mysql.com/doc/refman/5.7/en/date-and-time-literals.html.
// go: parseDatetime L1
fn parse_datetime(ctx: &Context, str: &[u8], fsp: isize, is_float: bool) -> Result<Time, Error> {
    let (mut year, mut month, mut day, mut hour, mut minute, mut second): (
        isize,
        isize,
        isize,
        isize,
        isize,
        isize,
    ) = (0, 0, 0, 0, 0, 0);
    let (mut delta_hour, mut delta_minute) = (0isize, 0isize);
    let mut hhmmss = false;
    let mut err: Result<(), Error> = Ok(());

    let sp = split_date_time(str);
    let mut seps: Vec<Vec<u8>> = sp
        .seps
        .unwrap_or_default()
        .into_iter()
        .map(|s| s.to_vec())
        .collect();
    let mut frac_str: &[u8] = sp.frac_str;
    let mut has_tz = sp.has_tz;
    let (tz_sign, tz_hour, tz_sep, tz_minute) = (sp.tz_sign, sp.tz_hour, sp.tz_sep, sp.tz_minute);
    let mut truncated_or_incorrect = sp.truncated;
    if truncated_or_incorrect {
        ctx.append_warning(
            ErrTruncatedWrongVal.fast_gen_by_args(vec![Arg::from("datetime"), go_str(str)]),
        );
    }

    // noAbsorb tests if can absorb FSP or TZ
    let no_absorb = |seps: &Vec<Vec<u8>>| -> bool {
        // if we have more than 5 parts (i.e. 6), the tailing part can't be absorbed
        // or if we only have 1 part, but its length is longer than 4, then it is at least YYMMD, in this case, FSP can
        // not be absorbed, and it will be handled later, and the leading sign prevents TZ from being absorbed, because
        // if date part has no separators, we can't use -/+ as separators between date & time.
        seps.len() > 5 || (seps.len() == 1 && seps[0].len() > 4)
    };
    if !frac_str.is_empty() && !is_float && !no_absorb(&seps) {
        seps.push(frac_str.to_vec());
        frac_str = b"";
    }
    if has_tz
        && !tz_sign.is_empty()
        // if tzSign is empty, we can be sure that the string literal contains timezone (such as 2010-10-10T10:10:10Z),
        // therefore we could safely skip this branch.
        && !no_absorb(&seps)
        && !(!tz_minute.is_empty() && tz_sep.is_empty())
    {
        // we can't absorb timezone if there is no separate between tzHour and tzMinute
        if !tz_hour.is_empty() {
            seps.push(tz_hour.to_vec());
        }
        if !tz_minute.is_empty() {
            seps.push(tz_minute.to_vec());
        }
        has_tz = false;
    }
    let wrong =
        |s: &'static str| ErrWrongValue.gen_with_stack_by_args(vec![Arg::from(s), go_str(str)]);
    match seps.len() {
        0 => return Err(wrong(DateTimeStr)),
        1 => {
            let s0 = seps[0].clone();
            let l = s0.len();
            // Values specified as numbers
            if is_float {
                let num_of_time = match str_to_int_digits(&s0) {
                    Ok(v) => v,
                    Err(_) => return Err(wrong(DateTimeStr)),
                };

                let (date_time, e) = parse_datetime_from_num(ctx, num_of_time);
                if e.is_err() {
                    return Err(wrong(DateTimeStr));
                }

                (year, month, day, hour, minute, second) = (
                    date_time.year(),
                    date_time.month(),
                    date_time.day(),
                    date_time.hour(),
                    date_time.minute(),
                    date_time.second(),
                );

                // case: 0.XXX or like "20170118.999"
                if s0.as_slice() == b"0" || (9..=14).contains(&l) {
                    hhmmss = true;
                }
            } else {
                // Values specified as strings
                match l {
                    14 => {
                        // No delimiter.
                        // YYYYMMDDHHMMSS
                        err = sscanf_digits(
                            &s0,
                            &[4, 2, 2, 2, 2, 2],
                            false,
                            &mut [
                                &mut year,
                                &mut month,
                                &mut day,
                                &mut hour,
                                &mut minute,
                                &mut second,
                            ],
                        );
                        hhmmss = true;
                    }
                    12 => {
                        // YYMMDDHHMMSS
                        err = sscanf_digits(
                            &s0,
                            &[2, 2, 2, 2, 2, 2],
                            false,
                            &mut [
                                &mut year,
                                &mut month,
                                &mut day,
                                &mut hour,
                                &mut minute,
                                &mut second,
                            ],
                        );
                        year = adjust_year(year);
                        hhmmss = true;
                    }
                    11 => {
                        // YYMMDDHHMMS
                        err = sscanf_digits(
                            &s0,
                            &[2, 2, 2, 2, 2, 1],
                            false,
                            &mut [
                                &mut year,
                                &mut month,
                                &mut day,
                                &mut hour,
                                &mut minute,
                                &mut second,
                            ],
                        );
                        year = adjust_year(year);
                        hhmmss = true;
                    }
                    10 => {
                        // YYMMDDHHMM
                        err = sscanf_digits(
                            &s0,
                            &[2, 2, 2, 2, 2],
                            false,
                            &mut [&mut year, &mut month, &mut day, &mut hour, &mut minute],
                        );
                        year = adjust_year(year);
                    }
                    9 => {
                        // YYMMDDHHM
                        err = sscanf_digits(
                            &s0,
                            &[2, 2, 2, 2, 1],
                            false,
                            &mut [&mut year, &mut month, &mut day, &mut hour, &mut minute],
                        );
                        year = adjust_year(year);
                    }
                    8 => {
                        // YYYYMMDD
                        err = sscanf_digits(
                            &s0,
                            &[4, 2, 2],
                            false,
                            &mut [&mut year, &mut month, &mut day],
                        );
                    }
                    7 => {
                        // YYMMDDH
                        err = sscanf_digits(
                            &s0,
                            &[2, 2, 2, 1],
                            false,
                            &mut [&mut year, &mut month, &mut day, &mut hour],
                        );
                        year = adjust_year(year);
                    }
                    6 | 5 => {
                        // YYMMDD && YYMMD
                        err = sscanf_digits(
                            &s0,
                            &[2, 2, 2],
                            false,
                            &mut [&mut year, &mut month, &mut day],
                        );
                        year = adjust_year(year);
                    }
                    _ => {
                        return Err(wrong(TimeStr));
                    }
                }
                if l == 5 || l == 6 || l == 8 {
                    // YYMMDD or YYYYMMDD
                    // We must handle float => string => datetime, the difference is that fractional
                    // part of float type is discarded directly, while fractional part of string type
                    // is parsed to HH:MM:SS.
                    // '20170118.123423' => 2017-01-18 12:34:23.234
                    match frac_str.len() {
                        0 => {}
                        1 | 2 => {
                            err = sscanf_digits(frac_str, &[2], true, &mut [&mut hour]);
                        }
                        3 | 4 => {
                            err = sscanf_digits(
                                frac_str,
                                &[2, 2],
                                true,
                                &mut [&mut hour, &mut minute],
                            );
                        }
                        _ => {
                            err = sscanf_digits(
                                frac_str,
                                &[2, 2, 2],
                                true,
                                &mut [&mut hour, &mut minute, &mut second],
                            );
                        }
                    }
                    truncated_or_incorrect = err.is_err();
                    // 20170118.123423 => 2017-01-18 00:00:00
                }
                if l == 9 || l == 10 {
                    if frac_str.is_empty() {
                        second = 0;
                    } else {
                        err = sscanf_digits(frac_str, &[2], true, &mut [&mut second]);
                    }
                    truncated_or_incorrect = err.is_err();
                }
                if truncated_or_incorrect {
                    ctx.append_warning(
                        ErrTruncatedWrongVal
                            .fast_gen_by_args(vec![Arg::from("datetime"), go_str(str)]),
                    );
                    err = Ok(());
                }
            }
        }
        2 => {
            return Err(ErrWrongValue.fast_gen_by_args(vec![Arg::from(DateTimeStr), go_str(str)]));
        }
        3 => {
            // YYYY-MM-DD
            err = scan_time_args(&seps, &mut [&mut year, &mut month, &mut day]);
        }
        4 => {
            // YYYY-MM-DD HH
            err = scan_time_args(&seps, &mut [&mut year, &mut month, &mut day, &mut hour]);
        }
        5 => {
            // YYYY-MM-DD HH-MM
            err = scan_time_args(
                &seps,
                &mut [&mut year, &mut month, &mut day, &mut hour, &mut minute],
            );
        }
        6 => {
            // We don't have fractional seconds part.
            // YYYY-MM-DD HH-MM-SS
            err = scan_time_args(
                &seps,
                &mut [
                    &mut year,
                    &mut month,
                    &mut day,
                    &mut hour,
                    &mut minute,
                    &mut second,
                ],
            );
            hhmmss = true;
        }
        _ => {
            // For case like `2020-05-28 23:59:59 00:00:00`, the seps should be > 6, the reluctant parts should be truncated.
            seps.truncate(6);
            // YYYY-MM-DD HH-MM-SS
            ctx.append_warning(
                ErrTruncatedWrongVal.fast_gen_by_args(vec![Arg::from("datetime"), go_str(str)]),
            );
            err = scan_time_args(
                &seps,
                &mut [
                    &mut year,
                    &mut month,
                    &mut day,
                    &mut hour,
                    &mut minute,
                    &mut second,
                ],
            );
            hhmmss = true;
        }
    }
    if let Err(e) = err {
        if e.error() == b"EOF" {
            return Err(wrong(DateTimeStr));
        }
        return Err(e);
    }

    // If str is sepereated by delimiters, the first one is year, and if the year is 1/2 digit,
    // we should adjust it.
    // TODO: adjust year is very complex, now we only consider the simplest way.
    if seps[0].len() <= 2
        && !is_float
        && !(year == 0
            && month == 0
            && day == 0
            && hour == 0
            && minute == 0
            && second == 0
            && frac_str.is_empty())
    {
        year = adjust_year(year);
        // Skip a special case "00-00-00".
    }

    let mut microsecond: isize = 0;
    let mut overflow = false;
    if hhmmss {
        // If input string is "20170118.999", without hhmmss, fsp is meaningless.
        // TODO: this case is not only meaningless, but erroneous, please confirm.
        (microsecond, overflow) = parse_frac(frac_str, fsp)?;
    }

    let (mut tmp, ok) = from_date_checked(year, month, day, hour, minute, second, microsecond);
    if !ok {
        return Err(wrong(DateTimeStr));
    }
    if overflow {
        // Convert to Go time and add 1 second, to handle input like 2017-01-05 08:40:59.575601
        let (t1, e) = tmp.go_time(&ctx.location());
        e?;
        tmp = from_go_time(&t1.add(gotime::SECOND));
    }
    if has_tz {
        // without hhmmss, timezone is also meaningless
        if !hhmmss {
            return Err(ErrWrongValue.gen_with_stack(DateTimeStr, vec![go_str(str)]));
        }
        if !tz_hour.is_empty() {
            delta_hour = ((tz_hour[0].wrapping_sub(b'0')).wrapping_mul(10)
                + tz_hour[1].wrapping_sub(b'0')) as isize;
        }
        if !tz_minute.is_empty() {
            delta_minute = ((tz_minute[0].wrapping_sub(b'0')).wrapping_mul(10)
                + tz_minute[1].wrapping_sub(b'0')) as isize;
        }
        // allowed delta range is [-14:00, 14:00], and we will intentionally reject -00:00
        if delta_hour > 14
            || delta_minute > 59
            || (delta_hour == 14 && delta_minute != 0)
            || (tz_sign == b"-" && delta_hour == 0 && delta_minute == 0)
        {
            return Err(wrong(DateTimeStr));
        }
        // by default, if the temporal string literal does not contain timezone information, it will be in the timezone
        // specified by the time_zone system variable. However, if the timezone is specified in the string literal, we
        // will use the specified timezone to interpret the string literal and convert it into the system timezone.
        let mut offset = delta_hour * 60 * 60 + delta_minute * 60;
        if tz_sign == b"-" {
            offset = -offset;
        }
        let name = format!(
            "UTC{}{}:{}",
            String::from_utf8_lossy(tz_sign),
            String::from_utf8_lossy(tz_hour),
            String::from_utf8_lossy(tz_minute)
        );
        let loc = gotime::fixed_zone(&name, offset as i32);
        let (t1, e) = tmp.go_time(&loc);
        e?;
        let t1 = t1.in_loc(&ctx.location());
        tmp = from_go_time(&t1);
    }

    let nt = new_time(tmp, mysql::TypeDatetime, fsp);

    Ok(nt)
}

/// `StrToInt(ctx, s, false)` for the digit-only strings parseDatetime passes (its only
/// call site here): Go's `getValidIntPrefix` keeps such a string whole, so the result is
/// `strconv.ParseInt(s, 10, 64)` and ErrOverflow when that fails.
// glue: boundary stub for StrToInt (block D01.4), specialised to [0-9]+ input
fn str_to_int_digits(s: &[u8]) -> Result<i64, Error> {
    let (v, err) = parse_int(s, 64);
    if err.is_err() {
        return Err(ErrOverflow.gen_with_stack_by_args(vec![Arg::from("BIGINT"), go_str(s)]));
    }
    Ok(v)
}

// go: scanTimeArgs L1
fn scan_time_args(seps: &[Vec<u8>], args: &mut [&mut isize]) -> Result<(), Error> {
    if seps.len() != args.len() {
        let joined: Vec<Arg> = seps.iter().map(|s| go_str(s.clone())).collect();
        let list = sprintf("%v", &[Arg::Stringer(sprint_list(&joined), "[]string")]);
        return Err(ErrWrongValue
            .gen_with_stack_by_args(vec![Arg::from(TimeStr), Arg::Stringer(list, "[]string")]));
    }

    for (s, dst) in seps.iter().zip(args.iter_mut()) {
        let (v, e) = atoi(s);
        **dst = v;
        e.map_err(|e| e.into_base())?;
    }
    Ok(())
}

/// `fmt.Sprint` of a `[]string`: `[a b c]`.
// glue: fmt of a []string argument (unreachable: scanTimeArgs is always called with matching counts)
fn sprint_list(items: &[Arg]) -> Vec<u8> {
    let mut out = vec![b'['];
    for (i, a) in items.iter().enumerate() {
        if i > 0 {
            out.push(b' ');
        }
        out.extend_from_slice(&sprintf("%s", std::slice::from_ref(a)));
    }
    out.push(b']');
    out
}

/// ParseYear parses a formatted string and returns a year number.
// go: ParseYear L1
pub fn parse_year(str: &[u8]) -> Result<i16, Error> {
    let (v, err) = parse_int(str, 16);
    err.map_err(|e| e.into_base())?;
    let mut y = v as i16;

    if str.len() == 2 || str.len() == 1 {
        y = adjust_year(y as isize) as i16;
    } else if str.len() != 4 {
        return Err(ErrInvalidYearFormat.clone());
    }

    if !(MinYear..=MaxYear).contains(&y) {
        return Err(ErrInvalidYearFormat.clone());
    }

    Ok(y)
}

/// adjustYear adjusts year according to y.
/// See https://dev.mysql.com/doc/refman/5.7/en/two-digit-years.html
// go: adjustYear L1
pub(crate) fn adjust_year(y: isize) -> isize {
    if (0..=69).contains(&y) {
        2000 + y
    } else if (70..=99).contains(&y) {
        1900 + y
    } else {
        y
    }
}

/// AdjustYear is used for adjusting year and checking its validation.
// go: AdjustYear L1 [M-ERRVAL]
pub fn adjust_year_checked(y: i64, adjust_zero: bool) -> (i64, Result<(), Error>) {
    if y == 0 && !adjust_zero {
        return (y, Ok(()));
    }
    let y = adjust_year(y as isize) as i64;
    if y < 0 {
        return (0, Err(ErrWarnDataOutOfRange.clone()));
    }
    if y < i64::from(MinYear) {
        return (i64::from(MinYear), Err(ErrWarnDataOutOfRange.clone()));
    }
    if y > i64::from(MaxYear) {
        return (i64::from(MaxYear), Err(ErrWarnDataOutOfRange.clone()));
    }

    (y, Ok(()))
}

/// NewDuration construct duration with time.
// go: NewDuration L1
pub fn new_duration(
    hour: isize,
    minute: isize,
    second: isize,
    microsecond: isize,
    fsp: isize,
) -> Duration {
    Duration {
        duration: (hour as i64)
            .wrapping_mul(gotime::HOUR)
            .wrapping_add((minute as i64).wrapping_mul(gotime::MINUTE))
            .wrapping_add((second as i64).wrapping_mul(gotime::SECOND))
            .wrapping_add((microsecond as i64).wrapping_mul(gotime::MICROSECOND)),
        fsp,
    }
}

/// Duration is the type for MySQL TIME type.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Duration {
    /// The embedded `gotime.Duration` (nanoseconds).
    pub duration: gotime::Duration,
    /// Fsp is short for Fractional Seconds Precision.
    /// See http://dev.mysql.com/doc/refman/5.7/en/fractional-seconds.html
    pub fsp: isize,
}

/// MaxMySQLDuration returns Duration with maximum mysql time.
// go: MaxMySQLDuration L1
pub fn max_mysql_duration(fsp: isize) -> Duration {
    new_duration(TimeMaxHour, TimeMaxMinute, TimeMaxSecond, 0, fsp)
}

/// AddInt64 adds int64 a and b if no overflow, otherwise returns error.
// Block D01.4, ported ahead (constitution §4.5: no out-edges).
// go: pkg/types/overflow.go AddInt64 L1
pub fn add_int64(a: i64, b: i64) -> Result<i64, Error> {
    if (a > 0 && b > 0 && i64::MAX - a < b) || (a < 0 && b < 0 && i64::MIN - a > b) {
        return Err(ErrOverflow
            .gen_with_stack_by_args(vec![Arg::from("BIGINT"), Arg::from(format!("({a}, {b})"))]));
    }

    Ok(a + b)
}

/// SubInt64 subtracts int64 a with b and returns int64 if no overflow error.
// Block D01.4, ported ahead (constitution §4.5: no out-edges).
// go: pkg/types/overflow.go SubInt64 L1
pub fn sub_int64(a: i64, b: i64) -> Result<i64, Error> {
    if (a > 0 && b < 0 && i64::MAX - a < b.wrapping_neg())
        || (a < 0 && b > 0 && i64::MIN - a > -b)
        || (a == 0 && b == i64::MIN)
    {
        return Err(ErrOverflow
            .gen_with_stack_by_args(vec![Arg::from("BIGINT"), Arg::from(format!("({a}, {b})"))]));
    }
    Ok(a - b)
}

impl Duration {
    /// Neg negative d, returns a duration value.
    // go: Duration.Neg L1
    #[allow(clippy::should_implement_trait)]
    pub fn neg(self) -> Duration {
        Duration {
            duration: self.duration.wrapping_neg(),
            fsp: self.fsp,
        }
    }

    /// Add adds d to d, returns a duration value.
    // go: Duration.Add L1
    #[allow(clippy::should_implement_trait)]
    pub fn add(self, v: Duration) -> Result<Duration, Error> {
        if v == Duration::default() {
            return Ok(self);
        }
        let dsum = add_int64(self.duration, v.duration)?;
        if self.fsp >= v.fsp {
            return Ok(Duration {
                duration: dsum,
                fsp: self.fsp,
            });
        }
        Ok(Duration {
            duration: dsum,
            fsp: v.fsp,
        })
    }

    /// Sub subtracts d to d, returns a duration value.
    // go: Duration.Sub L1
    #[allow(clippy::should_implement_trait)]
    pub fn sub(self, v: Duration) -> Result<Duration, Error> {
        if v == Duration::default() {
            return Ok(self);
        }
        let dsum = sub_int64(self.duration, v.duration)?;
        if self.fsp >= v.fsp {
            return Ok(Duration {
                duration: dsum,
                fsp: self.fsp,
            });
        }
        Ok(Duration {
            duration: dsum,
            fsp: v.fsp,
        })
    }

    /// DurationFormat returns a textual representation of the duration value formatted
    /// according to layout.
    /// See http://dev.mysql.com/doc/refman/5.7/en/date-and-time-functions.html#function_date-format
    // go: Duration.DurationFormat L1
    pub fn duration_format(self, layout: &[u8]) -> Result<Vec<u8>, Error> {
        let mut buf: Vec<u8> = Vec::with_capacity(layout.len() + 8);
        let mut in_pattern_match = false;
        let mut rest = layout;
        while !rest.is_empty() {
            let (r, n) = decode_rune(rest);
            let b = r.unwrap_or(char::REPLACEMENT_CHARACTER);
            rest = &rest[n..];
            if in_pattern_match {
                self.convert_date_format(b, &mut buf)?;
                in_pattern_match = false;
                continue;
            }

            // It's not in pattern match now.
            if b == '%' {
                in_pattern_match = true;
            } else {
                write_rune(&mut buf, b);
            }
        }
        Ok(buf)
    }

    // go: Duration.convertDateFormat L1
    fn convert_date_format(self, b: char, buf: &mut Vec<u8>) -> Result<(), Error> {
        match b {
            'H' => buf.extend_from_slice(&format_int_width_n(self.hour(), 2)),
            'k' => buf.extend_from_slice(self.hour().to_string().as_bytes()),
            'h' | 'I' => {
                let t = self.hour();
                if t % 12 == 0 {
                    buf.extend_from_slice(b"12");
                } else {
                    buf.extend_from_slice(&format_int_width_n(t % 12, 2));
                }
            }
            'l' => {
                let t = self.hour();
                if t % 12 == 0 {
                    buf.extend_from_slice(b"12");
                } else {
                    buf.extend_from_slice((t % 12).to_string().as_bytes());
                }
            }
            'i' => buf.extend_from_slice(&format_int_width_n(self.minute(), 2)),
            'p' => {
                let hour = self.hour();
                if hour / 12 % 2 == 0 {
                    buf.extend_from_slice(b"AM");
                } else {
                    buf.extend_from_slice(b"PM");
                }
            }
            'r' => {
                let h = self.hour() % 24;
                let (mi, s) = (self.minute(), self.second());
                let out = if h == 0 {
                    format!("{:02}:{:02}:{:02} AM", 12, mi, s)
                } else if h == 12 {
                    format!("{:02}:{:02}:{:02} PM", 12, mi, s)
                } else if h < 12 {
                    format!("{:02}:{:02}:{:02} AM", h, mi, s)
                } else {
                    format!("{:02}:{:02}:{:02} PM", h - 12, mi, s)
                };
                buf.extend_from_slice(out.as_bytes());
            }
            'T' => buf.extend_from_slice(
                format!(
                    "{:02}:{:02}:{:02}",
                    self.hour(),
                    self.minute(),
                    self.second()
                )
                .as_bytes(),
            ),
            'S' | 's' => buf.extend_from_slice(&format_int_width_n(self.second(), 2)),
            'f' => buf.extend_from_slice(format!("{:06}", self.micro_second()).as_bytes()),
            _ => write_rune(buf, b),
        }

        Ok(())
    }

    /// String returns the time formatted using default TimeFormat and fsp.
    // go: Duration.String L1 [M-NAME]
    pub fn string(self) -> Vec<u8> {
        let mut buf: Vec<u8> = Vec::with_capacity(16);

        let (sign, hours, minutes, seconds, fraction) = split_duration(self.duration);
        if sign < 0 {
            buf.push(b'-');
        }

        buf.extend_from_slice(format!("{hours:02}:{minutes:02}:{seconds:02}").as_bytes());
        if self.fsp > 0 {
            buf.push(b'.');
            buf.extend_from_slice(&self.format_frac(fraction));
        }

        buf
    }

    // go: Duration.formatFrac L1
    fn format_frac(self, frac: isize) -> Vec<u8> {
        let s = format!("{frac:06}");
        s.as_bytes()[0..self.fsp as usize].to_vec()
    }

    /// ToNumber changes duration to number format.
    /// e.g,
    /// 10:10:10 -> 101010
    // go: Duration.ToNumber L1
    pub fn to_number(self) -> MyDecimal {
        let (sign, hours, minutes, seconds, fraction) = split_duration(self.duration);
        let sign_str = if sign < 0 { "-" } else { "" };

        let s = if self.fsp == 0 {
            format!("{sign_str}{hours:02}{minutes:02}{seconds:02}").into_bytes()
        } else {
            let mut s = format!("{sign_str}{hours:02}{minutes:02}{seconds:02}.").into_bytes();
            s.extend_from_slice(&self.format_frac(fraction));
            s
        };

        // We skip checking error here because time formatted string can be parsed certainly.
        let mut dec = MyDecimal::default();
        let err = dec.from_string(&s);
        terror::log(err.err().as_ref());
        dec
    }

    /// ConvertToTime converts duration to Time.
    /// Tp is TypeDatetime, TypeTimestamp and TypeDate.
    // go: Duration.ConvertToTime L1 [M-ERRVAL]
    pub fn convert_to_time(self, ctx: &Context, tp: u8) -> (Time, Result<(), Error>) {
        self.convert_to_time_with_timestamp(ctx, tp, &gotime::now())
    }

    /// ConvertToTimeWithTimestamp converts duration to Time by system timestamp.
    /// Tp is TypeDatetime, TypeTimestamp and TypeDate.
    // go: Duration.ConvertToTimeWithTimestamp L1 [M-ERRVAL]
    pub fn convert_to_time_with_timestamp(
        self,
        ctx: &Context,
        tp: u8,
        ts: &gotime::Time,
    ) -> (Time, Result<(), Error>) {
        let (year, month, day) = ts.in_loc(&ctx.location()).date();
        let mut date_part = from_date(year as isize, month as isize, day as isize, 0, 0, 0, 0);
        mix_date_and_duration(&mut date_part, self);

        let t = new_time(date_part, mysql::TypeDatetime, self.fsp);
        t.convert(ctx, tp)
    }

    /// ConvertToYear converts duration to Year.
    // go: Duration.ConvertToYear L1 [M-ERRVAL]
    pub fn convert_to_year(self, ctx: &Context) -> (i64, Result<(), Error>) {
        self.convert_to_year_from_now(ctx, &gotime::now())
    }

    /// ConvertToYearFromNow converts duration to Year, with the `now` specified by the argument.
    // go: Duration.ConvertToYearFromNow L1 [M-ERRVAL]
    pub fn convert_to_year_from_now(
        self,
        ctx: &Context,
        now: &gotime::Time,
    ) -> (i64, Result<(), Error>) {
        if ctx.flags().cast_time_to_year_through_concat() {
            // this error will never happen, because we always give a valid FSP
            let (dur, _) = self.round_frac(DefaultFsp, &ctx.location());
            // the range of a duration will never exceed the range of `mysql.TypeLonglong`
            let (ival, _) = dur.to_number().to_int();

            return adjust_year_checked(ival, false);
        }

        let (year, month, day) = now.in_loc(&ctx.location()).date();
        let mut date_part = from_date(year as isize, month as isize, day as isize, 0, 0, 0, 0);
        mix_date_and_duration(&mut date_part, self);

        adjust_year_checked(date_part.year() as i64, false)
    }

    /// RoundFrac rounds fractional seconds precision with new fsp and returns a new one.
    /// We will use the “round half up” rule, e.g, >= 0.5 -> 1, < 0.5 -> 0,
    /// so 10:10:10.999999 round 0 -> 10:10:11
    /// and 10:10:10.000000 round 0 -> 10:10:10
    // go: Duration.RoundFrac L2 ([M-ERRVAL, M-F2I] loc cannot be nil, so Go's time.Local fallback is gone)
    pub fn round_frac(self, fsp: isize, loc: &Arc<Location>) -> (Duration, Result<(), Error>) {
        let tz = loc;

        let (fsp, err) = check_fsp(fsp);
        if let Err(e) = err {
            return (self, Err(e));
        }

        if fsp == self.fsp {
            return (self, Ok(()));
        }

        let n = gotime::date(0, 0, 0, 0, 0, 0, 0, tz);
        // [M-F2I]
        let d = tidb_base::gostd::conv::f64_to_i64(crate::fsp::pow10(9 - fsp));
        let nd = n
            .add(self.duration)
            .round(d.wrapping_mul(gotime::NANOSECOND))
            .sub(&n);
        (Duration { duration: nd, fsp }, Ok(()))
    }

    /// Compare returns an integer comparing the Duration instant t to o.
    /// If d is after o, returns 1, equal o, returns 0, before o, returns -1.
    // go: Duration.Compare L1
    pub fn compare(self, o: Duration) -> isize {
        if self.duration > o.duration {
            1
        } else if self.duration == o.duration {
            0
        } else {
            -1
        }
    }

    /// CompareString is like Compare,
    /// but parses str to Duration then compares.
    // go: Duration.CompareString L1
    pub fn compare_string(self, ctx: &Context, str: &[u8]) -> Result<isize, Error> {
        // use MaxFsp to parse the string
        let (o, _, err) = parse_duration(ctx, str, MaxFsp);
        err?;

        Ok(self.compare(o))
    }

    /// Hour returns current hour.
    /// e.g, hour("11:11:11") -> 11
    // go: Duration.Hour L1
    pub fn hour(self) -> isize {
        let (_, hour, _, _, _) = split_duration(self.duration);
        hour
    }

    /// Minute returns current minute.
    /// e.g, hour("11:11:11") -> 11
    // go: Duration.Minute L1
    pub fn minute(self) -> isize {
        let (_, _, minute, _, _) = split_duration(self.duration);
        minute
    }

    /// Second returns current second.
    /// e.g, hour("11:11:11") -> 11
    // go: Duration.Second L1
    pub fn second(self) -> isize {
        let (_, _, _, second, _) = split_duration(self.duration);
        second
    }

    /// MicroSecond returns current microsecond.
    /// e.g, hour("11:11:11.11") -> 110000
    // go: Duration.MicroSecond L1
    pub fn micro_second(self) -> isize {
        let (_, _, _, _, frac) = split_duration(self.duration);
        frac
    }
}

// go: isNegativeDuration L1
fn is_negative_duration(str: &[u8]) -> (bool, &[u8]) {
    match parser::char(str, b'-') {
        Err(_) => (false, str),
        Ok(rest) => (true, rest),
    }
}

// go: matchColon L1
fn match_colon(str: &[u8]) -> Result<&[u8], ()> {
    let rest = parser::space0(str);
    let rest = parser::char(rest, b':')?;
    let rest = parser::space0(rest);
    Ok(rest)
}

// go: matchDayHHMMSS L1
fn match_day_hhmmss(str: &[u8]) -> Result<(isize, [isize; 3], &[u8]), ()> {
    let (day, rest) = parser::number(str)?;

    let rest = parser::space(rest, 1)?;

    let (hhmmss, rest) = match_hhmmss_delimited(rest, false)?;

    Ok((day, hhmmss, rest))
}

// go: matchHHMMSSDelimited L1
fn match_hhmmss_delimited(str: &[u8], require_colon: bool) -> Result<([isize; 3], &[u8]), ()> {
    let mut hhmmss = [0isize; 3];

    let (hour, mut rest) = parser::number(str)?;
    hhmmss[0] = hour;

    for (i, slot) in hhmmss.iter_mut().enumerate().skip(1) {
        let remain = match match_colon(rest) {
            Ok(r) => r,
            Err(()) => {
                if i == 1 && require_colon {
                    return Err(());
                }
                break;
            }
        };
        let (num, remain) = parser::number(remain)?;
        *slot = num;
        rest = remain;
    }

    Ok((hhmmss, rest))
}

// go: matchHHMMSSCompact L1
fn match_hhmmss_compact(str: &[u8]) -> Result<([isize; 3], &[u8]), ()> {
    let (num, rest) = parser::number(str)?;
    let hhmmss = [num / 10000, (num / 100) % 100, num % 100];
    Ok((hhmmss, rest))
}

// go: hhmmssAddOverflow L1
fn hhmmss_add_overflow(hms: &mut [isize], mut overflow: bool) {
    let modulo = [-1isize, 60, 60];
    let mut i: isize = 2;
    while i >= 0 && overflow {
        let iu = i as usize;
        hms[iu] += 1;
        if hms[iu] == modulo[iu] {
            overflow = true;
            hms[iu] = 0;
        } else {
            overflow = false;
        }
        i -= 1;
    }
}

// go: checkHHMMSS L1
fn check_hhmmss(hms: [isize; 3]) -> bool {
    let (m, s) = (hms[1], hms[2]);
    m < 60 && s < 60
}

/// matchFrac returns overflow, fraction, rest, error
// go: matchFrac L1
fn match_frac(str: &[u8], fsp: isize) -> (bool, isize, &[u8], Result<(), Error>) {
    let rest = match parser::char(str, b'.') {
        Err(()) => return (false, 0, str, Ok(())),
        Ok(r) => r,
    };

    let (digits, rest) = match parser::digit(rest, 0) {
        Err(()) => return (false, 0, str, Err(parser::err_pattern_not_match())),
        Ok(v) => v,
    };

    match parse_frac(digits, fsp) {
        Err(e) => (false, 0, str, Err(e)),
        Ok((frac, overflow)) => (overflow, frac, rest, Ok(())),
    }
}

// go: matchDuration L1 [M-ERRVAL]
fn match_duration(str: &[u8], fsp: isize) -> (Duration, bool, Result<(), Error>) {
    let truncated =
        || ErrTruncatedWrongVal.gen_with_stack_by_args(vec![Arg::from("time"), go_str(str)]);
    let (fsp, err) = check_fsp(fsp);
    if let Err(e) = err {
        return (ZeroDuration, true, Err(e));
    }

    if str.is_empty() {
        return (ZeroDuration, true, Err(truncated()));
    }

    let (negative, rest) = is_negative_duration(str);
    let mut rest = parser::space0(rest);
    let chars_len = rest.len();

    let mut hhmmss: [isize; 3];

    if let Ok((day, mut hms, remain)) = match_day_hhmmss(rest) {
        hms[0] += 24 * day;
        (rest, hhmmss) = (remain, hms);
    } else if let Ok((hms, remain)) = match_hhmmss_delimited(rest, true) {
        (rest, hhmmss) = (remain, hms);
    } else if let Ok((hms, remain)) = match_hhmmss_compact(rest) {
        (rest, hhmmss) = (remain, hms);
    } else {
        return (ZeroDuration, true, Err(truncated()));
    }

    let rest = parser::space0(rest);
    let (overflow, mut frac, rest, err) = match_frac(rest, fsp);
    if err.is_err() || (!rest.is_empty() && chars_len >= 12) {
        return (ZeroDuration, true, Err(truncated()));
    }

    if overflow {
        hhmmss_add_overflow(&mut hhmmss, overflow);
        frac = 0;
    }

    if !check_hhmmss(hhmmss) {
        return (ZeroDuration, true, Err(truncated()));
    }

    if hhmmss[0] > TimeMaxHour {
        let t = if negative { MinTime } else { MaxTime };
        return (Duration { duration: t, fsp }, false, Err(truncated()));
    }

    let mut d = ((hhmmss[0] * 3600 + hhmmss[1] * 60 + hhmmss[2]) as i64)
        .wrapping_mul(gotime::SECOND)
        .wrapping_add((frac as i64).wrapping_mul(gotime::MICROSECOND));
    if negative {
        d = d.wrapping_neg();
    }
    let (d, err) = truncate_overflow_mysql_time(d);
    if err.is_ok() && !rest.is_empty() {
        return (Duration { duration: d, fsp }, false, Err(truncated()));
    }
    (Duration { duration: d, fsp }, false, err)
}

/// canFallbackToDateTime return true
///  1. the string is failed to be parsed by `matchDuration`
///  2. the string is start with a series of digits whose length match the full format of DateTime literal (12, 14)
///     or the string start with a date literal.
// go: canFallbackToDateTime L1
fn can_fallback_to_date_time(str: &[u8]) -> bool {
    let Ok((digits, rest)) = parser::digit(str, 1) else {
        return false;
    };
    if digits.len() == 12 || digits.len() == 14 {
        return true;
    }

    let Ok(rest) = parser::any_punct(rest) else {
        return false;
    };

    let Ok((_, rest)) = parser::digit(rest, 1) else {
        return false;
    };

    let Ok(rest) = parser::any_punct(rest) else {
        return false;
    };

    let Ok((_, rest)) = parser::digit(rest, 1) else {
        return false;
    };

    !rest.is_empty() && (rest[0] == b' ' || rest[0] == b'T')
}

/// ParseDuration parses the time form a formatted string with a fractional seconds part,
/// returns the duration type Time value and bool to indicate whether the result is null.
/// See http://dev.mysql.com/doc/refman/5.7/en/fractional-seconds.html
// go: ParseDuration L1 [M-ERRVAL]
pub fn parse_duration(
    ctx: &Context,
    str: &[u8],
    fsp: isize,
) -> (Duration, bool, Result<(), Error>) {
    let truncated =
        || ErrTruncatedWrongVal.gen_with_stack_by_args(vec![Arg::from("time"), go_str(str)]);
    let rest = trim_space_go(str);
    let (d, is_null, err) = match_duration(rest, fsp);
    if err.is_ok() {
        return (d, is_null, Ok(()));
    }
    if !can_fallback_to_date_time(rest) {
        return (d, is_null, Err(truncated()));
    }

    let (datetime, err) = parse_datetime_str(ctx, rest);
    if err.is_err() {
        return (ZeroDuration, true, Err(truncated()));
    }

    let d = match datetime.convert_to_duration() {
        Ok(d) => d,
        Err(_) => return (ZeroDuration, true, Err(truncated())),
    };

    let (d, err) = d.round_frac(fsp, &ctx.location());
    (d, false, err)
}

/// TruncateOverflowMySQLTime truncates d when it overflows, and returns ErrTruncatedWrongVal.
// go: TruncateOverflowMySQLTime L1 [M-ERRVAL]
pub fn truncate_overflow_mysql_time(d: gotime::Duration) -> (gotime::Duration, Result<(), Error>) {
    let darg = || Arg::Stringer(gotime::duration_string(d).into_bytes(), "time.Duration");
    if d > MaxTime {
        return (
            MaxTime,
            Err(ErrTruncatedWrongVal.gen_with_stack_by_args(vec![Arg::from("time"), darg()])),
        );
    } else if d < MinTime {
        return (
            MinTime,
            Err(ErrTruncatedWrongVal.gen_with_stack_by_args(vec![Arg::from("time"), darg()])),
        );
    }

    (d, Ok(()))
}

// go: splitDuration L1
pub(crate) fn split_duration(t: gotime::Duration) -> (isize, isize, isize, isize, isize) {
    let mut t = t;
    let mut sign = 1;
    if t < 0 {
        t = t.wrapping_neg();
        sign = -1;
    }

    let hours_duration = t / gotime::HOUR;
    t -= hours_duration * gotime::HOUR;
    let minutes_duration = t / gotime::MINUTE;
    t -= minutes_duration * gotime::MINUTE;
    let seconds_duration = t / gotime::SECOND;
    t -= seconds_duration * gotime::SECOND;
    let fraction_duration = t / gotime::MICROSECOND;

    (
        sign,
        hours_duration as isize,
        minutes_duration as isize,
        seconds_duration as isize,
        fraction_duration as isize,
    )
}

const maxDaysInMonth: [isize; 12] = [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

// go: getTime L1 [M-ERRVAL]
fn get_time(ctx: &Context, num: i64, origin_num: i64, tp: u8) -> (Time, Result<(), Error>) {
    let mut s1 = num / 1000000;
    let mut s2 = num - s1 * 1000000;

    let year = (s1 / 10000) as isize;
    s1 %= 10000;
    let month = (s1 / 100) as isize;
    let day = (s1 % 100) as isize;

    let hour = (s2 / 10000) as isize;
    s2 %= 10000;
    let minute = (s2 / 100) as isize;
    let second = (s2 % 100) as isize;

    let (ct, ok) = from_date_checked(year, month, day, hour, minute, second, 0);
    if !ok {
        let num_str = origin_num.to_string();
        return (
            *ZeroDatetime,
            Err(ErrWrongValue.gen_with_stack_by_args(vec![Arg::from(TimeStr), Arg::from(num_str)])),
        );
    }
    let t = new_time(ct, tp, DefaultFsp);
    let err = t.check(ctx);
    (t, err)
}

/// parseDateTimeFromNum parses date time from num.
/// See number_to_datetime function.
/// https://github.com/mysql/mysql-server/blob/5.7/sql-common/my_time.c
// go: parseDateTimeFromNum L1 [M-ERRVAL]
fn parse_date_time_from_num(ctx: &Context, num: i64) -> (Time, Result<(), Error>) {
    let mut t = *ZeroDate;
    // Check zero.
    if num == 0 {
        return (t, Ok(()));
    }
    let origin_num = num;
    let wrong = |n: i64| {
        ErrWrongValue.gen_with_stack_by_args(vec![Arg::from(TimeStr), Arg::from(n.to_string())])
    };

    // Check datetime type.
    if num >= 10000101000000 {
        t.set_type(mysql::TypeDatetime);
        return get_time(ctx, num, origin_num, t.tp());
    }

    // Check MMDD.
    if num < 101 {
        return (t, Err(wrong(num)));
    }

    // Adjust year
    // YYMMDD, year: 2000-2069
    if num <= (70 - 1) * 10000 + 1231 {
        let num = (num + 20000000) * 1000000;
        return get_time(ctx, num, origin_num, t.tp());
    }

    // Check YYMMDD.
    if num < 70 * 10000 + 101 {
        return (t, Err(wrong(num)));
    }

    // Adjust year
    // YYMMDD, year: 1970-1999
    if num <= 991231 {
        let num = (num + 19000000) * 1000000;
        return get_time(ctx, num, origin_num, t.tp());
    }

    // Adjust hour/min/second.
    if num <= 99991231 {
        let num = num * 1000000;
        return get_time(ctx, num, origin_num, t.tp());
    }

    // Check MMDDHHMMSS.
    if num < 101000000 {
        return (t, Err(wrong(num)));
    }

    // Set TypeDatetime type.
    t.set_type(mysql::TypeDatetime);

    // Adjust year
    // YYMMDDHHMMSS, 2000-2069
    if num <= 69 * 10000000000 + 1231235959 {
        let num = num + 20000000000000;
        return get_time(ctx, num, origin_num, t.tp());
    }

    // Check YYYYMMDDHHMMSS.
    if num < 70 * 10000000000 + 101000000 {
        return (t, Err(wrong(num)));
    }

    // Adjust year
    // YYMMDDHHMMSS, 1970-1999
    if num <= 991231235959 {
        let num = num + 19000000000000;
        return get_time(ctx, num, origin_num, t.tp());
    }

    get_time(ctx, num, origin_num, t.tp())
}

/// ParseTime parses a formatted string with type tp and specific fsp.
/// Type is TypeDatetime, TypeTimestamp and TypeDate.
/// Fsp is in range [0, 6].
/// MySQL supports many valid datetime format, but still has some limitation.
/// If delimiter exists, the date part and time part is separated by a space or T,
/// other punctuation character can be used as the delimiter between date parts or time parts.
/// If no delimiter, the format must be YYYYMMDDHHMMSS or YYMMDDHHMMSS
/// If we have fractional seconds part, we must use decimal points as the delimiter.
/// The valid datetime range is from '1000-01-01 00:00:00.000000' to '9999-12-31 23:59:59.999999'.
/// The valid timestamp range is from '1970-01-01 00:00:01.000000' to '2038-01-19 03:14:07.999999'.
/// The valid date range is from '1000-01-01' to '9999-12-31'
/// explicitTz is used to handle a data race of timeZone, refer to https://github.com/pingcap/tidb/issues/40710. It only works for timestamp now, be careful to use it!
// go: ParseTime L1 [M-ERRVAL]
pub fn parse_time(ctx: &Context, str: &[u8], tp: u8, fsp: isize) -> (Time, Result<(), Error>) {
    parse_time_impl(ctx, str, tp, fsp, false)
}

/// ParseTimeFromFloatString is similar to ParseTime, except that it's used to parse a float converted string.
// go: ParseTimeFromFloatString L1 [M-ERRVAL]
pub fn parse_time_from_float_string(
    ctx: &Context,
    str: &[u8],
    tp: u8,
    fsp: isize,
) -> (Time, Result<(), Error>) {
    // MySQL compatibility: 0.0 should not be converted to null, see #11203
    if str.len() >= 3 && &str[..3] == b"0.0" {
        return (new_time(ZeroCoreTime, tp, DefaultFsp), Ok(()));
    }
    parse_time_impl(ctx, str, tp, fsp, true)
}

// go: parseTime L1 [M-ERRVAL]
fn parse_time_impl(
    ctx: &Context,
    str: &[u8],
    tp: u8,
    fsp: isize,
    is_float: bool,
) -> (Time, Result<(), Error>) {
    let (fsp, err) = check_fsp(fsp);
    if let Err(e) = err {
        return (new_time(ZeroCoreTime, tp, DefaultFsp), Err(e));
    }

    let mut t = match parse_datetime(ctx, str, fsp, is_float) {
        Ok(t) => t,
        Err(e) => return (new_time(ZeroCoreTime, tp, DefaultFsp), Err(e)),
    };

    t.set_type(tp);
    if let Err(err) = t.check(ctx) {
        if tp == mysql::TypeTimestamp && !t.is_zero() {
            let (t_adjusted, err_adjusted) =
                adjust_timestamp_err_for_dst(&ctx.location(), str, tp, t, err.clone());
            if let Err(e) = &err_adjusted
                && ErrTimestampInDSTTransition.equal(e)
            {
                return (t_adjusted, err_adjusted);
            }
        }
        return (new_time(ZeroCoreTime, tp, DefaultFsp), Err(err));
    }
    (t, Ok(()))
}

// go: adjustTimestampErrForDST L1 [M-ERRVAL]
fn adjust_timestamp_err_for_dst(
    loc: &Arc<Location>,
    str: &[u8],
    tp: u8,
    mut t: Time,
    err: Error,
) -> (Time, Result<(), Error>) {
    if tp != mysql::TypeTimestamp || t.is_zero() {
        return (t, Err(err));
    }
    let (mut min_ts, mut max_ts) = (*MinTimestamp, *MaxTimestamp);
    let min_err = min_ts.convert_time_zone(&gotime::utc(), loc);
    let max_err = max_ts.convert_time_zone(&gotime::utc(), loc);
    if min_err.is_ok() && max_err.is_ok() && t.compare(&min_ts) > 0 && t.compare(&max_ts) < 0 {
        // Handle the case when the timestamp given is in the DST transition
        let (t_adjusted, err2) = t.adjusted_go_time(loc);
        if err2.is_ok() {
            t.set_core_time(from_go_time(&t_adjusted));
            return (
                t,
                Err(ErrTimestampInDSTTransition
                    .gen_with_stack_by_args(vec![go_str(str), Arg::from(loc.string().to_owned())])),
            );
        }
    }
    (t, Err(err))
}

/// ParseDatetime is a helper function wrapping ParseTime with datetime type and default fsp.
// go: ParseDatetime L1 [M-ERRVAL]
pub fn parse_datetime_str(ctx: &Context, str: &[u8]) -> (Time, Result<(), Error>) {
    parse_time(ctx, str, mysql::TypeDatetime, get_fsp(str))
}

/// ParseTimestamp is a helper function wrapping ParseTime with timestamp type and default fsp.
// go: ParseTimestamp L1 [M-ERRVAL]
pub fn parse_timestamp(ctx: &Context, str: &[u8]) -> (Time, Result<(), Error>) {
    parse_time(ctx, str, mysql::TypeTimestamp, get_fsp(str))
}

/// ParseDate is a helper function wrapping ParseTime with date type.
// go: ParseDate L1 [M-ERRVAL]
pub fn parse_date(ctx: &Context, str: &[u8]) -> (Time, Result<(), Error>) {
    // date has no fractional seconds precision
    parse_time(ctx, str, mysql::TypeDate, MinFsp)
}

/// ParseTimeFromYear parse a `YYYY` formed year to corresponded Datetime type.
/// Note: the invoker must promise the `year` is in the range [MinYear, MaxYear].
// go: ParseTimeFromYear L1
pub fn parse_time_from_year(year: i64) -> Result<Time, Error> {
    if year == 0 {
        return Ok(new_time(ZeroCoreTime, mysql::TypeDate, DefaultFsp));
    }

    let dt = from_date(year as isize, 0, 0, 0, 0, 0, 0);
    Ok(new_time(dt, mysql::TypeDatetime, DefaultFsp))
}

/// ParseTimeFromNum parses a formatted int64,
/// returns the value which type is tp.
// go: ParseTimeFromNum L1 [M-ERRVAL]
pub fn parse_time_from_num(
    ctx: &Context,
    num: i64,
    tp: u8,
    fsp: isize,
) -> (Time, Result<(), Error>) {
    // MySQL compatibility: 0 should not be converted to null, see #11203
    if num == 0 {
        let zt = new_time(ZeroCoreTime, tp, DefaultFsp);
        if !ctx.flags().ignore_zero_date_err() {
            let name = match tp {
                mysql::TypeTimestamp => Some(TimestampStr),
                mysql::TypeDate => Some(DateStr),
                mysql::TypeDatetime => Some(DateTimeStr),
                _ => None,
            };
            if let Some(name) = name {
                return (
                    zt,
                    Err(ErrTruncatedWrongVal
                        .gen_with_stack_by_args(vec![Arg::from(name), Arg::from("0")])),
                );
            }
        }
        return (zt, Ok(()));
    }
    let (fsp, err) = check_fsp(fsp);
    if let Err(e) = err {
        return (new_time(ZeroCoreTime, tp, DefaultFsp), Err(e));
    }

    let (mut t, err) = parse_date_time_from_num(ctx, num);
    if let Err(e) = err {
        return (new_time(ZeroCoreTime, tp, DefaultFsp), Err(e));
    }

    t.set_type(tp);
    t.set_fsp(fsp);
    if let Err(e) = t.check(ctx) {
        return (new_time(ZeroCoreTime, tp, DefaultFsp), Err(e));
    }
    (t, Ok(()))
}

/// ParseDatetimeFromNum is a helper function wrapping ParseTimeFromNum with datetime type and default fsp.
// go: ParseDatetimeFromNum L1 [M-ERRVAL]
pub fn parse_datetime_from_num(ctx: &Context, num: i64) -> (Time, Result<(), Error>) {
    parse_time_from_num(ctx, num, mysql::TypeDatetime, DefaultFsp)
}

/// ParseTimestampFromNum is a helper function wrapping ParseTimeFromNum with timestamp type and default fsp.
// go: ParseTimestampFromNum L1 [M-ERRVAL]
pub fn parse_timestamp_from_num(ctx: &Context, num: i64) -> (Time, Result<(), Error>) {
    parse_time_from_num(ctx, num, mysql::TypeTimestamp, DefaultFsp)
}

/// ParseDateFromNum is a helper function wrapping ParseTimeFromNum with date type.
// go: ParseDateFromNum L1 [M-ERRVAL]
pub fn parse_date_from_num(ctx: &Context, num: i64) -> (Time, Result<(), Error>) {
    // date has no fractional seconds precision
    parse_time_from_num(ctx, num, mysql::TypeDate, MinFsp)
}

/// TimeFromDays Converts a day number to a date.
// go: TimeFromDays L1
pub fn time_from_days(num: i64) -> Time {
    if num < 0 {
        return new_time(from_date(0, 0, 0, 0, 0, 0, 0), mysql::TypeDate, 0);
    }
    let (year, month, day) = get_date_from_daynr(num as usize);
    let (ct, ok) = from_date_checked(year as isize, month as isize, day as isize, 0, 0, 0, 0);
    if !ok {
        return new_time(from_date(0, 0, 0, 0, 0, 0, 0), mysql::TypeDate, 0);
    }
    new_time(ct, mysql::TypeDate, 0)
}

// go: checkDateType L1
fn check_date_type(
    t: CoreTime,
    allow_zero_in_date: bool,
    allow_invalid_date: bool,
) -> Result<(), Error> {
    let (year, month, day) = (t.year(), t.month(), t.day());
    if year == 0 && month == 0 && day == 0 {
        return Ok(());
    }

    if !allow_zero_in_date && (month == 0 || day == 0) {
        return Err(ErrWrongValue.gen_with_stack_by_args(vec![
            Arg::from(DateTimeStr),
            Arg::from(format!("{year:04}-{month:02}-{day:02}")),
        ]));
    }

    check_date_range(t)?;

    check_month_day(year, month, day, allow_invalid_date)?;

    Ok(())
}

// go: checkDateRange L1
fn check_date_range(t: CoreTime) -> Result<(), Error> {
    // Oddly enough, MySQL document says date range should larger than '1000-01-01',
    // but we can insert '0001-01-01' actually.
    if t.year() < 0 || t.month() < 0 || t.day() < 0 {
        return Err(ErrWrongValue.gen_with_stack_by_args(vec![Arg::from(TimeStr), t.arg()]));
    }
    if compare_time(t, MaxDatetime) > 0 {
        return Err(ErrWrongValue.gen_with_stack_by_args(vec![Arg::from(TimeStr), t.arg()]));
    }
    Ok(())
}

// go: checkMonthDay L1
fn check_month_day(
    year: isize,
    month: isize,
    day: isize,
    allow_invalid_date: bool,
) -> Result<(), Error> {
    let wrong = || {
        ErrWrongValue.gen_with_stack_by_args(vec![
            Arg::from(DateTimeStr),
            Arg::from(format!("{year}-{month}-{day}")),
        ])
    };
    if !(0..=12).contains(&month) {
        return Err(wrong());
    }

    let mut max_day = 31;
    if !allow_invalid_date {
        if month > 0 {
            max_day = maxDaysInMonth[(month - 1) as usize];
        }
        if month == 2 && !crate::core_time::is_leap_year(year as u16) {
            max_day = 28;
        }
    }

    if day < 0 || day > max_day {
        return Err(wrong());
    }
    Ok(())
}

// go: checkTimestampType L1
fn check_timestamp_type(t: CoreTime, tz: &Arc<Location>) -> Result<(), Error> {
    if compare_time(t, ZeroCoreTime) == 0 {
        return Ok(());
    }

    let check_time;
    if !gotime::is_utc(tz) {
        let mut convert_time = new_time(t, mysql::TypeTimestamp, DefaultFsp);
        if let Err(err) = convert_time.convert_time_zone(tz, &bound_timezone()) {
            let s = t.string().into_bytes();
            let (_, err2) =
                adjust_timestamp_err_for_dst(tz, &s, mysql::TypeTimestamp, Time::from_raw(t), err);
            return err2;
        }
        check_time = convert_time.core;
    } else {
        check_time = t;
    }
    if compare_time(check_time, MaxTimestamp.core) > 0
        || compare_time(check_time, MinTimestamp.core) < 0
    {
        return Err(ErrWrongValue.gen_with_stack_by_args(vec![Arg::from(TimeStr), t.arg()]));
    }

    let (_, err) = t.go_time(tz);
    err?;

    Ok(())
}

// go: checkDatetimeType L1
fn check_datetime_type(
    t: CoreTime,
    allow_zero_in_date: bool,
    allow_invalid_date: bool,
) -> Result<(), Error> {
    check_date_type(t, allow_zero_in_date, allow_invalid_date)?;

    let (hour, minute, second) = (t.hour(), t.minute(), t.second());
    if !(0..24).contains(&hour) {
        return Err(ErrWrongValue
            .gen_with_stack_by_args(vec![Arg::from(TimeStr), Arg::from(hour.to_string())]));
    }
    if !(0..60).contains(&minute) {
        return Err(ErrWrongValue
            .gen_with_stack_by_args(vec![Arg::from(TimeStr), Arg::from(minute.to_string())]));
    }
    if !(0..60).contains(&second) {
        return Err(ErrWrongValue
            .gen_with_stack_by_args(vec![Arg::from(TimeStr), Arg::from(second.to_string())]));
    }

    Ok(())
}

/// `strings.ToUpper` of a Go string (bytes): `unicode.ToUpper` on each rune, invalid
/// UTF-8 bytes becoming U+FFFD.
// go-std: strings.ToUpper L1
fn to_upper_go(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut has_lower = false;
    let mut is_ascii = true;
    for &c in s {
        if c >= 0x80 {
            is_ascii = false;
            break;
        }
        has_lower = has_lower || c.is_ascii_lowercase();
    }
    if is_ascii {
        if !has_lower {
            return s.to_vec();
        }
        return s.to_ascii_uppercase();
    }
    let mut rest = s;
    while !rest.is_empty() {
        let (r, n) = decode_rune(rest);
        let r = r.unwrap_or(char::REPLACEMENT_CHARACTER);
        write_rune(&mut out, tidb_base::gostd::to_upper_rune(r));
        rest = &rest[n..];
    }
    out
}

/// ExtractDatetimeNum extracts time value number from datetime unit and format.
// go: ExtractDatetimeNum L1
pub fn extract_datetime_num(t: &Time, unit: &[u8]) -> Result<i64, Error> {
    // TODO: Consider time_zone variable.
    match to_upper_go(unit).as_slice() {
        b"DAY" => Ok(t.day() as i64),
        b"WEEK" => {
            let week = t.week(0);
            Ok(week as i64)
        }
        b"MONTH" => Ok(t.month() as i64),
        b"QUARTER" => {
            let m = t.month() as i64;
            // 1 - 3 -> 1
            // 4 - 6 -> 2
            // 7 - 9 -> 3
            // 10 - 12 -> 4
            Ok((m + 2) / 3)
        }
        b"YEAR" => Ok(t.year() as i64),
        b"DAY_MICROSECOND" => {
            let (h, m, s) = t.clock();
            let d = t.day();
            Ok(((d * 1000000 + h * 10000 + m * 100 + s) as i64) * 1000000 + t.microsecond() as i64)
        }
        b"DAY_SECOND" => {
            let (h, m, s) = t.clock();
            let d = t.day();
            Ok(d as i64 * 1000000 + h as i64 * 10000 + m as i64 * 100 + s as i64)
        }
        b"DAY_MINUTE" => {
            let (h, m, _) = t.clock();
            let d = t.day();
            Ok(d as i64 * 10000 + h as i64 * 100 + m as i64)
        }
        b"DAY_HOUR" => {
            let (h, _, _) = t.clock();
            let d = t.day();
            Ok(d as i64 * 100 + h as i64)
        }
        b"YEAR_MONTH" => {
            let (y, m) = (t.year(), t.month());
            Ok(y as i64 * 100 + m as i64)
        }
        _ => Err(tidb_base::errors::errorf(
            "invalid unit %s",
            &[go_str(unit)],
        )),
    }
}

/// ExtractDurationNum extracts duration value number from duration unit and format.
// go: ExtractDurationNum L1
pub fn extract_duration_num(d: &Duration, unit: &[u8]) -> Result<i64, Error> {
    let (h, m, s, us) = (
        d.hour() as i64,
        d.minute() as i64,
        d.second() as i64,
        d.micro_second() as i64,
    );
    let mut res = match to_upper_go(unit).as_slice() {
        b"MICROSECOND" => us,
        b"SECOND" => s,
        b"MINUTE" => m,
        b"HOUR" => h,
        b"SECOND_MICROSECOND" => s * 1000000 + us,
        b"MINUTE_MICROSECOND" => m * 100000000 + s * 1000000 + us,
        b"MINUTE_SECOND" => m * 100 + s,
        b"HOUR_MICROSECOND" => h * 10000000000 + m * 100000000 + s * 1000000 + us,
        b"HOUR_SECOND" => h * 10000 + m * 100 + s,
        b"HOUR_MINUTE" => h * 100 + m,
        b"DAY_MICROSECOND" => (h * 10000 + m * 100 + s) * 1000000 + us,
        b"DAY_SECOND" => h * 10000 + m * 100 + s,
        b"DAY_MINUTE" => h * 100 + m,
        b"DAY_HOUR" => h,
        _ => {
            return Err(tidb_base::errors::errorf(
                "invalid unit %s",
                &[go_str(unit)],
            ));
        }
    };
    if d.duration < 0 {
        res = -res;
    }
    Ok(res)
}

/// The results of parseSingleTimeValue / parseTimeValue / ParseDurationValue:
/// years, months, days, nanoseconds and fsp (Go returns them together with the error).
pub type DurationValue = (i64, i64, i64, i64, isize);

/// `oneToSixDigitRegex.FindString(s)` (`^[0-9]{0,6}`): the leading up-to-6 digits.
// glue: the package regexp oneToSixDigitRegex (^[0-9]{0,6}) as a byte scan
fn one_to_six_digit_prefix(s: &[u8]) -> &[u8] {
    let n = s.iter().take(6).take_while(|c| c.is_ascii_digit()).count();
    &s[..n]
}

/// `numericRegex.FindAllString(s, -1)` (`[0-9]+`): the runs of ASCII digits.
// glue: the package regexp numericRegex ([0-9]+) as a byte scan
fn numeric_runs(s: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < s.len() {
        if s[i].is_ascii_digit() {
            let start = i;
            while i < s.len() && s[i].is_ascii_digit() {
                i += 1;
            }
            out.push(&s[start..i]);
        } else {
            i += 1;
        }
    }
    out
}

/// parseSingleTimeValue parse the format according the given unit. If we set strictCheck true, we'll check whether
/// the converted value not exceed the range of MySQL's TIME type.
/// The returned values are year, month, day, nanosecond and fsp.
// go: parseSingleTimeValue L1 [M-ERRVAL]
fn parse_single_time_value(
    unit: &[u8],
    format: &[u8],
    strict_check: bool,
) -> (DurationValue, Result<(), Error>) {
    let wrong =
        || ErrWrongValue.gen_with_stack_by_args(vec![Arg::from(DateTimeStr), go_str(format)]);
    let overflow = || ErrDatetimeFunctionOverflow.gen_with_stack_by_args(vec![Arg::from("time")]);
    // Format is a preformatted number, it format should be A[.[B]].
    let decimal_point_pos = format
        .iter()
        .position(|&c| c == b'.')
        .unwrap_or(format.len());
    let mut sign: i64 = 1;
    if !format.is_empty() && format[0] == b'-' {
        sign = -1;
    }

    // We should also continue even if an error occurs here
    // because the called may ignore the error and use the return value.
    let (mut iv, e) = parse_int(&format[0..decimal_point_pos], 64);
    let mut err: Result<(), Error> = Ok(());
    if e.is_err() {
        err = Err(wrong());
    }
    let mut riv = iv; // Rounded integer value

    let mut decimal_len: isize = 0;
    let mut dv: i64 = 0;
    let lf = format.len() as isize - 1;
    // Has fraction part
    if (decimal_point_pos as isize) < lf {
        let dv_pre = one_to_six_digit_prefix(&format[decimal_point_pos + 1..]); // the numberical prefix of the fraction part
        decimal_len = dv_pre.len() as isize;
        if decimal_len >= 6 {
            // MySQL rounds down to 1e-6.
            let (v, e) = parse_int(&dv_pre[0..6], 64);
            dv = v;
            if e.is_err() && err.is_ok() {
                err = Err(wrong());
            }
        } else {
            let mut digits = dv_pre.to_vec();
            digits.extend_from_slice(&b"000000"[..(6 - decimal_len) as usize]);
            let (v, e) = parse_int(&digits, 64);
            dv = v;
            if e.is_err() && err.is_ok() {
                err = Err(wrong());
            }
        }
        if dv >= 500000 {
            // Round up, and we should keep 6 digits for microsecond, so dv should in [000000, 999999].
            riv = riv.wrapping_add(sign);
        }
        if unit != b"SECOND" && err.is_ok() {
            err = Err(ErrTruncatedWrongVal.gen_with_stack_by_args(vec![go_str(format)]));
        }
        dv *= sign;
    }
    let abs = |v: i64| v.wrapping_abs();
    let zero: DurationValue = (0, 0, 0, 0, 0);
    match to_upper_go(unit).as_slice() {
        b"MICROSECOND" => {
            if strict_check && abs(riv) > TimeMaxValueSeconds as i64 * 1000 {
                return (zero, Err(overflow()));
            }
            let day_count = riv / (GoDurationDay / gotime::MICROSECOND);
            riv %= GoDurationDay / gotime::MICROSECOND;
            (
                (
                    0,
                    0,
                    day_count,
                    riv.wrapping_mul(gotime::MICROSECOND),
                    MaxFsp,
                ),
                err,
            )
        }
        b"SECOND" => {
            if strict_check && abs(iv) > TimeMaxValueSeconds as i64 {
                return (zero, Err(overflow()));
            }
            let day_count = iv / (GoDurationDay / gotime::SECOND);
            iv %= GoDurationDay / gotime::SECOND;
            (
                (
                    0,
                    0,
                    day_count,
                    iv.wrapping_mul(gotime::SECOND)
                        .wrapping_add(dv.wrapping_mul(gotime::MICROSECOND)),
                    decimal_len,
                ),
                err,
            )
        }
        b"MINUTE" => {
            if strict_check && abs(riv) > (TimeMaxHour * 60 + TimeMaxMinute) as i64 {
                return (zero, Err(overflow()));
            }
            let day_count = riv / (GoDurationDay / gotime::MINUTE);
            riv %= GoDurationDay / gotime::MINUTE;
            ((0, 0, day_count, riv.wrapping_mul(gotime::MINUTE), 0), err)
        }
        b"HOUR" => {
            if strict_check && abs(riv) > TimeMaxHour as i64 {
                return (zero, Err(overflow()));
            }
            let day_count = riv / 24;
            riv %= 24;
            ((0, 0, day_count, riv.wrapping_mul(gotime::HOUR), 0), err)
        }
        b"DAY" => {
            if strict_check && abs(riv) > (TimeMaxHour / 24) as i64 {
                return (zero, Err(overflow()));
            }
            ((0, 0, riv, 0, 0), err)
        }
        b"WEEK" => {
            if strict_check && 7i64.wrapping_mul(abs(riv)) > (TimeMaxHour / 24) as i64 {
                return (zero, Err(overflow()));
            }
            ((0, 0, 7i64.wrapping_mul(riv), 0, 0), err)
        }
        b"MONTH" => {
            if strict_check && abs(riv) > 1 {
                return (zero, Err(overflow()));
            }
            ((0, riv, 0, 0, 0), err)
        }
        b"QUARTER" => {
            if strict_check {
                return (zero, Err(overflow()));
            }
            ((0, 3i64.wrapping_mul(riv), 0, 0, 0), err)
        }
        b"YEAR" => {
            if strict_check {
                return (zero, Err(overflow()));
            }
            ((riv, 0, 0, 0, 0), err)
        }
        _ => (
            zero,
            Err(tidb_base::errors::errorf(
                "invalid singel timeunit - %s",
                &[go_str(unit)],
            )),
        ),
    }
}

/// parseTimeValue gets years, months, days, nanoseconds and fsp from a string
/// nanosecond will not exceed length of single day
/// MySQL permits any punctuation delimiter in the expr format.
/// See https://dev.mysql.com/doc/refman/8.0/en/expressions.html#temporal-intervals
// go: parseTimeValue L1 [M-ERRVAL]
fn parse_time_value(format: &[u8], index: usize, cnt: usize) -> (DurationValue, Result<(), Error>) {
    let mut neg = false;
    let original_fmt = format;
    let wrong =
        || ErrWrongValue.gen_with_stack_by_args(vec![Arg::from(DateTimeStr), go_str(original_fmt)]);
    let fsp = if index == MicrosecondIndex {
        MaxFsp
    } else {
        MinFsp
    };
    let mut format = trim_space_go(format);
    if !format.is_empty() && format[0] == b'-' {
        neg = true;
        format = &format[1..];
    }
    let mut fields: Vec<Vec<u8>> = vec![b"0".to_vec(); TimeValueCnt];
    let matches = numeric_runs(format);
    if matches.len() > cnt {
        return ((0, 0, 0, 0, 0), Err(wrong()));
    }
    let mut index = index as isize;
    for i in 0..matches.len() {
        let m = matches[matches.len() - 1 - i];
        if neg {
            let mut v = b"-".to_vec();
            v.extend_from_slice(m);
            fields[index as usize] = v;
        } else {
            fields[index as usize] = m.to_vec();
        }
        index -= 1;
    }

    // ParseInt may return an error when overflowed, but we should continue to parse the rest of the string because
    // the caller may ignore the error and use the return value.
    // In this case, we should return a big value to make sure the result date after adding this interval
    // is also overflowed and NULL is returned to the user.
    let mut err: Result<(), Error> = Ok(());
    let (years, e) = parse_int(&fields[YearIndex], 64);
    if e.is_err() {
        err = Err(wrong());
    }
    let (months, e) = parse_int(&fields[MonthIndex], 64);
    if err.is_ok() && e.is_err() {
        err = Err(wrong());
    }
    let (mut days, e) = parse_int(&fields[DayIndex], 64);
    if err.is_ok() && e.is_err() {
        err = Err(wrong());
    }

    let (hours, e) = parse_int(&fields[HourIndex], 64);
    if e.is_err() && err.is_ok() {
        err = Err(wrong());
    }
    let (minutes, e) = parse_int(&fields[MinuteIndex], 64);
    if e.is_err() && err.is_ok() {
        err = Err(wrong());
    }
    let (mut seconds, e) = parse_int(&fields[SecondIndex], 64);
    if e.is_err() && err.is_ok() {
        err = Err(wrong());
    }
    let (microseconds, e) = parse_int(&align_frac(&fields[MicrosecondIndex], MaxFsp), 64);
    if e.is_err() && err.is_ok() {
        err = Err(wrong());
    }
    seconds = hours
        .wrapping_mul(3600)
        .wrapping_add(minutes.wrapping_mul(60))
        .wrapping_add(seconds);
    days = days.wrapping_add(seconds / (3600 * 24));
    seconds %= 3600 * 24;
    (
        (
            years,
            months,
            days,
            seconds
                .wrapping_mul(gotime::SECOND)
                .wrapping_add(microseconds.wrapping_mul(gotime::MICROSECOND)),
            fsp,
        ),
        err,
    )
}

// go: parseAndValidateDurationValue L1
fn parse_and_validate_duration_value(
    format: &[u8],
    index: usize,
    cnt: usize,
) -> Result<(i64, isize), Error> {
    let overflow = || ErrDatetimeFunctionOverflow.gen_with_stack_by_args(vec![Arg::from("time")]);
    let ((year, month, day, nano, fsp), err) = parse_time_value(format, index, cnt);
    err?;
    if year != 0 || month != 0 || day.wrapping_abs() > (TimeMaxHour / 24) as i64 {
        return Err(overflow());
    }
    let dur = day.wrapping_mul(GoDurationDay).wrapping_add(nano);
    if dur.wrapping_abs() > MaxTime {
        return Err(overflow());
    }
    Ok((dur, fsp))
}

/// ParseDurationValue parses time value from time unit and format.
/// Returns y years m months d days + n nanoseconds
/// Nanoseconds will no longer than one day.
// go: ParseDurationValue L1 [M-ERRVAL]
pub fn parse_duration_value(unit: &[u8], format: &[u8]) -> (DurationValue, Result<(), Error>) {
    match to_upper_go(unit).as_slice() {
        b"MICROSECOND" | b"SECOND" | b"MINUTE" | b"HOUR" | b"DAY" | b"WEEK" | b"MONTH"
        | b"QUARTER" | b"YEAR" => parse_single_time_value(unit, format, false),
        b"SECOND_MICROSECOND" => {
            parse_time_value(format, MicrosecondIndex, SecondMicrosecondMaxCnt)
        }
        b"MINUTE_MICROSECOND" => {
            parse_time_value(format, MicrosecondIndex, MinuteMicrosecondMaxCnt)
        }
        b"MINUTE_SECOND" => parse_time_value(format, SecondIndex, MinuteSecondMaxCnt),
        b"HOUR_MICROSECOND" => parse_time_value(format, MicrosecondIndex, HourMicrosecondMaxCnt),
        b"HOUR_SECOND" => parse_time_value(format, SecondIndex, HourSecondMaxCnt),
        b"HOUR_MINUTE" => parse_time_value(format, MinuteIndex, HourMinuteMaxCnt),
        b"DAY_MICROSECOND" => parse_time_value(format, MicrosecondIndex, DayMicrosecondMaxCnt),
        b"DAY_SECOND" => parse_time_value(format, SecondIndex, DaySecondMaxCnt),
        b"DAY_MINUTE" => parse_time_value(format, MinuteIndex, DayMinuteMaxCnt),
        b"DAY_HOUR" => parse_time_value(format, HourIndex, DayHourMaxCnt),
        b"YEAR_MONTH" => parse_time_value(format, MonthIndex, YearMonthMaxCnt),
        _ => (
            (0, 0, 0, 0, 0),
            Err(tidb_base::errors::errorf(
                "invalid single timeunit - %s",
                &[go_str(unit)],
            )),
        ),
    }
}

/// ExtractDurationValue extract the value from format to Duration.
// go: ExtractDurationValue L1
pub fn extract_duration_value(unit: &[u8], format: &[u8]) -> Result<Duration, Error> {
    let unit = to_upper_go(unit);
    let dur = |r: Result<(i64, isize), Error>| -> Result<Duration, Error> {
        let (d, fsp) = r?;
        Ok(Duration { duration: d, fsp })
    };
    match unit.as_slice() {
        b"MICROSECOND" | b"SECOND" | b"MINUTE" | b"HOUR" | b"DAY" | b"WEEK" | b"MONTH"
        | b"QUARTER" | b"YEAR" => {
            let ((_, month, day, nano, fsp), err) = parse_single_time_value(&unit, format, true);
            err?;
            let d = Duration {
                duration: month
                    .wrapping_mul(30)
                    .wrapping_add(day)
                    .wrapping_mul(GoDurationDay)
                    .wrapping_add(nano),
                fsp,
            };
            Ok(d)
        }
        b"SECOND_MICROSECOND" => dur(parse_and_validate_duration_value(
            format,
            MicrosecondIndex,
            SecondMicrosecondMaxCnt,
        )),
        b"MINUTE_MICROSECOND" => dur(parse_and_validate_duration_value(
            format,
            MicrosecondIndex,
            MinuteMicrosecondMaxCnt,
        )),
        b"MINUTE_SECOND" => dur(parse_and_validate_duration_value(
            format,
            SecondIndex,
            MinuteSecondMaxCnt,
        )),
        b"HOUR_MICROSECOND" => dur(parse_and_validate_duration_value(
            format,
            MicrosecondIndex,
            HourMicrosecondMaxCnt,
        )),
        b"HOUR_SECOND" => dur(parse_and_validate_duration_value(
            format,
            SecondIndex,
            HourSecondMaxCnt,
        )),
        b"HOUR_MINUTE" => dur(parse_and_validate_duration_value(
            format,
            MinuteIndex,
            HourMinuteMaxCnt,
        )),
        b"DAY_MICROSECOND" => dur(parse_and_validate_duration_value(
            format,
            MicrosecondIndex,
            DayMicrosecondMaxCnt,
        )),
        b"DAY_SECOND" => dur(parse_and_validate_duration_value(
            format,
            SecondIndex,
            DaySecondMaxCnt,
        )),
        b"DAY_MINUTE" => dur(parse_and_validate_duration_value(
            format,
            MinuteIndex,
            DayMinuteMaxCnt,
        )),
        b"DAY_HOUR" => dur(parse_and_validate_duration_value(
            format,
            HourIndex,
            DayHourMaxCnt,
        )),
        b"YEAR_MONTH" => {
            parse_and_validate_duration_value(format, MonthIndex, YearMonthMaxCnt)?;
            // MONTH must exceed the limit of mysql's duration. So just returns overflow error.
            Err(ErrDatetimeFunctionOverflow.gen_with_stack_by_args(vec![Arg::from("time")]))
        }
        _ => Err(tidb_base::errors::errorf(
            "invalid single timeunit - %s",
            &[go_str(unit)],
        )),
    }
}

/// IsClockUnit returns true when unit is interval unit with hour, minute, second or microsecond.
// go: IsClockUnit L1
pub fn is_clock_unit(unit: &[u8]) -> bool {
    matches!(
        to_upper_go(unit).as_slice(),
        b"MICROSECOND"
            | b"SECOND"
            | b"MINUTE"
            | b"HOUR"
            | b"SECOND_MICROSECOND"
            | b"MINUTE_MICROSECOND"
            | b"HOUR_MICROSECOND"
            | b"DAY_MICROSECOND"
            | b"MINUTE_SECOND"
            | b"HOUR_SECOND"
            | b"DAY_SECOND"
            | b"HOUR_MINUTE"
            | b"DAY_MINUTE"
            | b"DAY_HOUR"
    )
}

/// IsDateUnit returns true when unit is interval unit with year, quarter, month, week or day.
// go: IsDateUnit L1
pub fn is_date_unit(unit: &[u8]) -> bool {
    matches!(
        to_upper_go(unit).as_slice(),
        b"DAY"
            | b"WEEK"
            | b"MONTH"
            | b"QUARTER"
            | b"YEAR"
            | b"DAY_MICROSECOND"
            | b"DAY_SECOND"
            | b"DAY_MINUTE"
            | b"DAY_HOUR"
            | b"YEAR_MONTH"
    )
}

/// IsMicrosecondUnit returns true when unit is interval unit with microsecond.
// go: IsMicrosecondUnit L1
pub fn is_microsecond_unit(unit: &[u8]) -> bool {
    matches!(
        to_upper_go(unit).as_slice(),
        b"MICROSECOND"
            | b"SECOND_MICROSECOND"
            | b"MINUTE_MICROSECOND"
            | b"HOUR_MICROSECOND"
            | b"DAY_MICROSECOND"
    )
}

/// IsDateFormat returns true when the specified time format could contain only date.
// go: IsDateFormat L1
pub fn is_date_format(format: &[u8]) -> bool {
    let format = trim_space_go(format);
    let seps = parse_date_format(format);
    let length = format.len();
    match seps.map(|s| s.len()).unwrap_or(0) {
        1 => {
            // "20129" will be parsed to 2020-12-09, which is date format.
            if (length == 8) || (length == 6) || (length == 5) {
                return true;
            }
        }
        3 => return true,
        _ => {}
    }
    false
}

/// ParseTimeFromInt64 parses mysql time value from int64.
// go: ParseTimeFromInt64 L1 [M-ERRVAL]
pub fn parse_time_from_int64(ctx: &Context, num: i64) -> (Time, Result<(), Error>) {
    parse_date_time_from_num(ctx, num)
}

/// ParseTimeFromFloat64 parses mysql time value from float64.
/// It is used in scenarios that distinguish date and datetime, e.g., date_add/sub() with first argument being real.
/// For example, 20010203 parses to date (no HMS) and 20010203040506 parses to datetime (with HMS).
// go: ParseTimeFromFloat64 L1 [M-ERRVAL, M-F2I]
pub fn parse_time_from_float64(ctx: &Context, f: f64) -> (Time, Result<(), Error>) {
    // [M-F2I]
    let int_part = tidb_base::gostd::conv::f64_to_i64(f);
    let (mut t, err) = parse_date_time_from_num(ctx, int_part);
    if let Err(e) = err {
        return (ZeroTime, Err(e));
    }
    if t.tp() == mysql::TypeDatetime {
        // US part is only kept when the integral part is recognized as datetime.
        // [M-F2I]
        let frac_part = tidb_base::gostd::conv::f64_to_u32((f - int_part as f64) * 1000000.0);
        let mut ct = t.core_time();
        ct.set_microsecond(frac_part);
        t.set_core_time(ct);
    }
    (t, Ok(()))
}

/// ParseTimeFromDecimal parses mysql time value from decimal.
/// It is used in scenarios that distinguish date and datetime, e.g., date_add/sub() with first argument being decimal.
/// For example, 20010203 parses to date (no HMS) and 20010203040506 parses to datetime (with HMS).
// go: ParseTimeFromDecimal L1 [M-ERRVAL]
pub fn parse_time_from_decimal(ctx: &Context, dec: &MyDecimal) -> (Time, Result<(), Error>) {
    let incorrect =
        || ErrIncorrectDatetimeValue.gen_with_stack_by_args(vec![go_str(dec.to_string())]);
    let (int_part, err) = dec.to_int();
    if let Err(e) = err
        && !ErrTruncated.equal(&e)
    {
        return (ZeroTime, Err(e));
    }
    let fsp = (MaxFsp).min(isize::from(dec.get_digits_frac()));
    let (mut t, err) = parse_date_time_from_num(ctx, int_part);
    if let Err(e) = err {
        return (ZeroTime, Err(e));
    }
    t.set_fsp(fsp);
    if fsp == 0 || t.tp() == mysql::TypeDate {
        // Shortcut for integer value or date value (fractional part omitted).
        return (t, Ok(()));
    }

    let int_part_dec = crate::mydecimal::new_dec_from_int(int_part);
    let mut frac_part_dec = MyDecimal::default();
    if decimal_sub(dec, &int_part_dec, &mut frac_part_dec).is_err() {
        return (ZeroTime, Err(incorrect()));
    }
    let million = crate::mydecimal::new_dec_from_int(1000000);
    let mut ms_part_dec = MyDecimal::default();
    let err = decimal_mul(&frac_part_dec, &million, &mut ms_part_dec);
    if let Err(e) = err
        && !ErrTruncated.equal(&e)
    {
        return (ZeroTime, Err(incorrect()));
    }
    let (ms_part, err) = ms_part_dec.to_int();
    if let Err(e) = err
        && !ErrTruncated.equal(&e)
    {
        return (ZeroTime, Err(incorrect()));
    }

    let mut ct = t.core_time();
    ct.set_microsecond(ms_part as u32);
    t.set_core_time(ct);

    (t, Ok(()))
}

const abbrevWeekdayName: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

/// FormatIntWidthN uses to format int with width. Insufficient digits are filled by 0.
// go: FormatIntWidthN L1
pub fn format_int_width_n(num: isize, n: usize) -> Vec<u8> {
    let num_string = num.to_string();
    if num_string.len() >= n {
        return num_string.into_bytes();
    }
    let mut out = vec![b'0'; n - num_string.len()];
    out.extend_from_slice(num_string.as_bytes());
    out
}

// go: abbrDayOfMonth L1
fn abbr_day_of_month(day: isize) -> &'static str {
    match day {
        1 | 21 | 31 => "st",
        2 | 22 => "nd",
        3 | 23 => "rd",
        _ => "th",
    }
}

/// mysqlTimeFix fixes the Time use the values in the context.
// go: mysqlTimeFix L1
fn mysql_time_fix(t: &mut CoreTime, ctx: &HashMap<&'static str, isize>) -> Result<(), Error> {
    // Key of the ctx is the format char, such as `%j` `%p` and so on.
    if let Some(_year_of_day) = ctx.get("%j") {
        // TODO: Implement the function that converts day of year to yy:mm:dd.
    }
    if let Some(&value_am_or_pm) = ctx.get("%p") {
        if ctx.contains_key("%H") {
            return Err(ErrWrongValue.gen_with_stack_by_args(vec![Arg::from(TimeStr), t.ptr_arg()]));
        }
        if t.hour() == 0 {
            return Err(ErrWrongValue.gen_with_stack_by_args(vec![Arg::from(TimeStr), t.ptr_arg()]));
        }
        if t.hour() == 12 {
            // 12 is a special hour.
            match value_am_or_pm {
                constForAM => t.set_hour(0),
                constForPM => t.set_hour(12),
                _ => {}
            }
            return Ok(());
        }
        if value_am_or_pm == constForPM {
            t.set_hour(t.get_hour().wrapping_add(12));
        }
    } else if ctx.contains_key("%h") && t.hour() == 12 {
        t.set_hour(0);
    }
    Ok(())
}

/// strToDate converts date string according to format,
/// the value will be stored in argument t or ctx.
/// The second return value is true when success but still need to append a warning.
// go: strToDate L1
fn str_to_date(
    t: &mut CoreTime,
    date: &[u8],
    format: &[u8],
    ctx: &mut HashMap<&'static str, isize>,
) -> (bool, bool) {
    let date = skip_white_space(date);
    let format = skip_white_space(format);

    let (token, format_remain, succ) = get_format_token(format);
    if !succ {
        return (false, false);
    }

    if token.is_empty() {
        if !date.is_empty() {
            // Extra characters at the end of date are ignored, but a warning should be reported at this case.
            return (true, true);
        }
        // Normal case. Both token and date are empty now.
        return (true, false);
    }

    if date.is_empty() {
        ctx.insert(intern_token(token), 0);
        return (true, false);
    }

    let (date_remain, succ) = match_date_with_token(t, date, token, ctx);
    if !succ {
        return (false, false);
    }

    str_to_date(t, date_remain, format_remain, ctx)
}

/// The keys of the strToDate context map are format tokens; the map is only ever read
/// with the keys `%j`, `%p`, `%H`, `%h`, so other tokens share one slot.
// glue: Go map[string]int keyed by arbitrary tokens, read only with four literal keys
fn intern_token(token: &[u8]) -> &'static str {
    match token {
        b"%j" => "%j",
        b"%p" => "%p",
        b"%H" => "%H",
        b"%h" => "%h",
        _ => "",
    }
}

/// getFormatToken takes one format control token from the string.
/// format "%d %H %m" will get token "%d" and the remain is " %H %m".
// go: getFormatToken L1
fn get_format_token(format: &[u8]) -> (&[u8], &[u8], bool) {
    if format.is_empty() {
        return (b"", b"", true);
    }

    // Just one character.
    if format.len() == 1 {
        if format[0] == b'%' {
            return (b"", b"", false);
        }
        return (format, b"", true);
    }

    // More than one character.
    if format[0] == b'%' {
        return (&format[..2], &format[2..], true);
    }

    (&format[..1], &format[1..], true)
}

// go: skipWhiteSpace L1
fn skip_white_space(input: &[u8]) -> &[u8] {
    let mut i = 0;
    while i < input.len() {
        let (r, n) = decode_rune(&input[i..]);
        if !crate::util_parser::is_space_rune(r.unwrap_or(char::REPLACEMENT_CHARACTER)) {
            return &input[i..];
        }
        i += n;
    }
    b""
}

/// monthAbbrev: the 3-letter lower-case month abbreviations.
// glue: the package map monthAbbrev as a match
fn month_abbrev(s: &[u8]) -> Option<u8> {
    match s {
        b"jan" => Some(1),
        b"feb" => Some(2),
        b"mar" => Some(3),
        b"apr" => Some(4),
        b"may" => Some(5),
        b"jun" => Some(6),
        b"jul" => Some(7),
        b"aug" => Some(8),
        b"sep" => Some(9),
        b"oct" => Some(10),
        b"nov" => Some(11),
        b"dec" => Some(12),
        _ => None,
    }
}

type DateFormatParser =
    fn(&mut CoreTime, &[u8], &mut HashMap<&'static str, isize>) -> Option<usize>;

/// dateFormatParserTable: the parser of each format token. A parser returns the number of
/// bytes it consumed (`Some`) or failure (`None`); Go returns the remaining string.
// glue: the package map dateFormatParserTable as a match
fn date_format_parser(token: &[u8]) -> Option<DateFormatParser> {
    let p: DateFormatParser = match token {
        b"%b" => abbreviated_month,        // Abbreviated month name (Jan..Dec)
        b"%c" => month_numeric,            // Month, numeric (0..12)
        b"%d" => day_of_month_numeric,     // Day of the month, numeric (0..31)
        b"%e" => day_of_month_numeric,     // Day of the month, numeric (0..31)
        b"%f" => micro_seconds,            // Microseconds (000000..999999)
        b"%h" => hour12_numeric,           // Hour (01..12)
        b"%H" => hour24_numeric,           // Hour (00..23)
        b"%I" => hour12_numeric,           // Hour (01..12)
        b"%i" => minutes_numeric,          // Minutes, numeric (00..59)
        b"%j" => day_of_year_numeric,      // Day of year (001..366)
        b"%k" => hour24_numeric,           // Hour (0..23)
        b"%l" => hour12_numeric,           // Hour (1..12)
        b"%M" => full_name_month,          // Month name (January..December)
        b"%m" => month_numeric,            // Month, numeric (00..12)
        b"%p" => is_am_or_pm,              // AM or PM
        b"%r" => time12_hour,              // Time, 12-hour (hh:mm:ss followed by AM or PM)
        b"%s" => seconds_numeric,          // Seconds (00..59)
        b"%S" => seconds_numeric,          // Seconds (00..59)
        b"%T" => time24_hour,              // Time, 24-hour (hh:mm:ss)
        b"%Y" => year_numeric_four_digits, // Year, numeric, four digits
        b"%#" => skip_all_nums,            // Skip all numbers
        b"%." => skip_all_punct,           // Skip all punctation characters
        b"%@" => skip_all_alpha,           // Skip all alpha characters
        // Deprecated since MySQL 5.7.5
        b"%y" => year_numeric_two_digits, // Year, numeric (two digits)
        _ => return None,
    };
    Some(p)
}

/// GetFormatType checks the type(Duration, Date or Datetime) of a format string.
// go: GetFormatType L1
pub fn get_format_type(format: &[u8]) -> (bool, bool) {
    let (mut is_duration, mut is_date) = (false, false);
    let mut format = skip_white_space(format);
    loop {
        let (token, rest, succ) = get_format_token(format);
        format = rest;
        if token.is_empty() {
            break;
        }
        if !succ {
            (is_duration, is_date) = (false, false);
            break;
        }
        if token.len() >= 2 && token[0] == b'%' {
            match token[1] {
                b'h' | b'H' | b'i' | b'I' | b's' | b'S' | b'k' | b'l' | b'f' | b'r' | b'T' => {
                    is_duration = true;
                }
                b'y' | b'Y' | b'm' | b'M' | b'c' | b'b' | b'D' | b'd' | b'e' => is_date = true,
                _ => {}
            }
        }
        if is_duration && is_date {
            break;
        }
    }
    (is_duration, is_date)
}

// go: matchDateWithToken L1
fn match_date_with_token<'a>(
    t: &mut CoreTime,
    date: &'a [u8],
    token: &[u8],
    ctx: &mut HashMap<&'static str, isize>,
) -> (&'a [u8], bool) {
    if let Some(parse) = date_format_parser(token) {
        return match parse(t, date, ctx) {
            Some(n) => (&date[n..], true),
            None => (date, false),
        };
    }

    if date.starts_with(token) {
        return (&date[token.len()..], true);
    }
    (date, false)
}

/// Try to parse digits with number of `limit` starting from `input`
/// Return <number, n chars to step forward> if success.
/// Return <_, 0> if fail.
// go: parseNDigits L1 [M-INT]
fn parse_n_digits(input: &[u8], limit: usize) -> (isize, usize) {
    if limit == 0 {
        return (0, 0);
    }

    let mut num: u64 = 0;
    let mut step = 0;
    while step < input.len() && step < limit && input[step].is_ascii_digit() {
        num = num
            .wrapping_mul(10)
            .wrapping_add(u64::from(input[step] - b'0'));
        step += 1;
    }
    (num as isize, step)
}

// go: secondsNumeric L1
fn seconds_numeric(
    t: &mut CoreTime,
    input: &[u8],
    _: &mut HashMap<&'static str, isize>,
) -> Option<usize> {
    let (v, step) = parse_n_digits(input, 2);
    if step == 0 || v >= 60 {
        return None;
    }
    t.set_second(v as u8);
    Some(step)
}

// go: minutesNumeric L1
fn minutes_numeric(
    t: &mut CoreTime,
    input: &[u8],
    _: &mut HashMap<&'static str, isize>,
) -> Option<usize> {
    let (v, step) = parse_n_digits(input, 2);
    if step == 0 || v >= 60 {
        return None;
    }
    t.set_minute(v as u8);
    Some(step)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ParseState {
    Normal,
    Fail,
    EndOfLine,
}

// go: parseSep L1
fn parse_sep(input: &[u8]) -> (&[u8], ParseState) {
    let input = skip_white_space(input);
    if input.is_empty() {
        return (input, ParseState::EndOfLine);
    }
    if input[0] != b':' {
        return (input, ParseState::Fail);
    }
    let input = skip_white_space(&input[1..]);
    if input.is_empty() {
        return (input, ParseState::EndOfLine);
    }
    (input, ParseState::Normal)
}

// glue: the byte offset of the suffix `rest` within `input`
fn consumed(input: &[u8], rest: &[u8]) -> usize {
    input.len() - rest.len()
}

// go: time12Hour L1
fn time12_hour(
    t: &mut CoreTime,
    input: &[u8],
    _: &mut HashMap<&'static str, isize>,
) -> Option<usize> {
    let try_parse = |t: &mut CoreTime, input: &[u8]| -> (usize, ParseState) {
        // hh:mm:ss AM
        // Note that we should update `t` as soon as possible, or we
        // can not get correct result for incomplete input like "12:13"
        // that is shorter than "hh:mm:ss"
        let orig = input;
        let (mut hour, step) = parse_n_digits(input, 2); // 1..12
        if step == 0 || hour > 12 || hour == 0 {
            return (consumed(orig, input), ParseState::Fail);
        }
        // Handle special case: 12:34:56 AM -> 00:34:56
        // For PM, we will add 12 it later
        if hour == 12 {
            hour = 0;
        }
        t.set_hour(hour as u8);

        // ':'
        let (input, state) = parse_sep(&input[step..]);
        if state != ParseState::Normal {
            return (consumed(orig, input), state);
        }

        let (minute, step) = parse_n_digits(input, 2); // 0..59
        if step == 0 || minute > 59 {
            return (consumed(orig, input), ParseState::Fail);
        }
        t.set_minute(minute as u8);

        // ':'
        let (input, state) = parse_sep(&input[step..]);
        if state != ParseState::Normal {
            return (consumed(orig, input), state);
        }

        let (second, step) = parse_n_digits(input, 2); // 0..59
        if step == 0 || second > 59 {
            return (consumed(orig, input), ParseState::Fail);
        }
        t.set_second(second as u8);

        let input = skip_white_space(&input[step..]);
        if input.is_empty() {
            // No "AM"/"PM" suffix, it is ok
            return (consumed(orig, input), ParseState::EndOfLine);
        } else if input.len() < 2 {
            // some broken char, fail
            return (consumed(orig, input), ParseState::Fail);
        }

        if has_case_insensitive_prefix(input, b"AM") {
            t.set_hour(hour as u8);
        } else if has_case_insensitive_prefix(input, b"PM") {
            t.set_hour((hour + 12) as u8);
        } else {
            return (consumed(orig, input), ParseState::Fail);
        }

        (consumed(orig, &input[2..]), ParseState::Normal)
    };

    let (n, state) = try_parse(t, input);
    if state == ParseState::Fail {
        return None;
    }
    Some(n)
}

// go: time24Hour L1
fn time24_hour(
    t: &mut CoreTime,
    input: &[u8],
    _: &mut HashMap<&'static str, isize>,
) -> Option<usize> {
    let try_parse = |t: &mut CoreTime, input: &[u8]| -> (usize, ParseState) {
        // hh:mm:ss
        // Note that we should update `t` as soon as possible, or we
        // can not get correct result for incomplete input like "12:13"
        // that is shorter than "hh:mm:ss"
        let orig = input;
        let (hour, step) = parse_n_digits(input, 2); // 0..23
        if step == 0 || hour > 23 {
            return (consumed(orig, input), ParseState::Fail);
        }
        t.set_hour(hour as u8);

        // ':'
        let (input, state) = parse_sep(&input[step..]);
        if state != ParseState::Normal {
            return (consumed(orig, input), state);
        }

        let (minute, step) = parse_n_digits(input, 2); // 0..59
        if step == 0 || minute > 59 {
            return (consumed(orig, input), ParseState::Fail);
        }
        t.set_minute(minute as u8);

        // ':'
        let (input, state) = parse_sep(&input[step..]);
        if state != ParseState::Normal {
            return (consumed(orig, input), state);
        }

        let (second, step) = parse_n_digits(input, 2); // 0..59
        if step == 0 || second > 59 {
            return (consumed(orig, input), ParseState::Fail);
        }
        t.set_second(second as u8);
        (consumed(orig, &input[step..]), ParseState::Normal)
    };

    let (n, state) = try_parse(t, input);
    if state == ParseState::Fail {
        return None;
    }
    Some(n)
}

const constForAM: isize = 1;
const constForPM: isize = 2;

// go: isAMOrPM L1
fn is_am_or_pm(
    _: &mut CoreTime,
    input: &[u8],
    ctx: &mut HashMap<&'static str, isize>,
) -> Option<usize> {
    if input.len() < 2 {
        return None;
    }

    let s = to_lower_go(&input[..2]);
    match s.as_slice() {
        b"am" => {
            ctx.insert("%p", constForAM);
        }
        b"pm" => {
            ctx.insert("%p", constForPM);
        }
        _ => return None,
    }
    Some(2)
}

/// `strings.ToLower` of a Go string (bytes).
// go-std: strings.ToLower L1
fn to_lower_go(s: &[u8]) -> Vec<u8> {
    if s.is_ascii() {
        return s.to_ascii_lowercase();
    }
    let mut out = Vec::with_capacity(s.len());
    let mut rest = s;
    while !rest.is_empty() {
        let (r, n) = decode_rune(rest);
        let r = r.unwrap_or(char::REPLACEMENT_CHARACTER);
        write_rune(&mut out, tidb_base::gostd::to_lower_rune(r));
        rest = &rest[n..];
    }
    out
}

// go: dayOfMonthNumeric L1
fn day_of_month_numeric(
    t: &mut CoreTime,
    input: &[u8],
    _: &mut HashMap<&'static str, isize>,
) -> Option<usize> {
    let (v, step) = parse_n_digits(input, 2); // 0..31
    if step == 0 || v > 31 {
        return None;
    }
    t.set_day(v as u8);
    Some(step)
}

// go: hour24Numeric L1
fn hour24_numeric(
    t: &mut CoreTime,
    input: &[u8],
    ctx: &mut HashMap<&'static str, isize>,
) -> Option<usize> {
    let (v, step) = parse_n_digits(input, 2); // 0..23
    if step == 0 || v > 23 {
        return None;
    }
    t.set_hour(v as u8);
    ctx.insert("%H", v);
    Some(step)
}

// go: hour12Numeric L1
fn hour12_numeric(
    t: &mut CoreTime,
    input: &[u8],
    ctx: &mut HashMap<&'static str, isize>,
) -> Option<usize> {
    let (v, step) = parse_n_digits(input, 2); // 1..12
    if step == 0 || v > 12 || v == 0 {
        return None;
    }
    t.set_hour(v as u8);
    ctx.insert("%h", v);
    Some(step)
}

// go: microSeconds L1
fn micro_seconds(
    t: &mut CoreTime,
    input: &[u8],
    _: &mut HashMap<&'static str, isize>,
) -> Option<usize> {
    let (mut v, step) = parse_n_digits(input, 6);
    if step == 0 {
        t.set_microsecond(0);
        return Some(0);
    }
    for _ in step..6 {
        v *= 10;
    }
    t.set_microsecond(v as u32);
    Some(step)
}

// go: yearNumericFourDigits L1
fn year_numeric_four_digits(
    t: &mut CoreTime,
    input: &[u8],
    ctx: &mut HashMap<&'static str, isize>,
) -> Option<usize> {
    year_numeric_n_digits(t, input, ctx, 4)
}

// go: yearNumericTwoDigits L1
fn year_numeric_two_digits(
    t: &mut CoreTime,
    input: &[u8],
    ctx: &mut HashMap<&'static str, isize>,
) -> Option<usize> {
    year_numeric_n_digits(t, input, ctx, 2)
}

// go: yearNumericNDigits L1
fn year_numeric_n_digits(
    t: &mut CoreTime,
    input: &[u8],
    _: &mut HashMap<&'static str, isize>,
    n: usize,
) -> Option<usize> {
    let (mut year, step) = parse_n_digits(input, n);
    if step == 0 {
        return None;
    } else if step <= 2 {
        year = adjust_year(year);
    }
    t.set_year(year as u16);
    Some(step)
}

// go: dayOfYearNumeric L1
fn day_of_year_numeric(
    _: &mut CoreTime,
    input: &[u8],
    ctx: &mut HashMap<&'static str, isize>,
) -> Option<usize> {
    // MySQL declares that "%j" should be "Day of year (001..366)". But actually,
    // it accepts a number that is up to three digits, which range is [1, 999].
    let (v, step) = parse_n_digits(input, 3);
    if step == 0 || v == 0 {
        return None;
    }
    ctx.insert("%j", v);
    Some(step)
}

// go: abbreviatedMonth L1
fn abbreviated_month(
    t: &mut CoreTime,
    input: &[u8],
    _: &mut HashMap<&'static str, isize>,
) -> Option<usize> {
    if input.len() >= 3 {
        let month_name = to_lower_go(&input[..3]);
        if let Some(month) = month_abbrev(&month_name) {
            t.set_month(month);
            // Go: input[len(monthName):], the lower-cased name's length
            return Some(month_name.len().min(input.len()));
        }
    }
    None
}

// go: hasCaseInsensitivePrefix L1
fn has_case_insensitive_prefix(input: &[u8], prefix: &[u8]) -> bool {
    if input.len() < prefix.len() {
        return false;
    }
    equal_fold_go(&input[..prefix.len()], prefix)
}

/// `strings.EqualFold` of Go strings (bytes).
// go-std: strings.EqualFold L1
fn equal_fold_go(s: &[u8], t: &[u8]) -> bool {
    match (std::str::from_utf8(s), std::str::from_utf8(t)) {
        (Ok(a), Ok(b)) => tidb_base::gostd::equal_fold(a, b),
        _ => {
            let a = String::from_utf8_lossy(s);
            let b = String::from_utf8_lossy(t);
            tidb_base::gostd::equal_fold(&a, &b)
        }
    }
}

// go: fullNameMonth L1
fn full_name_month(
    t: &mut CoreTime,
    input: &[u8],
    _: &mut HashMap<&'static str, isize>,
) -> Option<usize> {
    for (i, month) in MonthNames.iter().enumerate() {
        if has_case_insensitive_prefix(input, month.as_bytes()) {
            t.set_month((i + 1) as u8);
            return Some(month.len());
        }
    }
    None
}

// go: monthNumeric L1
fn month_numeric(
    t: &mut CoreTime,
    input: &[u8],
    _: &mut HashMap<&'static str, isize>,
) -> Option<usize> {
    let (v, step) = parse_n_digits(input, 2); // 1..12
    if step == 0 || v > 12 {
        return None;
    }
    t.set_month(v as u8);
    Some(step)
}

/// DateFSP gets fsp from date string.
// go: DateFSP L1
pub fn date_fsp(date: &[u8]) -> isize {
    match date.iter().rposition(|&c| c == b'.') {
        Some(i) => (date.len() - i - 1) as isize,
        None => 0,
    }
}

/// DateTimeIsOverflow returns if this date is overflow.
/// See: https://dev.mysql.com/doc/refman/8.0/en/datetime.html
// go: DateTimeIsOverflow L2 (the context location cannot be nil, so the time.Local fallback is gone)
pub fn date_time_is_overflow(ctx: &Context, date: Time) -> Result<bool, Error> {
    let tz = ctx.location();

    let (b, e);
    match date.tp() {
        mysql::TypeDate | mysql::TypeDatetime => {
            let (v, err) = MinDatetime.go_time(&tz);
            err?;
            b = v;
            let (v, err) = MaxDatetime.go_time(&tz);
            err?;
            e = v;
        }
        mysql::TypeTimestamp => {
            let (mut min_ts, mut max_ts) = (*MinTimestamp, *MaxTimestamp);
            if !gotime::is_utc(&tz) {
                min_ts.convert_time_zone(&gotime::utc(), &tz)?;
                max_ts.convert_time_zone(&gotime::utc(), &tz)?;
            }
            let (v, err) = min_ts.go_time(&tz);
            err?;
            b = v;
            let (v, err) = max_ts.go_time(&tz);
            err?;
            e = v;
        }
        _ => return Ok(false),
    }

    let (t, err) = date.adjusted_go_time(&tz);
    err?;

    let in_range = (t.after(&b) || t.equal(&b)) && (t.before(&e) || t.equal(&e));
    Ok(!in_range)
}

/// `for i, ch := range input { if !pred(ch) { break }; retIdx = i + 1 }; input[retIdx:]`:
/// note Go advances by one byte past the start of the last matching rune.
// glue: shared loop of skipAllNums / skipAllPunct / skipAllAlpha
fn skip_while_rune(input: &[u8], pred: impl Fn(char) -> bool) -> usize {
    let mut ret_idx = 0;
    let mut i = 0;
    while i < input.len() {
        let (r, n) = decode_rune(&input[i..]);
        if !pred(r.unwrap_or(char::REPLACEMENT_CHARACTER)) {
            break;
        }
        ret_idx = i + 1;
        i += n;
    }
    ret_idx
}

// go: skipAllNums L1
fn skip_all_nums(
    _: &mut CoreTime,
    input: &[u8],
    _: &mut HashMap<&'static str, isize>,
) -> Option<usize> {
    Some(skip_while_rune(input, crate::util_parser::is_number_rune))
}

// go: skipAllPunct L1
fn skip_all_punct(
    _: &mut CoreTime,
    input: &[u8],
    _: &mut HashMap<&'static str, isize>,
) -> Option<usize> {
    Some(skip_while_rune(input, crate::util_parser::is_punct_rune))
}

// go: skipAllAlpha L1
fn skip_all_alpha(
    _: &mut CoreTime,
    input: &[u8],
    _: &mut HashMap<&'static str, isize>,
) -> Option<usize> {
    Some(skip_while_rune(input, tidb_base::gostd::is_letter))
}
