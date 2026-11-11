//! go-file: pkg/sessionctx/variable/slow_log.go
//!
//! The warning collection and the threshold matchers of the slow log (D05.2, PR F). The
//! slow log writer itself (`SessionVars.SlowLogFormat`, `writeSlowLogItem`,
//! `kvExecDetailFormat`) and the `Setter`/`Match` accessors built by
//! `makeExecDetailAccessor`/`makeKVExecDetailAccessor` belong to D14.2 (tracker rules): they
//! need the statement's `UsedStatsInfo` (D11), client-go's `util.ExecDetails` from the
//! request context, `ProcessInfo`'s user and connection info (D04.1/D03.1) and the RU and
//! CPU details. The rule parser is in [`super::slow_log_rules`].

use tidb_base::contextutil::SQLWarn;
use tidb_base::errors;
use tidb_base::terror;

use super::slow_log_rules::Threshold;
use crate::stmtctx::StatementContext;

/// JSONSQLWarnForSlowLog helps to print the SQLWarn through the slow log in JSON format.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JSONSQLWarnForSlowLog {
    /// Level.
    pub level: String,
    /// Message (Go string bytes).
    pub message: Vec<u8>,
    /// IsExtra means this SQL Warn is expected to be recorded only under some conditions
    /// (like in EXPLAIN) and should haven't been recorded as a warning now, but we recorded
    /// it anyway to help diagnostics.
    pub is_extra: bool,
}

// go: extractMsgFromSQLWarn L1
fn extract_msg_from_sql_warn(sql_warn: &SQLWarn) -> Vec<u8> {
    // CollectWarningsForSlowLog guarantees sqlWarn is not nil.
    let warn = errors::cause(&sql_warn.err);
    if warn.is_normalized() {
        let sql_err = terror::to_sql_error(warn);
        return sql_err.message;
    }
    warn.error()
}

/// CollectWarningsForSlowLog collects warnings from statement context for slow log output.
// go: CollectWarningsForSlowLog L1
pub fn collect_warnings_for_slow_log(stmt_ctx: &StatementContext) -> Vec<JSONSQLWarnForSlowLog> {
    let warnings = stmt_ctx.get_warnings();
    let extra_warnings = stmt_ctx.get_extra_warnings();
    let mut res = Vec::with_capacity(warnings.len() + extra_warnings.len());
    for w in &warnings {
        res.push(JSONSQLWarnForSlowLog {
            level: w.level.to_string(),
            message: extract_msg_from_sql_warn(w),
            is_extra: false,
        });
    }
    for w in &extra_warnings {
        res.push(JSONSQLWarnForSlowLog {
            level: w.level.to_string(),
            message: extract_msg_from_sql_warn(w),
            is_extra: true,
        });
    }
    res
}

/// A value a threshold is compared with (`MatchEqual[T comparable]` / `matchGE[T
/// numericComparable]` instantiate on `int`, `int64`, `uint64`, `float64`, `bool`,
/// `string`). Go's `int` and `int64` are distinct types: a threshold parsed as `int64`
/// never equals an `int` value.
// glue: the type parameter of MatchEqual / matchGE
#[derive(Clone, Debug, PartialEq)]
pub enum MatchValue<'a> {
    /// Go `int`.
    Int(i64),
    /// Go `int64`.
    I64(i64),
    /// Go `uint64`.
    U64(u64),
    /// Go `float64`.
    F64(f64),
    /// Go `bool`.
    Bool(bool),
    /// Go `string`.
    Str(&'a str),
}

/// MatchEqual compares a value `v` with a threshold and returns true if they are equal.
// Note: the type assertion `threshold.(T)` is the variant match; parsed thresholds are
// never Go `int`.
// go: MatchEqual L1
pub fn match_equal(threshold: &Threshold, v: &MatchValue<'_>) -> bool {
    match (threshold, v) {
        (Threshold::I64(t), MatchValue::I64(v)) => v == t,
        (Threshold::U64(t), MatchValue::U64(v)) => v == t,
        (Threshold::F64(t), MatchValue::F64(v)) => v == t,
        (Threshold::Bool(t), MatchValue::Bool(v)) => v == t,
        (Threshold::Str(t), MatchValue::Str(v)) => *v == t.as_str(),
        _ => false,
    }
}

/// matchGE: `v >= threshold` when the threshold has `v`'s type.
// go: matchGE L1
pub fn match_ge(threshold: &Threshold, v: &MatchValue<'_>) -> bool {
    match (threshold, v) {
        (Threshold::I64(t), MatchValue::I64(v)) => v >= t,
        (Threshold::U64(t), MatchValue::U64(v)) => v >= t,
        (Threshold::F64(t), MatchValue::F64(v)) => v >= t,
        _ => false,
    }
}

/// uint64FromNonNegative converts a signed value to uint64 if it is non-negative.
// go: uint64FromNonNegative L1
pub fn uint64_from_non_negative(v: i64) -> Option<u64> {
    if v < 0 {
        return None;
    }
    Some(v as u64)
}

/// matchZero: whether the numeric threshold is zero.
// Note: Go's `int` case is unreachable: parsed thresholds are never `int`.
// go: matchZero L1
pub fn match_zero(threshold: &Threshold) -> bool {
    match threshold {
        Threshold::U64(v) => *v == 0,
        Threshold::I64(v) => *v == 0,
        Threshold::F64(v) => *v == 0.0,
        _ => false,
    }
}

/// ParseString returns the input string as-is.
// go: ParseString L1
pub fn parse_string(v: &str) -> Result<Threshold, tidb_base::Error> {
    Ok(Threshold::Str(v.to_owned()))
}
