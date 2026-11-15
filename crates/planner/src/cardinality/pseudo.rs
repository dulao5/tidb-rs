//! go-file: pkg/planner/cardinality/pseudo.go
//!
//! The pseudo estimation: the rates of a table without statistics (D10.6, VS1-29). Every
//! constant is compared with Go v8.5.8 in specs/D10.6-cardinality §3.

// Go package-level names (pseudoEqualRate, TypeLonglong, ...), mapping rule M-GONAME.
#![allow(non_upper_case_globals)]

use std::collections::HashSet;

use tidb_base::Error;
use tidb_base::mysql::has_uni_key_flag;
use tidb_expression::{Expr, ExprRef, ast_funcs as ast};
use tidb_sessionctx::ranger::types::Range;
use tidb_statistics::{HistColl, Table, column_stats_is_invalid, index_stats_is_invalid};
use tidb_types::context::Context as TypeContext;
use tidb_types::datum::{KindMaxValue, KindMinNotNull, KindNull};

use super::selectivity::{get_constant_column_id, unknownColumnID};
use crate::PlanCtx;

/// `pseudoEqualRate` (pseudo.go:31).
pub(crate) const pseudoEqualRate: f64 = 1000.0;
/// `pseudoLessRate` (pseudo.go:32).
pub(crate) const pseudoLessRate: f64 = 3.0;
/// `pseudoBetweenRate` (pseudo.go:33).
pub(crate) const pseudoBetweenRate: f64 = 40.0;

/// If one condition can't be calculated, we will assume that the selectivity of this condition is 0.8.
pub(crate) const selectionFactor: f64 = 0.8;

/// PseudoAvgCountPerValue gets a pseudo average count if histogram not exists.
// go: PseudoAvgCountPerValue L1
pub fn pseudo_avg_count_per_value(t: &Table) -> f64 {
    t.realtime_count as f64 / pseudoEqualRate
}

/// pseudoSelectivity.
///
/// Go looks the column up with `coll.GetCol(col.ID)` although the `HistColl` of a query is
/// keyed by `UniqueID` (spec §5 item 1); ported as is.
// go: pseudoSelectivity L1
pub(crate) fn pseudo_selectivity(sctx: &PlanCtx<'_>, coll: &HistColl, exprs: &[ExprRef]) -> f64 {
    let vars = &*sctx.vars;
    let mut min_factor = selectionFactor;
    let mut col_exists: HashSet<&str> = HashSet::new();
    for expr in exprs {
        let Expr::ScalarFunction(fun) = &**expr else {
            continue;
        };
        let col_id = get_constant_column_id(fun.get_args());
        if col_id == unknownColumnID {
            continue;
        }
        column_stats_is_invalid(None, Some(vars), coll, col_id);
        match fun.func_name.l.as_str() {
            ast::EQ | ast::NullEQ | ast::In => {
                min_factor = tidb_base::gostd::math::min(min_factor, 1.0 / pseudoEqualRate);
                let Some(col) = coll.get_col(col_id) else {
                    continue;
                };
                let info = col.info.as_ref().expect("Column.Info");
                col_exists.insert(info.name.l.as_str());
                if has_uni_key_flag(info.get_flag()) {
                    return 1.0 / coll.realtime_count as f64;
                }
            }
            ast::GE | ast::GT | ast::LE | ast::LT => {
                min_factor = tidb_base::gostd::math::min(min_factor, 1.0 / pseudoLessRate);
                // FIXME: To resolve the between case.
            }
            _ => {}
        }
    }
    if col_exists.is_empty() {
        return min_factor;
    }
    // use the unique key info
    let mut has_unique_key = false;
    coll.for_each_index_immutable(|_, idx| {
        let info = idx.info.as_ref().expect("Index.Info");
        let mut unique = true;
        let mut first_match = false;
        for col in info.columns.as_deref().unwrap_or(&[]) {
            if !col_exists.contains(col.name.l.as_str()) {
                unique = false;
                break;
            }
            first_match = true;
        }
        if first_match {
            // This might trigger the statistics load.
            index_stats_is_invalid(vars, None, coll, idx.histogram.id);
        }
        if info.unique && unique {
            has_unique_key = true;
            return true;
        }
        false
    });
    if has_unique_key {
        return 1.0 / coll.realtime_count as f64;
    }
    min_factor
}

/// getPseudoRowCountBySignedIntRanges.
///
/// `high-low` wraps around like Go's int64 subtraction (`M-INT`).
// go: getPseudoRowCountBySignedIntRanges L1 [M-INT]
pub(crate) fn get_pseudo_row_count_by_signed_int_ranges(
    int_ranges: &[Range],
    table_row_count: f64,
) -> f64 {
    let mut row_count = 0.0;
    for rg in int_ranges {
        let mut low = rg.low_val[0].get_int64();
        if rg.low_val[0].kind() == KindNull || rg.low_val[0].kind() == KindMinNotNull {
            low = i64::MIN;
        }
        let mut high = rg.high_val[0].get_int64();
        if rg.high_val[0].kind() == KindMaxValue {
            high = i64::MAX;
        }
        let mut cnt = if low == i64::MIN && high == i64::MAX {
            table_row_count
        } else if low == i64::MIN || high == i64::MAX {
            table_row_count / pseudoLessRate
        } else if low == high {
            1.0 // When primary key is handle, the equal row count is at most one.
        } else {
            table_row_count / pseudoBetweenRate
        };
        let diff = high.wrapping_sub(low);
        if diff > 0 && cnt > diff as f64 {
            cnt = diff as f64;
        }
        row_count += cnt;
    }
    if row_count > table_row_count {
        row_count = table_row_count;
    }
    row_count
}

