//! go-file: pkg/types/core_time.go

#![allow(non_upper_case_globals)]

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::gofmt::Arg;
use tidb_base::gostd::time::{self as gotime, Location};

use crate::errors::{ErrDatetimeFunctionOverflow, ErrWrongValue, TimeStr};
use crate::time::{
    Duration, dayBitFieldMask, dayBitFieldOffset, hourBitFieldMask, hourBitFieldOffset,
    microsecondBitFieldMask, microsecondBitFieldOffset, minuteBitFieldMask, minuteBitFieldOffset,
    monthBitFieldMask, monthBitFieldOffset, secondBitFieldMask, secondBitFieldOffset,
    split_duration, yearBitFieldMask, yearBitFieldOffset,
};

/// CoreTime is the internal struct type for Time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct CoreTime(pub u64);

/// ZeroCoreTime is the zero value for TimeInternal type.
pub const ZeroCoreTime: CoreTime = CoreTime(0);

impl CoreTime {
    /// String implements fmt.Stringer.
    // go: CoreTime.String L1 [M-NAME]
    pub fn string(self) -> String {
        format!(
            "{{{} {} {} {} {} {} {}}}",
            self.get_year(),
            self.get_month(),
            self.get_day(),
            self.get_hour(),
            self.get_minute(),
            self.get_second(),
            self.get_microsecond()
        )
    }

    /// The CoreTime as a Go `fmt` argument (a `fmt.Stringer`).
    // glue: CoreTime passed as an interface{} argument to GenWithStackByArgs
    pub(crate) fn arg(self) -> Arg {
        Arg::Stringer(self.string().into_bytes(), "types.CoreTime")
    }

    /// A `*CoreTime` as a Go `fmt` argument (the pointer's method set has `String`).
    // glue: *CoreTime passed as an interface{} argument to GenWithStackByArgs
    pub(crate) fn ptr_arg(self) -> Arg {
        Arg::Stringer(self.string().into_bytes(), "*types.CoreTime")
    }

    // go: CoreTime.getYear L1
    pub(crate) fn get_year(self) -> u16 {
        ((self.0 & yearBitFieldMask) >> yearBitFieldOffset) as u16
    }

    // go: CoreTime.setYear L1
    pub(crate) fn set_year(&mut self, year: u16) {
        self.0 &= !yearBitFieldMask;
        self.0 |= (u64::from(year) << yearBitFieldOffset) & yearBitFieldMask;
    }

    /// Year returns the year value.
    // go: CoreTime.Year L1
    pub fn year(self) -> isize {
        self.get_year() as isize
    }

    // go: CoreTime.getMonth L1
    pub(crate) fn get_month(self) -> u8 {
        ((self.0 & monthBitFieldMask) >> monthBitFieldOffset) as u8
    }

    // go: CoreTime.setMonth L1
    pub(crate) fn set_month(&mut self, month: u8) {
        self.0 &= !monthBitFieldMask;
        self.0 |= (u64::from(month) << monthBitFieldOffset) & monthBitFieldMask;
    }

    /// Month returns the month value.
    // go: CoreTime.Month L1
    pub fn month(self) -> isize {
        self.get_month() as isize
    }

    // go: CoreTime.getDay L1
    pub(crate) fn get_day(self) -> u8 {
        ((self.0 & dayBitFieldMask) >> dayBitFieldOffset) as u8
    }

    // go: CoreTime.setDay L1
    pub(crate) fn set_day(&mut self, day: u8) {
        self.0 &= !dayBitFieldMask;
        self.0 |= (u64::from(day) << dayBitFieldOffset) & dayBitFieldMask;
    }

    /// Day returns the day value.
    // go: CoreTime.Day L1
    pub fn day(self) -> isize {
        self.get_day() as isize
    }

    // go: CoreTime.getHour L1
    pub(crate) fn get_hour(self) -> u8 {
        ((self.0 & hourBitFieldMask) >> hourBitFieldOffset) as u8
    }

    // go: CoreTime.setHour L1
    pub(crate) fn set_hour(&mut self, hour: u8) {
        self.0 &= !hourBitFieldMask;
        self.0 |= (u64::from(hour) << hourBitFieldOffset) & hourBitFieldMask;
    }

