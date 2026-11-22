//! go-file: pkg/planner/cardinality/cross_estimation.go
//!
//! The row count of a table scan under a limit (block D10.6, the part FindBestTask reads,
//! specs/D10.2-find-best-task §3). The correlation-based cross estimation needs real
//! statistics (histograms with correlations): the new stack only has the pseudo table
//! (VS1 plan §3.2), on which Go skips it; with real statistics it is a boundary.

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_statistics::Table;

use crate::planctx::PlanCtx;
use crate::property::StatsInfo;
use crate::util::AccessPath;

/// AdjustRowCountForTableScanByLimit will adjust the row count for table scan by limit.
/// For a query like `select pk from t using index(primary) where pk > 10 limit 1`, the row count of the table scan
/// should be adjusted by the limit number 1, because only one row is returned.
///
/// Returns a `Result` only for the boundary of the cross estimation.
// go: AdjustRowCountForTableScanByLimit L2 (Result for the boundary of the cross estimation)
pub fn adjust_row_count_for_table_scan_by_limit(
    sctx: &PlanCtx<'_>,
    ds_stats_info: &StatsInfo,
    ds_table_stats: &StatsInfo,
    ds_statistic_table: &Table,
    path: &AccessPath,
    expected_cnt: f64,
    desc: bool,
) -> Result<f64, Error> {
    let mut row_count = path.count_after_access;
    if expected_cnt < ds_stats_info.row_count {
        let selectivity = ds_stats_info.row_count / path.count_after_access;
        let uniform_est = path.count_after_access.min(expected_cnt / selectivity);

        let (corr_est, ok, corr) = cross_estimate_table_row_count(
            sctx,
            ds_stats_info,
            ds_table_stats,
            ds_statistic_table,
            path,
            expected_cnt,
            desc,
        )?;
        if ok {
            // TODO: actually, before using this count as the estimated row count of table scan, we need additionally
            // check if count < row_count(first_region | last_region), and use the larger one since we build one copTask
            // for one region now, so even if it is `limit 1`, we have to scan at least one region in table scan.
            // Currently, we can use `tikvrpc.CmdDebugGetRegionProperties` interface as `getSampRegionsRowCount()` does
            // to get the row count in a region, but that result contains MVCC old version rows, so it is not that accurate.
            // Considering that when this scenario happens, the execution time is close between IndexScan and TableScan,
            // we do not add this check temporarily.

            // to reduce risks of correlation adjustment, use the maximum between uniformEst and corrEst
            row_count = uniform_est.max(corr_est);
        } else {
            let abs = corr.abs();
            if abs < 1.0 {
                let correlation_factor = (1.0 - abs).powf(sctx.vars.correlation_exp_factor as f64);
                row_count = path
                    .count_after_access
                    .min(uniform_est / correlation_factor);
            }
        }
    }
    Ok(row_count)
}

/// crossEstimateTableRowCount estimates row count of table scan using histogram of another
/// column which is in TableFilters and has high order correlation with handle column. Go
/// skips it on the pseudo table, without table filters or with the correlation adjustment
/// off; the correlation search over real statistics (`getMostCorrCol4Handle`,
/// `crossEstimateRowCount`) is a boundary that only real statistics reach.
// go: crossEstimateTableRowCount L2 (the estimation over real statistics is a boundary: D10.6 part 2)
fn cross_estimate_table_row_count(
    sctx: &PlanCtx<'_>,
    _ds_stats_info: &StatsInfo,
    _ds_table_stats: &StatsInfo,
    ds_statistic_table: &Table,
    path: &AccessPath,
    _expected_cnt: f64,
    _desc: bool,
) -> Result<(f64, bool, f64), Error> {
    if ds_statistic_table.pseudo
        || path.table_filters.is_empty()
        || !sctx.vars.enable_correlation_adjustment
    {
        return Ok((0.0, false, 0.0));
    }
    // col, corr := getMostCorrCol4Handle(path.TableFilters, dsStatisticTable, threshold);
    // return crossEstimateRowCount(...): real statistics, which the new stack does not have
    // (VS1 plan §3.2).
    // glue: blocked-by:D10.6 (getMostCorrCol4Handle / crossEstimateRowCount: the cross estimation over real statistics, part 2 of the cardinality block)
    Err(blocked(
        "D10.6",
        "the cross estimation of a table scan over real statistics",
    ))
}