/// getPseudoRowCountByUnsignedIntRanges.
// go: getPseudoRowCountByUnsignedIntRanges L1
pub(crate) fn get_pseudo_row_count_by_unsigned_int_ranges(
    int_ranges: &[Range],
    table_row_count: f64,
) -> f64 {
    let mut row_count = 0.0;
    for rg in int_ranges {
        let mut cnt;
        let mut low = rg.low_val[0].get_uint64();
        if rg.low_val[0].kind() == KindNull || rg.low_val[0].kind() == KindMinNotNull {
            low = 0;
        }
        let mut high = rg.high_val[0].get_uint64();
        if rg.high_val[0].kind() == KindMaxValue {
            high = u64::MAX;
        }
        if low == 0 && high == u64::MAX {
            cnt = table_row_count;
        } else if low == 0 || high == u64::MAX {
            cnt = table_row_count / pseudoLessRate;
        } else if low == high {
            cnt = 1.0; // When primary key is handle, the equal row count is at most one.
        } else {
            cnt = table_row_count / pseudoBetweenRate;
        }
        if high > low && cnt > (high - low) as f64 {
            cnt = (high - low) as f64;
        }
        row_count += cnt;
    }
    if row_count > table_row_count {
        row_count = table_row_count;
    }
    row_count
}

/// getPseudoRowCountByIndexRanges.
///
/// `colsLen` is Go's `int` (`-1` when the index is not unique).
// go: getPseudoRowCountByIndexRanges L1
pub(crate) fn get_pseudo_row_count_by_index_ranges(
    tc: &TypeContext,
    index_ranges: &[Range],
    table_row_count: f64,
    cols_len: isize,
) -> Result<f64, Error> {
    if table_row_count == 0.0 {
        return Ok(0.0);
    }
    let mut total_count = 0.0;
    for index_range in index_ranges {
        let mut count = table_row_count;
        let mut i = index_range.prefix_equal_len(tc)? as isize;
        if i == cols_len && !index_range.low_exclude && !index_range.high_exclude {
            total_count += 1.0;
            continue;
        }
        if i >= index_range.low_val.len() as isize {
            i = index_range.low_val.len() as isize - 1;
        }
        // Go indexes LowVal[i] with i = -1 for an empty range and panics.
        let col_idx = usize::try_from(i).expect("index out of range [-1]");
        let row_count = get_pseudo_row_count_by_column_ranges(
            tc,
            table_row_count,
            std::slice::from_ref(index_range),
            col_idx,
        )?;
        count = count / table_row_count * row_count;
        // If the condition is a = 1, b = 1, c = 1, d = 1, we think every a=1, b=1, c=1 only filtrate 1/100 data,
        // so as to avoid collapsing too fast.
        for _ in 0..col_idx {
            count /= 100.0;
        }
        total_count += count;
    }
    if total_count > table_row_count {
        total_count = table_row_count / 3.0;
    }
    Ok(total_count)
}

/// getPseudoRowCountByColumnRanges calculate the row count by the ranges if there's no statistics information for this column.
// go: getPseudoRowCountByColumnRanges L1
pub(crate) fn get_pseudo_row_count_by_column_ranges(
    tc: &TypeContext,
    table_row_count: f64,
    column_ranges: &[Range],
    col_idx: usize,
) -> Result<f64, Error> {
    let mut row_count = 0.0;
    for ran in column_ranges {
        if ran.low_val[col_idx].kind() == KindNull && ran.high_val[col_idx].kind() == KindMaxValue {
            row_count += table_row_count;
        } else if ran.low_val[col_idx].kind() == KindMinNotNull {
            let null_count = table_row_count / pseudoEqualRate;
            if ran.high_val[col_idx].kind() == KindMaxValue {
                row_count += table_row_count - null_count;
            } else {
                let less_count = table_row_count / pseudoLessRate;
                row_count += less_count - null_count;
            }
        } else if ran.high_val[col_idx].kind() == KindMaxValue {
            row_count += table_row_count / pseudoLessRate;
        } else {
            let (compare, err) =
                ran.low_val[col_idx].compare(tc, &ran.high_val[col_idx], ran.collators[col_idx]);
            err?;
            if compare == 0 {
                row_count += table_row_count / pseudoEqualRate;
            } else {
                row_count += table_row_count / pseudoBetweenRate;
            }
        }
    }
    if row_count > table_row_count {
        row_count = table_row_count;
    }
    Ok(row_count)
}