    /// Hour returns the hour value.
    // go: CoreTime.Hour L1
    pub fn hour(self) -> isize {
        self.get_hour() as isize
    }

    // go: CoreTime.getMinute L1
    pub(crate) fn get_minute(self) -> u8 {
        ((self.0 & minuteBitFieldMask) >> minuteBitFieldOffset) as u8
    }

    // go: CoreTime.setMinute L1
    pub(crate) fn set_minute(&mut self, minute: u8) {
        self.0 &= !minuteBitFieldMask;
        self.0 |= (u64::from(minute) << minuteBitFieldOffset) & minuteBitFieldMask;
    }

    /// Minute returns the minute value.
    // go: CoreTime.Minute L1
    pub fn minute(self) -> isize {
        self.get_minute() as isize
    }

    // go: CoreTime.getSecond L1
    pub(crate) fn get_second(self) -> u8 {
        ((self.0 & secondBitFieldMask) >> secondBitFieldOffset) as u8
    }

    // go: CoreTime.setSecond L1
    pub(crate) fn set_second(&mut self, second: u8) {
        self.0 &= !secondBitFieldMask;
        self.0 |= (u64::from(second) << secondBitFieldOffset) & secondBitFieldMask;
    }

    /// Second returns the second value.
    // go: CoreTime.Second L1
    pub fn second(self) -> isize {
        self.get_second() as isize
    }

    // go: CoreTime.getMicrosecond L1
    pub(crate) fn get_microsecond(self) -> u32 {
        ((self.0 & microsecondBitFieldMask) >> microsecondBitFieldOffset) as u32
    }

    // go: CoreTime.setMicrosecond L1
    pub(crate) fn set_microsecond(&mut self, microsecond: u32) {
        self.0 &= !microsecondBitFieldMask;
        self.0 |= (u64::from(microsecond) << microsecondBitFieldOffset) & microsecondBitFieldMask;
    }

    /// Microsecond returns the microsecond value.
    // go: CoreTime.Microsecond L1
    pub fn microsecond(self) -> isize {
        self.get_microsecond() as isize
    }

    /// Weekday returns weekday value.
    // go: CoreTime.Weekday L1
    pub fn weekday(self) -> gotime::Weekday {
        // No need to consider timezone, use the date directly.
        let (t1, _err) = self.go_time(&gotime::utc());
        // allow invalid dates
        t1.weekday()
    }

    /// YearWeek returns year and week.
    // go: CoreTime.YearWeek L1
    pub fn year_week(self, mode: isize) -> (isize, isize) {
        let behavior = week_mode(mode) | weekBehaviourYear;
        calc_week(self, behavior)
    }

    /// Week returns week value.
    // go: CoreTime.Week L1
    pub fn week(self, mode: isize) -> isize {
        if self.get_month() == 0 || self.get_day() == 0 {
            return 0;
        }
        let (_, week) = calc_week(self, week_mode(mode));
        week
    }

    /// YearDay returns year and day.
    // go: CoreTime.YearDay L1
    pub fn year_day(self) -> isize {
        if self.get_month() == 0 || self.get_day() == 0 {
            return 0;
        }
        let (year, month, day) = (self.year(), self.month(), self.day());
        calc_daynr(year, month, day) - calc_daynr(year, 1, 1) + 1
    }

    /// GoTime converts Time to GoTime.
    // go: CoreTime.GoTime L1 [M-ERRVAL]
    pub fn go_time(self, loc: &Arc<Location>) -> (gotime::Time, Result<(), Error>) {
        // gotime.Time can't represent month 0 or day 0, date contains 0 would be converted to a nearest date,
        // For example, 2006-12-00 00:00:00 would become 2015-11-30 23:59:59.
        let (year, month, day, hour, minute, second, microsecond) = (
            self.year(),
            self.month(),
            self.day(),
            self.hour(),
            self.minute(),
            self.second(),
            self.microsecond(),
        );
        let tm = gotime::date(
            year as i64,
            month as i64,
            day as i64,
            hour as i64,
            minute as i64,
            second as i64,
            microsecond as i64 * 1000,
            loc,
        );
        let (year2, month2, day2) = tm.date();
        let (hour2, minute2, second2) = tm.clock();
        let microsec2 = tm.nanosecond() / 1000;
        // This function will check the result, and return an error if it's not the same with the origin input .
        if year2 != year as i64
            || month2 != month as i64
            || day2 != day as i64
            || hour2 != hour as i64
            || minute2 != minute as i64
            || second2 != second as i64
            || microsec2 != microsecond as i64
        {
            return (
                tm,
                Err(ErrWrongValue.gen_with_stack_by_args(vec![Arg::from(TimeStr), self.arg()])),
            );
        }
        (tm, Ok(()))
    }

