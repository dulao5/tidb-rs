//! go-file: pkg/planner/cardinality/row_count_index.go
//!
//! `GetRowCountByIndexRanges` with its pseudo path (D10.6 part 1, VS1-29: `Selectivity` calls
//! it for the indexes of a table). The estimation from real index statistics
//! (`getIndexRowCountForStatsV1` / `V2` and their helpers) is D10.6 part 2; it needs D11.1
//! part 2 and is never reached with pseudo statistics.

// Go package-level names (pseudoEqualRate, TypeLonglong, ...), mapping rule M-GONAME.
#![allow(non_upper_case_globals)]

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_expression::Column;
use tidb_sessionctx::ranger::types::Range;
use tidb_statistics::{HistColl, Index, index_stats_is_invalid};
use tidb_types::datum::{KindMaxValue, KindNull};

use super::pseudo::get_pseudo_row_count_by_index_ranges;
use super::trace::{StatsItem, check_no_trace, record_used_item_stats_status};
use crate::PlanCtx;

/// GetRowCountByIndexRanges estimates the row count by a slice of Range.
// go: GetRowCountByIndexRanges L1
pub fn get_row_count_by_index_ranges(
    sctx: &PlanCtx<'_>,
    coll: &HistColl,
    idx_id: i64,
    index_ranges: &[Range],
    idx_cols: &[Arc<Column>],
) -> Result<f64, Error> {
    check_no_trace(sctx)?;
    let vars = &*sctx.vars;
    let sc = vars.stmt_ctx();
    let idx = coll.get_idx(idx_id).map(Arc::as_ref);
    record_used_item_stats_status(sctx, StatsItem::Index(idx), coll.physical_id, idx_id);
    // Fast-path: a full-range scan over a non-MV index returns exactly RealtimeCount
    // regardless of histogram availability, so we can short-circuit before
    // IndexStatsIsInvalid — which would otherwise queue an unnecessary async
    // histogram load whenever the index stats are not fully loaded.
    if let Some(idx) = idx
        && can_skip_index_estimation(idx, index_ranges)
    {
        let (realtime_cnt, _) = coll.get_scaled_realtime_and_modify_cnt(Some(idx));
        return Ok(realtime_cnt as f64);
    }
    if index_stats_is_invalid(vars, idx, coll, idx_id) {
        let mut cols_len: isize = -1;
        if let Some(idx) = idx
            && idx.info.as_ref().expect("Index.Info").unique
        {
            cols_len = idx
                .info
                .as_ref()
                .expect("Index.Info")
                .columns
                .as_ref()
                .map_or(0, Vec::len) as isize;
        }
        return get_pseudo_row_count_by_index_ranges(
            &sc.type_ctx(),
            index_ranges,
            coll.realtime_count as f64,
            cols_len,
        );
    }
    let idx = idx.expect("valid index stats");
    let (realtime_cnt, modify_count) = coll.get_scaled_realtime_and_modify_cnt(Some(idx));
    if idx.cm_sketch.is_some() && idx.stats_ver == tidb_statistics::Version1 {
        get_index_row_count_for_stats_v1(sctx, coll, idx_id, index_ranges)
    } else {
        get_index_row_count_for_stats_v2(
            sctx,
            idx,
            coll,
            index_ranges,
            idx_cols,
            realtime_cnt,
            modify_count,
        )
    }
}

/// getIndexRowCountForStatsV1.
// glue: stub of getIndexRowCountForStatsV1, blocked-by:D10.6 (part 2: index row counts from statistics; never reached with pseudo statistics)
fn get_index_row_count_for_stats_v1(
    _sctx: &PlanCtx<'_>,
    _coll: &HistColl,
    _idx_id: i64,
    _index_ranges: &[Range],
) -> Result<f64, Error> {
    Err(blocked("D10.6", "getIndexRowCountForStatsV1"))
}

/// getIndexRowCountForStatsV2.
// glue: stub of getIndexRowCountForStatsV2, blocked-by:D10.6 (part 2: index row counts from statistics; never reached with pseudo statistics)
fn get_index_row_count_for_stats_v2(
    _sctx: &PlanCtx<'_>,
    _idx: &Index,
    _coll: &HistColl,
    _index_ranges: &[Range],
    _idx_cols: &[Arc<Column>],
    _realtime_row_count: i64,
    _modify_count: i64,
) -> Result<f64, Error> {
    Err(blocked("D10.6", "getIndexRowCountForStatsV2"))
}

/// canSkipIndexEstimation.
// go: canSkipIndexEstimation L1
fn can_skip_index_estimation(idx: &Index, index_ranges: &[Range]) -> bool {
    if idx.info.as_ref().expect("Index.Info").mv_index {
        return false;
    }
    index_ranges.iter().any(is_full_range_including_nulls)
}

/// isFullRangeIncludingNulls checks if a single range covers all values including NULLs.
/// Unlike ranger.IsFullRange, this requires the low bound to be NULL (KindNull) inclusive,
/// not KindMinNotNull and not an exclusive lower bound, so NULL rows are guaranteed to be
/// included in the count.
// go: isFullRangeIncludingNulls L1
fn is_full_range_including_nulls(ran: &Range) -> bool {
    if ran.low_val.len() != ran.high_val.len() || ran.low_val.is_empty() {
        return false;
    }
    // An exclusive bound on NULL (low) or +inf (high) would drop those endpoints
    // and shrink the range, so the fast path must not apply.
    if ran.low_exclude || ran.high_exclude {
        return false;
    }
    for i in 0..ran.low_val.len() {
        if ran.low_val[i].kind() != KindNull {
            return false;
        }
        if ran.high_val[i].kind() != KindMaxValue {
            return false;
        }
    }
    true
}
