//! go-file: pkg/planner/cardinality/row_size.go
//!
//! Average row sizes for the cost model (D10.6, VS1-29).

// Go package-level names (pseudoEqualRate, TypeLonglong, ...), mapping rule M-GONAME.
#![allow(non_upper_case_globals)]

use std::sync::Arc;

use tidb_base::gostd::math::log2;
use tidb_base::kv::{StoreType, TiFlash, TiKV};
use tidb_base::mysql::*;
use tidb_chunk::codec::{estimate_type_width, get_fixed_len_pub as get_fixed_len};
use tidb_expression::Column as ExprColumn;
use tidb_statistics::{Column, HistColl};
use tidb_tablecodec::tablecodec::RecordRowKeyLen;

use crate::PlanCtx;

/// `pseudoColSize` (row_size.go:29).
const pseudoColSize: f64 = 8.0;

/// Go's builtin `max` of two `float64`s.
// glue: Go builtin max
fn fmax(x: f64, y: f64) -> f64 {
    tidb_base::gostd::math::max(x, y)
}

/// Go's `math.Round` (half away from zero).
// glue: Go math.Round
fn round(x: f64) -> f64 {
    x.round()
}

/// GetIndexAvgRowSize computes average row size for a index scan.
// go: GetIndexAvgRowSize L1
pub fn get_index_avg_row_size(
    ctx: &PlanCtx<'_>,
    coll: &HistColl,
    cols: &[Arc<ExprColumn>],
    is_unique: bool,
) -> f64 {
    let mut size = get_avg_row_size(ctx, coll, cols, true, true);
    // tablePrefix(1) + tableID(8) + indexPrefix(2) + indexID(8)
    // Because the cols for index scan always contain the handle, so we don't add the rowID here.
    size += 19.0;
    if !is_unique {
        // add the len("_")
        size += 1.0;
    }
    size
}

/// GetTableAvgRowSize computes average row size for a table scan, exclude the index key-value pairs.
// go: GetTableAvgRowSize L1
pub fn get_table_avg_row_size(
    ctx: &PlanCtx<'_>,
    coll: &HistColl,
    cols: &[Arc<ExprColumn>],
    store_type: StoreType,
    handle_in_cols: bool,
) -> f64 {
    let mut size = get_avg_row_size(ctx, coll, cols, false, true);
    if store_type == TiKV {
        size += RecordRowKeyLen as f64;
        // The `cols` for TiKV always contain the row_id, so prefix row size subtract its length.
        size -= 8.0;
    } else if store_type == TiFlash && !handle_in_cols {
        size += 8.0 /* row_id length */;
    }
    // Avoid errors related to size less than zero
    fmax(0.0, size)
}

/// GetAvgRowSize computes average row size for given columns.
// go: GetAvgRowSize L1 [M-FMA]
pub fn get_avg_row_size(
    ctx: &PlanCtx<'_>,
    coll: &HistColl,
    cols: &[Arc<ExprColumn>],
    is_encoded_key: bool,
    is_for_scan: bool,
) -> f64 {
    let session_vars = &*ctx.vars;
    let mut size = 0.0;
    if coll.pseudo || coll.col_num() == 0 || coll.realtime_count == 0 {
        size = pseudoColSize * cols.len() as f64;
    } else {
        for col in cols {
            let col_hist = coll.get_col(col.unique_id);
            // Normally this would not happen, it is for compatibility with old version stats which
            // does not include TotColSize.
            let Some(col_hist) = col_hist.filter(|h| {
                h.is_handle
                    || h.histogram.tot_col_size != 0
                    || h.histogram.null_count == coll.realtime_count
            }) else {
                size += pseudoColSize;
                continue;
            };
            // We differentiate if the column is encoded as key or value, because the resulted size
            // is different.
            if session_vars.enable_chunk_rpc && !is_for_scan {
                size += avg_col_size_chunk_format(col_hist, coll.realtime_count);
            } else {
                size += avg_col_size(col_hist, coll.realtime_count, is_encoded_key);
            }
        }
    }
    // Avoid errors related to size less than zero
    size = fmax(0.0, size);
    if session_vars.enable_chunk_rpc && !is_for_scan {
        // Add 1/8 byte for each column's nullBitMap byte.
        // [M-FMA] row_size.go:89 (gc turns `/8` into `*0.125` and fuses it; exact either way)
        return super::madd(size, cols.len() as f64, 0.125);
    }
    // Add 1 byte for each column's flag byte. See `encode` for details.
    size + cols.len() as f64
}

