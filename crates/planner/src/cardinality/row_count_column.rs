//! go-file: pkg/planner/cardinality/row_count_column.go
//!
//! Row counts of column ranges (D10.6, VS1-29). The pseudo path is ported; the estimation
//! from real statistics (`GetColumnRowCount`, `equalRowCountOnColumn`,
//! `betweenRowCountOnColumn`: TopN, CM sketch, out-of-range estimation) needs D11.1 part 2
//! and is a boundary stub. tidb-rs has only pseudo statistics before D11.2, so the stub is
//! never reached by a real statement (spec §1.2).
//!
//! Go's `init` registers these functions in `statistics`' function variables to break an
//! import cycle; Rust has no such cycle (not ported, functions.tsv `not_ported`).

// Go package-level names (pseudoEqualRate, TypeLonglong, ...), mapping rule M-GONAME.
#![allow(non_upper_case_globals)]

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_sessionctx::ranger::types::Range;
use tidb_statistics::{Column, HistColl, Table, column_stats_is_invalid};
use tidb_types::datum::{Datum, KindInt64};

use super::pseudo::{
    get_pseudo_row_count_by_column_ranges, get_pseudo_row_count_by_signed_int_ranges,
    get_pseudo_row_count_by_unsigned_int_ranges, pseudoBetweenRate, pseudoEqualRate,
    pseudoLessRate,
};
use super::trace::{StatsItem, check_no_trace, record_used_item_stats_status};
use crate::PlanCtx;

/// GetRowCountByColumnRanges estimates the row count by a slice of Range.
// go: GetRowCountByColumnRanges L1
pub fn get_row_count_by_column_ranges(
    sctx: &PlanCtx<'_>,
    coll: &HistColl,
    col_unique_id: i64,
    col_ranges: &[Range],
) -> Result<f64, Error> {
    check_no_trace(sctx)?;
    let vars = &*sctx.vars;
    let sc = vars.stmt_ctx();
    let c = coll.get_col(col_unique_id).map(Arc::as_ref);
    let mut col_info_id = col_unique_id;
    if !coll.unique_id2_col_info_id.is_empty() {
        col_info_id = coll
            .unique_id2_col_info_id
            .get(&col_unique_id)
            .copied()
            .unwrap_or(0);
    }
    record_used_item_stats_status(sctx, StatsItem::Column(c), coll.physical_id, col_info_id);
    if column_stats_is_invalid(c, Some(vars), coll, col_unique_id) {
        return get_pseudo_row_count_by_column_ranges(
            &sc.type_ctx(),
            coll.realtime_count as f64,
            col_ranges,
            0,
        );
    }
    let c = c.expect("valid column stats");
    get_column_row_count(
        sctx,
        c,
        col_ranges,
        coll.realtime_count,
        coll.modify_count,
        false,
    )
}

/// GetRowCountByIntColumnRanges estimates the row count by a slice of IntColumnRange.
// go: GetRowCountByIntColumnRanges L1
pub fn get_row_count_by_int_column_ranges(
    sctx: &PlanCtx<'_>,
    coll: &HistColl,
    col_unique_id: i64,
    int_ranges: &[Range],
) -> Result<f64, Error> {
    check_no_trace(sctx)?;
    let vars = &*sctx.vars;
    let c = coll.get_col(col_unique_id).map(Arc::as_ref);
    let mut col_info_id = col_unique_id;
    if !coll.unique_id2_col_info_id.is_empty() {
        col_info_id = coll
            .unique_id2_col_info_id
            .get(&col_unique_id)
            .copied()
            .unwrap_or(0);
    }
    record_used_item_stats_status(sctx, StatsItem::Column(c), coll.physical_id, col_info_id);
    if column_stats_is_invalid(c, Some(vars), coll, col_unique_id) {
        if int_ranges.is_empty() {
            return Ok(0.0);
        }
        let result = if int_ranges[0].low_val[0].kind() == KindInt64 {
            get_pseudo_row_count_by_signed_int_ranges(int_ranges, coll.realtime_count as f64)
        } else {
            get_pseudo_row_count_by_unsigned_int_ranges(int_ranges, coll.realtime_count as f64)
        };
        return Ok(result);
    }
    let c = c.expect("valid column stats");
    get_column_row_count(
        sctx,
        c,
        int_ranges,
        coll.realtime_count,
        coll.modify_count,
        true,
    )
}

/// equalRowCountOnColumn estimates the row count by a slice of Range and a Datum.
// glue: stub of equalRowCountOnColumn, blocked-by:D11.1 (part 2: TopN.QueryTopN, CMSketch, Histogram.OutOfRange; never reached with pseudo statistics)
fn equal_row_count_on_column(
    _sctx: &PlanCtx<'_>,
    _c: &Column,
    _val: &Datum,
    _encoded_val: &[u8],
    _realtime_row_count: i64,
    _modify_count: i64,
) -> Result<f64, Error> {
    Err(blocked("D11.1", "equalRowCountOnColumn"))
}

