//! go-file: pkg/util/paging/paging.go
//!
//! Whole-file port of a D00.3 util file (constitution §4.4), first needed by the
//! coprocessor client (D07.2).
//!
//! A paging request may be separated into multi requests if there are more data than a
//! page. The paging size grows from min to max. See https://github.com/pingcap/tidb/issues/36328
//! e.g. a paging request scans over range (r1, r200), it requires 128 rows in the first
//! batch, if it's not drained, then the paging size grows, the new range is calculated like
//! (r100, r200), then send a request again. Compare with the common unary request, paging
//! request allows early access of data, it offers a streaming-like way processing data.

// Package-level consts keep their Go names (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use tidb_base::gostd::math;

pub const MinPagingSize: u64 = 128;
const maxPagingSizeShift: u64 = 7;
const pagingSizeGrow: u64 = 2;
pub const MaxPagingSize: u64 = 50000;
const pagingGrowingSum: u64 = ((2 << maxPagingSizeShift) - 1) * MinPagingSize;
pub const Threshold: u64 = 960;

/// GrowPagingSize grows the paging size and ensures it does not exceed MaxPagingSize
// go: GrowPagingSize L1 [M-INT]
pub fn grow_paging_size(mut size: u64, mut maxv: u64) -> u64 {
    if maxv < MaxPagingSize {
        // Defensive programing, for example, call with max = 0.
        // max should never less than MaxPagingSize.
        // Otherwise the session variable maybe wrong, or the distsql request does not obey
        // the session variable setting.
        maxv = MaxPagingSize;
    }

    // Go's shift drops the high bit.
    size = size.wrapping_shl(1);
    if size > maxv {
        return maxv;
    }
    size
}

/// CalculateSeekCnt calculates the seek count from expect count
// go: CalculateSeekCnt L1 [M-INT, M-F2I]
pub fn calculate_seek_cnt(expect_cnt: u64) -> f64 {
    if expect_cnt == 0 {
        return 0.0;
    }
    if expect_cnt > pagingGrowingSum {
        // if the expectCnt is larger than pagingGrowingSum, calculate the seekCnt for the
        // excess.
        return (8
            + (expect_cnt
                .wrapping_sub(pagingGrowingSum)
                .wrapping_add(MaxPagingSize)
                .wrapping_sub(1))
                / MaxPagingSize) as f64;
    }
    if expect_cnt > MinPagingSize {
        // if the expectCnt is less than pagingGrowingSum,
        // calculate the seekCnt(number of terms) from the sum of a geometric progression.
        // expectCnt = minPagingSize * (pagingSizeGrow ^ seekCnt - 1) / (pagingSizeGrow - 1)
        // simplify (pagingSizeGrow ^ seekCnt - 1) to pagingSizeGrow ^ seekCnt, we can infer
        // that seekCnt = log((pagingSizeGrow - 1) * expectCnt / minPagingSize) / log(pagingSizeGrow)
        // The quotient is in (0, 8): int() truncates it like Go.
        return 1.0
            + ((math::log(((pagingSizeGrow - 1) * expect_cnt) as f64 / MinPagingSize as f64)
                / math::log(pagingSizeGrow as f64)) as isize) as f64;
    }
    1.0
}
