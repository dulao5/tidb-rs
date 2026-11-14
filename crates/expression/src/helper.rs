//! go-file: pkg/expression/helper.go
//!
//! The current-time helpers of `helper.go` (block D08.12) and `boolToInt64`. `GetTimeValue`
//! is owned by D06.4 (column default values, D06.4 tasks P1-1); its `-1`-style unary
//! operator case needs `EvalSimpleAst` (the expression rewriter, D09.2) and is a boundary
//! stub. Not ported here (registered exemptions in the tracker's `cast` domain):
//! `IsValidCurrentTimestampExpr` (DDL only) and `pickRandomLocationForTest` (`intest` only).

use tidb_base::Error;
use tidb_base::gostd::math::pow10;
use tidb_base::gostd::time as gotime;
use tidb_base::mysql::{TypeDate, TypeDatetime, TypeTimestamp};
use tidb_types::core_time::ZeroCoreTime;
use tidb_types::datum::Datum;
use tidb_types::time::{Time, from_go_time, new_time};

use std::sync::Arc;

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

/// The dynamic argument of `GetTimeValue` (Go `v any`): the cases of its type switch.
pub enum TimeValueArg<'a> {
    /// `string` (a column's default value).
    Str(&'a str),
    /// `*driver.ValueExpr`.
    ValueExpr(&'a tidb_parser::ast::ValueExpr),
    /// `*ast.FuncCallExpr`.
    FuncCall(&'a tidb_parser::ast::FuncCallExpr),
    /// `*ast.UnaryOperationExpr`.
    UnaryOperation(&'a tidb_parser::ast::UnaryOperationExpr),
    /// Any other dynamic type (the `default` case).
    Other,
}

/// GetTimeValue gets the time value with type tp.
// go: GetTimeValue L1 [M-ERRVAL]
pub fn get_time_value(
    ctx: &dyn crate::BuildContext,
    v: TimeValueArg<'_>,
    tp: u8,
    fsp: isize,
    explicit_tz: Option<Arc<gotime::Location>>,
) -> Result<Datum, Error> {
    use tidb_base::mysql::{DefaultCollationName, TypeLonglong};
    use tidb_parser::ast::Datum as AstDatum;
    use tidb_parser::ast::funcs::{CurrentDate, CurrentTimestamp};
    use tidb_types::time::{ZeroDatetimeStr, from_date, parse_time, parse_time_from_num};

    let mut d = Datum::default();
    let mut tc = ctx.get_eval_ctx().type_ctx();
    if let Some(tz) = explicit_tz {
        tc = tc.with_location(tz);
    }

    let value = match v {
        TimeValueArg::Str(x) => {
            let lower_x = tidb_base::gostd::to_lower(x);
            if lower_x == CurrentTimestamp {
                let (value, err) = get_time_current_time_stamp(&*ctx.get_eval_ctx(), tp, fsp);
                err?;
                value
            } else if lower_x == CurrentDate {
                let (mut value, err) = get_time_current_time_stamp(&*ctx.get_eval_ctx(), tp, fsp);
                err?;
                let (yy, mm, dd) = (value.year(), value.month(), value.day());
                let truncated = from_date(yy, mm, dd, 0, 0, 0, 0);
                value.set_core_time(truncated);
                value
            } else if lower_x == ZeroDatetimeStr {
                let (value, err) = parse_time_from_num(&tc, 0, tp, fsp);
                tidb_base::terror::log(err.as_ref().err());
                value
            } else {
                let (value, err) = parse_time(&tc, x.as_bytes(), tp, fsp);
                err?;
                value
            }
        }
        TimeValueArg::ValueExpr(x) => match &x.datum {
            AstDatum::Str(s) => {
                let (value, err) = parse_time(&tc, s.as_bytes(), tp, fsp);
                err?;
                value
            }
            AstDatum::Int(i) => {
                let (value, err) = parse_time_from_num(&tc, *i, tp, fsp);
                err?;
                value
            }
            AstDatum::Bool(b) => {
                let (value, err) = parse_time_from_num(&tc, i64::from(*b), tp, fsp);
                err?;
                value
            }
            AstDatum::Null => return Ok(d),
            _ => return Err(crate::errors::errDefaultValue.clone()),
        },
        TimeValueArg::FuncCall(x) => {
            if x.fn_name.l() == CurrentTimestamp || x.fn_name.l() == CurrentDate {
                d.set_string(
                    x.fn_name.l().to_uppercase().into_bytes(),
                    DefaultCollationName,
                );
                return Ok(d);
            }
            return Err(crate::errors::errDefaultValue.clone());
        }
        TimeValueArg::UnaryOperation(_) => {
            // support some expression, like `-1`: EvalSimpleAst, then a cast to BIGINT.
            // glue: stub, blocked-by:D09.2 (GetTimeValue's UnaryOperationExpr case: EvalSimpleAst)
            let _ = TypeLonglong;
            return Err(tidb_base::blocked::blocked(
                "D09.2",
                "a unary-operator time default value (expression.EvalSimpleAst)",
            ));
        }
        TimeValueArg::Other => return Ok(d),
    };
    d.set_mysql_time(value);
    Ok(d)
}
