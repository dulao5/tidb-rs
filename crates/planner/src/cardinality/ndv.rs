//! go-file: pkg/planner/cardinality/ndv.go
//!
//! NDV estimation (D10.6, VS1-29).

// Go package-level names (pseudoEqualRate, TypeLonglong, ...), mapping rule M-GONAME.
#![allow(non_upper_case_globals)]

use std::sync::Arc;

use tidb_expression::{Column as ExprColumn, Schema};
use tidb_statistics::{Column, Table};

use crate::property::StatsInfo;

/// `distinctFactor` (ndv.go:27).
const distinctFactor: f64 = 0.8;

/// EstimateColumnNDV computes estimated NDV of specified column using the original
/// histogram of `DataSource` which is retrieved from storage(not the derived one).
// go: EstimateColumnNDV L1
pub fn estimate_column_ndv(tbl: &Table, col_id: i64) -> f64 {
    let hist = tbl.get_col(col_id);
    let mut ndv;
    if let Some(hist) = hist
        && hist.is_stats_initialized()
    {
        ndv = hist.histogram.ndv as f64;
        // TODO: a better way to get the total row count derived from the last analyze.
        let analyze_count = get_total_row_count(tbl, hist);
        if analyze_count > 0 {
            let factor = tbl.realtime_count as f64 / analyze_count as f64;
            ndv *= factor;
        }
    } else {
        ndv = tbl.realtime_count as f64 * distinctFactor;
    }
    ndv
}

/// getTotalRowCount returns the total row count, which is obtained when collecting colHist.
// go: getTotalRowCount L1 [M-F2I]
fn get_total_row_count(stats_tbl: &Table, col_hist: &Column) -> i64 {
    use tidb_base::gostd::conv::f64_to_i64;
    if col_hist.is_full_load() {
        return f64_to_i64(col_hist.total_row_count()); // [M-F2I]
    }
    // If colHist is not fully loaded, we may still get its total row count from other index/column stats.
    let mut tot_count: i64 = 0;
    let mut stop = false;
    stats_tbl.for_each_index_immutable(|_, idx| {
        if idx.is_full_load()
            && idx.histogram.last_update_version == col_hist.histogram.last_update_version
        {
            tot_count = f64_to_i64(idx.total_row_count()); // [M-F2I]
            stop = true;
            return true;
        }
        false
    });
    if stop {
        return tot_count;
    }
    stats_tbl.for_each_column_immutable(|_, col| {
        if col.is_full_load()
            && col.histogram.last_update_version == col_hist.histogram.last_update_version
        {
            tot_count = f64_to_i64(col.total_row_count()); // [M-F2I]
            return true;
        }
        false
    });
    tot_count
}

/// EstimateColsNDVWithMatchedLen returns the NDV of a couple of columns.
/// If the columns match any GroupNDV maintained by child operator, we can get an accurate NDV.
/// Otherwise, we simply return the max NDV among the columns, which is a lower bound.
// go: EstimateColsNDVWithMatchedLen L1
pub fn estimate_cols_ndv_with_matched_len(
    cols: &[Arc<ExprColumn>],
    schema: &Schema,
    profile: &StatsInfo,
) -> (f64, usize) {
    let mut ndv = 1.0;
    if let Some(group_ndv) = StatsInfo::get_group_ndv4_cols(Some(profile), cols) {
        return (
            tidb_base::gostd::math::max(group_ndv.ndv, ndv),
            group_ndv.cols.len(),
        );
    }
    let Some(indices) = schema.columns_indices(cols) else {
        log::error!(
            "column not found in schema, columns: {:?}, schema: {}",
            cols.iter().map(|c| c.unique_id).collect::<Vec<_>>(),
            String::from_utf8_lossy(&schema.to_string())
        );
        return (ndv, 1);
    };
    for idx in indices {
        // It is a very naive estimation.
        let col = &schema.columns[idx as usize];
        ndv = tidb_base::gostd::math::max(
            ndv,
            profile.col_ndvs.get(&col.unique_id).copied().unwrap_or(0.0),
        );
    }
    (ndv, 1)
}

/// EstimateColsDNVWithMatchedLenFromUniqueIDs is similar to EstimateColsDNVWithMatchedLen, but it receives UniqueIDs instead of Columns.
// go: EstimateColsDNVWithMatchedLenFromUniqueIDs L1
pub fn estimate_cols_dnv_with_matched_len_from_unique_ids(
    ids: &[i64],
    schema: &Schema,
    profile: &StatsInfo,
) -> (f64, usize) {
    let mut cols = Vec::with_capacity(ids.len());
    for &id in ids {
        cols.push(Arc::new(ExprColumn {
            unique_id: id,
            ..ExprColumn::default()
        }));
    }
    estimate_cols_ndv_with_matched_len(&cols, schema, profile)
}