    /// AdjustedGoTime converts Time to GoTime and adjust for invalid DST times
    /// like during the DST change with increased offset,
    /// normally moving to Daylight Saving Time.
    /// see https://github.com/pingcap/tidb/issues/28739
    // go: CoreTime.AdjustedGoTime L1 [M-ERRVAL]
    pub fn adjusted_go_time(self, loc: &Arc<Location>) -> (gotime::Time, Result<(), Error>) {
        let (tm, err) = self.go_time(loc);
        if err.is_ok() {
            return (tm, Ok(()));
        }

        // The converted go time did not map back to the same time, probably it was between a
        // daylight saving transition, adjust the time to the closest Zone bound.
        let (start, end) = tm.zone_bounds();
        // time zone transitions are normally 1 hour, allow up to 4 hours before returning error
        if hours(gotime::duration_abs(start.sub(&tm))) > 4.0
            && hours(gotime::duration_abs(end.sub(&tm))) > 4.0
        {
            let tm_arg = Arg::Stringer(tm.string().into_bytes(), "time.Time");
            return (
                tm,
                Err(ErrWrongValue.gen_with_stack_by_args(vec![Arg::from(TimeStr), tm_arg])),
            );
        }
        // use the closest transition time
        if gotime::duration_abs(tm.sub(&start)) <= gotime::duration_abs(tm.sub(&end)) {
            return (start, Ok(()));
        }
        (end, Ok(()))
    }

    /// IsLeapYear returns if it's leap year.
    // go: CoreTime.IsLeapYear L1
    pub fn is_leap_year(self) -> bool {
        is_leap_year(self.get_year())
    }
}

/// `time.Duration.Hours`: the duration as a floating point number of hours.
// go-std: time.Duration.Hours L1
fn hours(d: gotime::Duration) -> f64 {
    let hour = d / gotime::HOUR;
    let nsec = d % gotime::HOUR;
    hour as f64 + nsec as f64 / (60.0 * 60.0 * 1e9)
}

// go: isLeapYear L1
pub(crate) fn is_leap_year(year: u16) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}