/// GetAvgRowSizeDataInDiskByRows computes average row size for given columns.
// go: GetAvgRowSizeDataInDiskByRows L1
pub fn get_avg_row_size_data_in_disk_by_rows(coll: &HistColl, cols: &[Arc<ExprColumn>]) -> f64 {
    let mut size = 0.0;
    if coll.pseudo || coll.col_num() == 0 || coll.realtime_count == 0 {
        for col in cols {
            size += estimate_type_width(col.get_static_type()) as f64;
        }
    } else {
        for col in cols {
            let col_hist = coll.get_col(col.unique_id);
            // Normally this would not happen, it is for compatibility with old version stats which
            // does not include TotColSize.
            let Some(col_hist) = col_hist.filter(|h| {
                h.is_handle
                    || h.histogram.tot_col_size != 0
                    || h.histogram.null_count == coll.realtime_count
            }) else {
                size += estimate_type_width(col.get_static_type()) as f64;
                continue;
            };
            size += avg_col_size_data_in_disk_by_rows(col_hist, coll.realtime_count);
        }
    }
    // Add 8 byte for each column's size record. See `DataInDiskByRows` for details.
    fmax(0.0, size + (8 * cols.len()) as f64)
}

/// AvgColSize is the average column size of the histogram. These sizes are derived from function `encode`
/// and `Datum::ConvertTo`, so we need to update them if those 2 functions are changed.
// go: AvgColSize L1
pub fn avg_col_size(c: &Column, count: i64, is_key: bool) -> f64 {
    if count == 0 {
        return 0.0;
    }
    // Note that, if the handle column is encoded as value, instead of key, i.e,
    // when the handle column is in a unique index, the real column size may be
    // smaller than 8 because it is encoded using `EncodeVarint`. Since we don't
    // know the exact value size now, use 8 as approximation.
    if c.is_handle {
        return 8.0;
    }
    let hist_count = c.total_row_count();
    let mut not_null_ratio = 1.0;
    if hist_count > 0.0 {
        not_null_ratio = fmax(0.0, 1.0 - c.histogram.null_count as f64 / hist_count);
    }
    match c.histogram.tp.get_type() {
        TypeFloat | TypeDouble | TypeDuration | TypeDate | TypeDatetime | TypeTimestamp => {
            return 8.0 * not_null_ratio;
        }
        TypeTiny | TypeShort | TypeInt24 | TypeLong | TypeLonglong | TypeYear | TypeEnum
        | TypeBit | TypeSet
            if is_key =>
        {
            return 8.0 * not_null_ratio;
        }
        _ => {}
    }
    // Keep two decimal place.
    fmax(
        0.0,
        round(c.histogram.tot_col_size as f64 / count as f64 * 100.0) / 100.0,
    )
}

/// AvgColSizeChunkFormat is the average column size of the histogram. These sizes are derived from function `Encode`
/// and `DecodeToChunk`, so we need to update them if those 2 functions are changed.
// go: AvgColSizeChunkFormat L1
pub fn avg_col_size_chunk_format(c: &Column, count: i64) -> f64 {
    if count == 0 {
        return 0.0;
    }
    let fixed_len = get_fixed_len(&c.histogram.tp);
    if fixed_len >= 0 {
        return fixed_len as f64;
    }
    // Keep two decimal place.
    // Add 8 bytes for unfixed-len type's offsets.
    // Minus Log2(avgSize) for unfixed-len type LEN.
    let avg_size = c.histogram.tot_col_size as f64 / count as f64;
    if avg_size < 1.0 {
        return fmax(0.0, round(avg_size * 100.0) / 100.0) + 8.0;
    }
    fmax(0.0, round((avg_size - log2(avg_size)) * 100.0) / 100.0) + 8.0
}

/// AvgColSizeDataInDiskByRows is the average column size of the histogram. These sizes are derived
/// from `chunk.DataInDiskByRows` so we need to update them if those 2 functions are changed.
// go: AvgColSizeDataInDiskByRows L1
pub fn avg_col_size_data_in_disk_by_rows(c: &Column, count: i64) -> f64 {
    if count == 0 {
        return 0.0;
    }
    let hist_count = c.total_row_count();
    let mut not_null_ratio = 1.0;
    if hist_count > 0.0 {
        not_null_ratio = 1.0 - c.histogram.null_count as f64 / hist_count;
    }
    let size = get_fixed_len(&c.histogram.tp);
    if size >= 0 {
        return size as f64 * not_null_ratio;
    }
    // Keep two decimal place.
    // Minus Log2(avgSize) for unfixed-len type LEN.
    let avg_size = c.histogram.tot_col_size as f64 / count as f64;
    if avg_size < 1.0 {
        return fmax(0.0, round(avg_size * 100.0) / 100.0);
    }
    round((avg_size - log2(avg_size)) * 100.0) / 100.0
}
