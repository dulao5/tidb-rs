//! Unit tests of `helper.go` (block D08.12): the current-time helpers read the statement
//! time of the context, which the govec vectors (generated at a fixed `now` of their own)
//! cannot share with the replay, and `boolToInt64`.

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::gostd::time as gotime;
use tidb_base::gostd::time::{fixed_zone, utc};
use tidb_base::mysql::{TypeDate, TypeDatetime, TypeDuration, TypeTimestamp};
use tidb_expression::helper::{
    bool_to_int64, get_stmt_timestamp, get_time_current_time_stamp, get_time_current_timestamp,
};
use tidb_sessionctx::exprstatic as es;

/// 2024-01-02 03:04:05.123456789 UTC.
fn stmt_time() -> gotime::Time {
    gotime::unix(1_704_164_645, 123_456_789).in_loc(&utc())
}

fn ctx(loc: Arc<gotime::Location>, now: Result<gotime::Time, Error>) -> Arc<es::EvalContext> {
    let f: es::TimeFn = Arc::new(move || now.clone());
    es::new_eval_context(vec![es::with_location(Some(loc)), es::with_current_time(f)])
}

#[test]
fn bool_to_int64_values() {
    assert_eq!(bool_to_int64(true), 1);
    assert_eq!(bool_to_int64(false), 0);
}

#[test]
fn stmt_timestamp_is_the_context_time() {
    let c = ctx(utc(), Ok(stmt_time()));
    let t = get_stmt_timestamp(&*c).unwrap();
    assert_eq!((t.unix(), t.nanosecond()), (1_704_164_645, 123_456_789));
}

#[test]
fn current_timestamp_truncates_and_converts() {
    // UTC+8: the statement time is converted into the session time zone for DATETIME,
    // TIMESTAMP and DATE, and truncated (not rounded) to fsp.
    let c = ctx(fixed_zone("UTC+8", 8 * 3600), Ok(stmt_time()));
    for (tp, fsp, want) in [
        (TypeDatetime, 3, "2024-01-02 11:04:05.123"),
        (TypeDatetime, 0, "2024-01-02 11:04:05"),
        (TypeTimestamp, 6, "2024-01-02 11:04:05.123456"),
        (TypeDate, 0, "2024-01-02"),
    ] {
        let d = get_time_current_timestamp(&*c, tp, fsp).unwrap();
        assert_eq!(
            String::from_utf8(d.get_mysql_time().string()).unwrap(),
            want,
            "tp {tp} fsp {fsp}"
        );
        let (t, err) = get_time_current_time_stamp(&*c, tp, fsp);
        assert!(err.is_ok());
        assert_eq!(t.tp(), tp);
    }
    // Other types are not converted: they keep the zone of the statement time, which the
    // static context already returns in the session zone (exprstatic timeOnce.getTime).
    let (t, err) = get_time_current_time_stamp(&*c, TypeDuration, 0);
    assert!(err.is_ok());
    assert_eq!(t.hour(), 11);
}

#[test]
fn current_timestamp_error_keeps_the_zero_value() {
    let err = Error::plain("no time");
    let c = ctx(utc(), Err(err));
    let (t, r) = get_time_current_time_stamp(&*c, TypeDatetime, 2);
    assert!(r.is_err());
    assert!(t.is_zero());
    assert_eq!(t.tp(), TypeDatetime);
    assert_eq!(t.fsp(), 2);
    assert!(get_time_current_timestamp(&*c, TypeDatetime, 2).is_err());
}
