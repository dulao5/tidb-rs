//! go-file: pkg/expression/helper.go
//!
//! The current-time helpers of `helper.go` (block D08.12) and `boolToInt64`. Not ported here
//! (registered exemptions in the tracker's `cast` domain): `IsValidCurrentTimestampExpr`
//! (DDL only), `GetTimeValue` (needs the parser driver's `ValueExpr` and `EvalSimpleAst`;
//! owned by D06.4, column default values) and `pickRandomLocationForTest` (`intest` only).

use tidb_base::Error;
use tidb_base::gostd::math::pow10;
use tidb_base::gostd::time as gotime;
use tidb_base::mysql::{TypeDate, TypeDatetime, TypeTimestamp};
use tidb_types::core_time::ZeroCoreTime;
use tidb_types::datum::Datum;
use tidb_types::time::{Time, from_go_time, new_time};

use crate::context::{EvalContext, location};

/// boolToInt64 is 1 for true and 0 for false.
// go: boolToInt64 L1
pub fn bool_to_int64(v: bool) -> i64 {
    if v { 1 } else { 0 }
}

/// GetTimeCurrentTimestamp is used for generating a timestamp for some special cases: cast
/// null value to timestamp type with not null flag.
// go: GetTimeCurrentTimestamp L1 [M-ERRVAL]
pub fn get_time_current_timestamp(
    ctx: &dyn EvalContext,
    tp: u8,
    fsp: isize,
) -> Result<Datum, Error> {
    let (t, err) = get_time_current_time_stamp(ctx, tp, fsp);
    err?;
    let mut d = Datum::default();
    d.set_mysql_time(t);
    Ok(d)
}

/// The statement's current time truncated to `fsp`, as a `tp` value in the session time
/// zone. On error the value is returned too, as in Go (M-ERRVAL): the zero time of `tp` and
/// `fsp` when the statement time fails, the half-converted value when the time-zone
/// conversion fails.
// go: getTimeCurrentTimeStamp L1 [M-ERRVAL, M-F2I]
pub fn get_time_current_time_stamp(
    ctx: &dyn EvalContext,
    tp: u8,
    fsp: isize,
) -> (Time, Result<(), Error>) {
    let mut value = new_time(ZeroCoreTime, tp, fsp);
    let default_time = match get_stmt_timestamp(ctx) {
        Ok(t) => t,
        Err(e) => return (value, Err(e)),
    };
    // Go: time.Duration(math.Pow10(9-fsp)) * time.Nanosecond; fsp is in [-1, 6] here, so the
    // float is an exact integer in range and the conversion does not depend on the target.
    let unit = pow10(9 - fsp) as gotime::Duration * gotime::NANOSECOND;
    value.set_core_time(from_go_time(&default_time.truncate(unit)));
    if (tp == TypeTimestamp || tp == TypeDatetime || tp == TypeDate)
        && let Err(e) = value.convert_time_zone(&default_time.location(), &location(ctx))
    {
        return (value, Err(e));
    }
    (value, Ok(()))
}

/// If the timestamp session variable is set, use it as the current time, otherwise use the
/// cached time: during one SQL statement the "current_time" should be the same.
// go: getStmtTimestamp L2 (the intest random location and the injectNow failpoint are test-only and not ported)
pub fn get_stmt_timestamp(ctx: &dyn EvalContext) -> Result<gotime::Time, Error> {
    ctx.current_time()
}