/// GetColumnRowCount estimates the row count by a slice of Range.
// glue: stub of GetColumnRowCount, blocked-by:D11.1 (part 2: TopN / CMSketch / out-of-range estimation; never reached with pseudo statistics)
pub fn get_column_row_count(
    _sctx: &PlanCtx<'_>,
    _c: &Column,
    _ranges: &[Range],
    _realtime_row_count: i64,
    _modify_count: i64,
    _pk_is_handle: bool,
) -> Result<f64, Error> {
    Err(blocked("D11.1", "GetColumnRowCount"))
}

/// betweenRowCountOnColumn estimates the row count for interval [l, r).
///
/// Go returns a `float64`; the stub returns the boundary error.
// glue: stub of betweenRowCountOnColumn, blocked-by:D11.1 (part 2: TopN.BetweenCount; never reached with pseudo statistics)
fn between_row_count_on_column(
    _sctx: &PlanCtx<'_>,
    _c: &Column,
    _l: &Datum,
    _r: &Datum,
    _low_encoded: &[u8],
    _high_encoded: &[u8],
) -> Result<f64, Error> {
    Err(blocked("D11.1", "betweenRowCountOnColumn"))
}

// functions below are mainly for testing.

/// ColumnGreaterRowCount estimates the row count where the column greater than value.
// go: ColumnGreaterRowCount L1
pub fn column_greater_row_count(sctx: &PlanCtx<'_>, t: &Table, value: &Datum, col_id: i64) -> f64 {
    let c = t.get_col(col_id).map(Arc::as_ref);
    if column_stats_is_invalid(c, Some(&*sctx.vars), &t.hist_coll, col_id) {
        return t.realtime_count as f64 / pseudoLessRate;
    }
    let c = c.expect("valid column stats");
    c.greater_row_count(value) * c.get_increase_factor(t.realtime_count)
}

/// columnLessRowCount estimates the row count where the column less than value. Note that null values are not counted.
// go: columnLessRowCount L1
pub fn column_less_row_count(sctx: &PlanCtx<'_>, t: &Table, value: &Datum, col_id: i64) -> f64 {
    let c = t.get_col(col_id).map(Arc::as_ref);
    if column_stats_is_invalid(c, Some(&*sctx.vars), &t.hist_coll, col_id) {
        return t.realtime_count as f64 / pseudoLessRate;
    }
    let c = c.expect("valid column stats");
    c.less_row_count(value) * c.get_increase_factor(t.realtime_count)
}

/// columnBetweenRowCount estimates the row count where column greater or equal to a and less than b.
// go: columnBetweenRowCount L1
pub fn column_between_row_count(
    sctx: &PlanCtx<'_>,
    t: &Table,
    a: &Datum,
    b: &Datum,
    col_id: i64,
) -> Result<f64, Error> {
    let sc = sctx.vars.stmt_ctx();
    let c = t.get_col(col_id).map(Arc::as_ref);
    if column_stats_is_invalid(c, Some(&*sctx.vars), &t.hist_coll, col_id) {
        return Ok(t.realtime_count as f64 / pseudoBetweenRate);
    }
    let c = c.expect("valid column stats");
    let mut a_encoded = Vec::new();
    let err =
        tidb_codec::codec::encode_key(&sc.time_zone(), &mut a_encoded, std::slice::from_ref(a));
    if let Err(e) = err {
        sc.handle_error(e)?;
    }
    let mut b_encoded = Vec::new();
    let err =
        tidb_codec::codec::encode_key(&sc.time_zone(), &mut b_encoded, std::slice::from_ref(b));
    if let Err(e) = err {
        sc.handle_error(e)?;
    }
    let mut count = between_row_count_on_column(sctx, c, a, b, &a_encoded, &b_encoded)?;
    if a.is_null() {
        count += c.histogram.null_count as f64;
    }
    Ok(count * c.get_increase_factor(t.realtime_count))
}

/// ColumnEqualRowCount estimates the row count where the column equals to value.
// go: ColumnEqualRowCount L1
pub fn column_equal_row_count(
    sctx: &PlanCtx<'_>,
    t: &Table,
    value: &Datum,
    col_id: i64,
) -> Result<f64, Error> {
    let c = t.get_col(col_id).map(Arc::as_ref);
    if column_stats_is_invalid(c, Some(&*sctx.vars), &t.hist_coll, col_id) {
        return Ok(t.realtime_count as f64 / pseudoEqualRate);
    }
    let c = c.expect("valid column stats");
    let sc = sctx.vars.stmt_ctx();
    let mut encoded_val = Vec::new();
    let err = tidb_codec::codec::encode_key(
        &sc.time_zone(),
        &mut encoded_val,
        std::slice::from_ref(value),
    );
    if let Err(e) = err {
        sc.handle_error(e)?;
    }
    let result = equal_row_count_on_column(
        sctx,
        c,
        value,
        &encoded_val,
        t.realtime_count,
        t.modify_count,
    )?;
    Ok(result * c.get_increase_factor(t.realtime_count))
}