const daysByMonth: [isize; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

/// GetLastDay returns the last day of the month
// go: GetLastDay L1 [M-INT]
pub fn get_last_day(year: isize, month: isize) -> isize {
    let mut day = 0;
    if month > 0 && month <= 12 {
        day = daysByMonth[(month - 1) as usize];
    }
    if month == 2 && is_leap_year(year as u16) {
        day = 29;
    }
    day
}

// go: getFixDays L1
fn get_fix_days(year: isize, month: isize, day: isize, ot: &gotime::Time) -> isize {
    if (year != 0 || month != 0) && day == 0 {
        let od = ot.day() as isize;
        let t = ot.add_date(year as i64, month as i64, day as i64);
        let td = t.day() as isize;
        if od != td {
            let tm = t.month() as isize - 1;
            let t_max = get_last_day(t.year() as isize, tm);
            let dd = t_max - od;
            return dd;
        }
    }
    0
}

/// compareTime compare two Time.
/// return:
///
///    0: if a == b
///    1: if a > b
///
/// -1: if a < b
// go: compareTime L1
pub(crate) fn compare_time(a: CoreTime, b: CoreTime) -> isize {
    let ta = datetime_to_uint64(a);
    let tb = datetime_to_uint64(b);

    if ta < tb {
        return -1;
    } else if ta > tb {
        return 1;
    }

    if a.microsecond() < b.microsecond() {
        return -1;
    } else if a.microsecond() > b.microsecond() {
        return 1;
    }

    0
}

/// AddDate fix gap between mysql and golang api
/// When we execute select date_add('2018-01-31',interval 1 month) in mysql we got 2018-02-28
/// but in tidb we got 2018-03-03.
/// Dig it and we found it's caused by golang api time.Date(year int, month Month, day, hour, min, sec, nsec int, loc *Location) Time ,
/// it says October 32 converts to November 1 ,it conflicts with mysql.
/// See https://dev.mysql.com/doc/refman/5.7/en/date-and-time-functions.html#function_date-add
// go: AddDate L1 [M-ERRVAL]
pub fn add_date(
    year: i64,
    month: i64,
    day: i64,
    ot: &gotime::Time,
) -> (gotime::Time, Result<(), Error>) {
    // We must limit the range of year, month and day to avoid overflow.
    // The datetime range is from '1000-01-01 00:00:00.000000' to '9999-12-31 23:59:59.499999',
    // so it is safe to limit the added value from -10000*365 to 10000*365.
    const MAX_ADD: i64 = 10000 * 365;
    const MIN_ADD: i64 = -MAX_ADD;
    if !(MIN_ADD..=MAX_ADD).contains(&year)
        || !(MIN_ADD..=MAX_ADD).contains(&month)
        || !(MIN_ADD..=MAX_ADD).contains(&day)
    {
        return (
            gotime::Time::zero(),
            Err(ErrDatetimeFunctionOverflow.gen_with_stack_by_args(vec![Arg::from("datetime")])),
        );
    }

    let df = get_fix_days(year as isize, month as isize, day as isize, ot);
    let nt = if df != 0 {
        ot.add_date(year, month, df as i64)
    } else {
        ot.add_date(year, month, day)
    };

    if nt.year() < 0 || nt.year() > 9999 {
        return (
            nt,
            Err(ErrDatetimeFunctionOverflow.gen_with_stack_by_args(vec![Arg::from("datetime")])),
        );
    }

    (nt, Ok(()))
}

// go: calcTimeFromSec L1 [M-INT]
pub(crate) fn calc_time_from_sec(to: &mut CoreTime, seconds: isize, microseconds: isize) {
    to.set_hour((seconds / 3600) as u8);
    let seconds = seconds % 3600;
    to.set_minute((seconds / 60) as u8);
    to.set_second((seconds % 60) as u8);
    to.set_microsecond(microseconds as u32);
}

pub(crate) const secondsIn24Hour: isize = 86400;

// go: calcTimeDiffInternal L1 [M-INT]
#[allow(clippy::too_many_arguments)]
fn calc_time_diff_internal(
    t1: CoreTime,
    year: isize,
    month: isize,
    day: isize,
    hour: isize,
    minute: isize,
    second: isize,
    microsecond: isize,
    sign: isize,
) -> (isize, isize, bool) {
    let mut days = calc_daynr(t1.year(), t1.month(), t1.day());
    let days2 = calc_daynr(year, month, day);
    days -= sign * days2;

    let mut tmp = ((days as i64)
        .wrapping_mul(secondsIn24Hour as i64)
        .wrapping_add(t1.hour() as i64 * 3600)
        .wrapping_add(t1.minute() as i64 * 60)
        .wrapping_add(t1.second() as i64)
        .wrapping_sub(
            (sign as i64).wrapping_mul(
                (hour as i64)
                    .wrapping_mul(3600)
                    .wrapping_add((minute as i64).wrapping_mul(60))
                    .wrapping_add(second as i64),
            ),
        ))
    .wrapping_mul(1_000_000)
    .wrapping_add(t1.microsecond() as i64)
    .wrapping_sub((sign as i64).wrapping_mul(microsecond as i64));

    let mut neg = false;
    if tmp < 0 {
        tmp = tmp.wrapping_neg();
        neg = true;
    }
    let seconds = (tmp / 1_000_000) as isize;
    let microseconds = (tmp % 1_000_000) as isize;
    (seconds, microseconds, neg)
}

/// calcTimeDiff calculates difference between two datetime values as seconds + microseconds.
/// sign can be +1 or -1, and t2 is preprocessed with sign first.
// go: calcTimeTimeDiff L1
pub(crate) fn calc_time_time_diff(t1: CoreTime, t2: CoreTime, sign: isize) -> (isize, isize, bool) {
    calc_time_diff_internal(
        t1,
        t2.year(),
        t2.month(),
        t2.day(),
        t2.hour(),
        t2.minute(),
        t2.second(),
        t2.microsecond(),
        sign,
    )
}

/// calcTimeDiff calculates difference between a datetime value and a duration as seconds + microseconds.
// go: calcTimeDurationDiff L1
pub(crate) fn calc_time_duration_diff(t: CoreTime, d: Duration) -> (isize, isize, bool) {
    let (sign, hh, mm, ss, micro) = split_duration(d.duration);
    calc_time_diff_internal(t, 0, 0, 0, hh, mm, ss, micro, -sign)
}

/// datetimeToUint64 converts time value to integer in YYYYMMDDHHMMSS format.
// go: datetimeToUint64 L1
pub(crate) fn datetime_to_uint64(t: CoreTime) -> u64 {
    t.year() as u64 * 10_000_000_000
        + t.month() as u64 * 100_000_000
        + t.day() as u64 * 1_000_000
        + t.hour() as u64 * 10_000
        + t.minute() as u64 * 100
        + t.second() as u64
}

/// calcDaynr calculates days since 0000-00-00.
// go: calcDaynr L1
pub(crate) fn calc_daynr(year: isize, month: isize, day: isize) -> isize {
    if year == 0 && month == 0 {
        return 0;
    }

    let mut year = year;
    let mut delsum = 365 * year + 31 * (month - 1) + day;
    if month <= 2 {
        year -= 1;
    } else {
        delsum -= (month * 4 + 23) / 10;
    }
    let temp = ((year / 100 + 1) * 3) / 4;
    delsum + year / 4 - temp
}

/// DateDiff calculates number of days between two days.
// go: DateDiff L1
pub fn date_diff(start_time: CoreTime, end_time: CoreTime) -> isize {
    calc_daynr(start_time.year(), start_time.month(), start_time.day())
        - calc_daynr(end_time.year(), end_time.month(), end_time.day())
}

/// calcDaysInYear calculates days in one year, it works with 0 <= year <= 99.
// go: calcDaysInYear L1
pub(crate) fn calc_days_in_year(year: isize) -> isize {
    if (year & 3) == 0 && (year % 100 != 0 || (year % 400 == 0 && (year != 0))) {
        return 366;
    }
    365
}

/// calcWeekday calculates weekday from daynr, returns 0 for Monday, 1 for Tuesday ...
// go: calcWeekday L1
pub(crate) fn calc_weekday(mut daynr: isize, sunday_first_day_of_week: bool) -> isize {
    daynr += 5;
    if sunday_first_day_of_week {
        daynr += 1;
    }
    daynr % 7
}

/// weekBehaviour: the flags of a week mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WeekBehaviour(u32);

impl std::ops::BitOr for WeekBehaviour {
    type Output = WeekBehaviour;
    // glue: Go `|` on weekBehaviour
    fn bitor(self, rhs: WeekBehaviour) -> WeekBehaviour {
        WeekBehaviour(self.0 | rhs.0)
    }
}

/// weekBehaviourMondayFirst set Monday as first day of week; otherwise Sunday is first day of week
pub(crate) const weekBehaviourMondayFirst: WeekBehaviour = WeekBehaviour(1 << 0);
/// If set, Week is in range 1-53, otherwise Week is in range 0-53.
/// Note that this flag is only relevant if WEEK_JANUARY is not set.
pub(crate) const weekBehaviourYear: WeekBehaviour = WeekBehaviour(1 << 1);
/// If not set, Weeks are numbered according to ISO 8601:1988.
/// If set, the week that contains the first 'first-day-of-week' is week 1.
pub(crate) const weekBehaviourFirstWeekday: WeekBehaviour = WeekBehaviour(1 << 2);

impl WeekBehaviour {
    // go: weekBehaviour.test L1
    fn test(self, flag: WeekBehaviour) -> bool {
        (self.0 & flag.0) != 0
    }
}

// go: weekMode L1 [M-INT]
pub(crate) fn week_mode(mode: isize) -> WeekBehaviour {
    let mut week_format = WeekBehaviour((mode & 7) as u32);
    if (week_format.0 & weekBehaviourMondayFirst.0) == 0 {
        week_format.0 ^= weekBehaviourFirstWeekday.0;
    }
    week_format
}

/// calcWeek calculates week and year for the time.
// go: calcWeek L1
pub(crate) fn calc_week(t: CoreTime, wb: WeekBehaviour) -> (isize, isize) {
    let mut days: isize;
    let (ty, tm, td) = (
        t.get_year() as isize,
        t.get_month() as isize,
        t.get_day() as isize,
    );
    let daynr = calc_daynr(ty, tm, td);
    let mut first_daynr = calc_daynr(ty, 1, 1);
    let monday_first = wb.test(weekBehaviourMondayFirst);
    let mut week_year = wb.test(weekBehaviourYear);
    let first_weekday = wb.test(weekBehaviourFirstWeekday);

    let mut weekday = calc_weekday(first_daynr, !monday_first);

    let mut year = ty;

    if tm == 1 && td <= 7 - weekday {
        if !week_year && ((first_weekday && weekday != 0) || (!first_weekday && weekday >= 4)) {
            return (year, 0);
        }
        week_year = true;
        year -= 1;
        days = calc_days_in_year(year);
        first_daynr -= days;
        weekday = (weekday + 53 * 7 - days) % 7;
    }

    if (first_weekday && weekday != 0) || (!first_weekday && weekday >= 4) {
        days = daynr - (first_daynr + 7 - weekday);
    } else {
        days = daynr - (first_daynr - weekday);
    }

    if week_year && days >= 52 * 7 {
        weekday = (weekday + calc_days_in_year(year)) % 7;
        if (!first_weekday && weekday < 4) || (first_weekday && weekday == 0) {
            year += 1;
            return (year, 1);
        }
    }
    (year, days / 7 + 1)
}

/// mixDateAndDuration mixes a date value and a duration value.
// go: mixDateAndDuration L1 [M-INT]
pub(crate) fn mix_date_and_duration(date: &mut CoreTime, dur: Duration) {
    if dur.duration >= 0 && dur.hour() < 24 {
        let (_, hh, mm, ss, frac) = split_duration(dur.duration);
        date.set_hour(hh as u8);
        date.set_minute(mm as u8);
        date.set_second(ss as u8);
        date.set_microsecond(frac as u32);
        return;
    }

    // Time is negative or outside of 24 hours internal.
    let (seconds, microseconds, _) = calc_time_duration_diff(*date, dur);

    // If we want to use this function with arbitrary dates, this code will need
    // to cover cases when time is negative and "date < -time".

    let days = seconds / secondsIn24Hour;
    calc_time_from_sec(date, seconds % secondsIn24Hour, microseconds);
    let (year, month, day) = get_date_from_daynr(days as usize);
    date.set_year(year as u16);
    date.set_month(month as u8);
    date.set_day(day as u8);
}

const daysInMonth: [usize; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

/// getDateFromDaynr changes a daynr to year, month and day,
/// daynr 0 is returned as date 00.00.00
// go: getDateFromDaynr L1 [M-INT]
pub(crate) fn get_date_from_daynr(daynr: usize) -> (usize, usize, usize) {
    if daynr <= 365 || daynr >= 3652500 {
        return (0, 0, 0);
    }

    let mut year = daynr * 100 / 36525;
    let temp = ((year.wrapping_sub(1) / 100 + 1) * 3) / 4;
    let mut day_of_year = daynr
        .wrapping_sub(year * 365)
        .wrapping_sub(year.wrapping_sub(1) / 4)
        .wrapping_add(temp);

    let mut days_in_year = calc_days_in_year(year as isize);
    while day_of_year > days_in_year as usize {
        day_of_year -= days_in_year as usize;
        year += 1;
        days_in_year = calc_days_in_year(year as isize);
    }

    let mut leap_day = 0usize;
    if days_in_year == 366 && day_of_year > 31 + 28 {
        day_of_year -= 1;
        if day_of_year == 31 + 28 {
            // Handle leapyears leapday.
            leap_day = 1;
        }
    }

    let mut month = 1usize;
    for days in daysInMonth {
        if day_of_year <= days {
            break;
        }
        day_of_year -= days;
        month += 1;
    }

    let day = day_of_year + leap_day;
    (year, month, day)
}

const intervalYEAR: &str = "YEAR";
const intervalQUARTER: &str = "QUARTER";
const intervalMONTH: &str = "MONTH";
const intervalWEEK: &str = "WEEK";
const intervalDAY: &str = "DAY";
const intervalHOUR: &str = "HOUR";
const intervalMINUTE: &str = "MINUTE";
const intervalSECOND: &str = "SECOND";
const intervalMICROSECOND: &str = "MICROSECOND";

// go: timestampDiff L1 [M-INT]
pub(crate) fn timestamp_diff(interval_type: &[u8], t1: CoreTime, t2: CoreTime) -> i64 {
    let (seconds, microseconds, neg) = calc_time_time_diff(t2, t1, 1);
    let mut months: usize = 0;
    let is = |s: &str| interval_type == s.as_bytes();
    if is(intervalYEAR) || is(intervalQUARTER) || is(intervalMONTH) {
        let (year_beg, year_end, month_beg, month_end, day_beg, day_end);
        let (second_beg, second_end, microsecond_beg, microsecond_end);

        if neg {
            year_beg = t2.year() as usize;
            year_end = t1.year() as usize;
            month_beg = t2.month() as usize;
            month_end = t1.month() as usize;
            day_beg = t2.day() as usize;
            day_end = t1.day() as usize;
            second_beg = (t2.hour() * 3600 + t2.minute() * 60 + t2.second()) as usize;
            second_end = (t1.hour() * 3600 + t1.minute() * 60 + t1.second()) as usize;
            microsecond_beg = t2.microsecond() as usize;
            microsecond_end = t1.microsecond() as usize;
        } else {
            year_beg = t1.year() as usize;
            year_end = t2.year() as usize;
            month_beg = t1.month() as usize;
            month_end = t2.month() as usize;
            day_beg = t1.day() as usize;
            day_end = t2.day() as usize;
            second_beg = (t1.hour() * 3600 + t1.minute() * 60 + t1.second()) as usize;
            second_end = (t2.hour() * 3600 + t2.minute() * 60 + t2.second()) as usize;
            microsecond_beg = t1.microsecond() as usize;
            microsecond_end = t2.microsecond() as usize;
        }

        // calc years
        let mut years = year_end.wrapping_sub(year_beg);
        if month_end < month_beg || (month_end == month_beg && day_end < day_beg) {
            years = years.wrapping_sub(1);
        }

        // calc months
        months = years.wrapping_mul(12);
        if month_end < month_beg || (month_end == month_beg && day_end < day_beg) {
            months = months.wrapping_add(12 - (month_beg - month_end));
        } else {
            months = months.wrapping_add(month_end - month_beg);
        }

        if day_end < day_beg
            || (day_end == day_beg
                && ((second_end < second_beg)
                    || (second_end == second_beg && microsecond_end < microsecond_beg)))
        {
            months = months.wrapping_sub(1);
        }
    }

    let neg_v: i64 = if neg { -1 } else { 1 };
    let months = months as i64;
    let seconds = seconds as i64;
    if is(intervalYEAR) {
        months / 12 * neg_v
    } else if is(intervalQUARTER) {
        months / 3 * neg_v
    } else if is(intervalMONTH) {
        months * neg_v
    } else if is(intervalWEEK) {
        seconds / secondsIn24Hour as i64 / 7 * neg_v
    } else if is(intervalDAY) {
        seconds / secondsIn24Hour as i64 * neg_v
    } else if is(intervalHOUR) {
        seconds / 3600 * neg_v
    } else if is(intervalMINUTE) {
        seconds / 60 * neg_v
    } else if is(intervalSECOND) {
        seconds * neg_v
    } else if is(intervalMICROSECOND) {
        // In MySQL difference between any two valid datetime values
        // in microseconds fits into longlong.
        (seconds * 1_000_000 + microseconds as i64) * neg_v
    } else {
        0
    }
}
