//! go-file: pkg/statistics/histogram.go
//!
//! D11.1 part 1 (VS1-29): the `Histogram` structure and its read methods. Building, merging,
//! splitting ranges, protobuf conversion, out-of-range estimation and the global-stats merge
//! are D11.1 part 2 (`blocked-by:D11.1`, specs/D11.1-statistics §1.3).
//!
//! Mapping (specs/D11.1-statistics §2):
//! - `Tp *types.FieldType` is `Arc<FieldType>`: Go points at the column's `FieldType` (or a
//!   clone with the binary collation for strings); the Rust `ColumnInfo` holds its type by
//!   value (N13 §8.2), so [`new_histogram`] takes an `Arc` and the callers copy the type once.
//! - `Bounds *chunk.Chunk` is a `Chunk` held by value; Go never shares it between histograms
//!   (`Copy` deep-copies it).
//! - The sizes in `MemoryUsage` are Go's `unsafe.Sizeof` values on 64-bit targets, so the
//!   stats cache accounts the same bytes as Go.

use std::sync::Arc;

use tidb_chunk::Chunk;
use tidb_chunk::chunk::new_chunk_with_capacity;
use tidb_chunk::compare::compare as chunk_compare;
use tidb_parser::gocharset::CollationBin;
use tidb_parser::types::ETString;
use tidb_parser::types::FieldType;
use tidb_types::datum::*;

use crate::scalar::Scalar;

/// Histogram represents statistics for a column or index.
#[derive(Clone, Debug)]
pub struct Histogram {
    /// Tp.
    pub tp: Arc<FieldType>,

    // Histogram elements.
    //
    // A bucket bound is the smallest and greatest values stored in the bucket. The lower and upper bound
    // are stored in one column.
    //
    // A bucket count is the number of items stored in all previous buckets and the current bucket.
    // Bucket counts are always in increasing order.
    //
    // A bucket repeat is the number of repeats of the bucket value, it can be used to find popular values.
    /// Bounds.
    pub bounds: Chunk,
    /// Buckets.
    pub buckets: Vec<Bucket>,

    /// Used for estimating fraction of the interval [lower, upper] that lies within the [lower, value].
    /// For some types like `Int`, we do not build it because we can get them directly from `Bounds`.
    pub scalars: Vec<Scalar>,
    /// Column ID.
    pub id: i64,
    /// Number of distinct values. Note that It contains the NDV of the TopN which is excluded from histogram.
    pub ndv: i64,
    /// Number of null values.
    pub null_count: i64,
    /// LastUpdateVersion is the version that this histogram updated last time.
    pub last_update_version: u64,

    /// TotColSize is the total column size for the histogram.
    /// For unfixed-len types, it includes LEN and BYTE.
    pub tot_col_size: i64,

    /// Correlation is the statistical correlation between physical row ordering and logical ordering of
    /// the column values. This ranges from -1 to +1, and it is only valid for Column histogram, not for
    /// Index histogram.
    pub correlation: f64,
}

/// EmptyHistogramSize is the size of empty histogram, about 112 = 8*6 for int64 & float64, 24*2 for arrays, 8*2 for references.
///
/// Go's `unsafe.Sizeof(Histogram{})` on 64-bit targets (the Rust struct has another layout).
pub const EmptyHistogramSize: i64 = 112;

/// Bucket store the bucket count and repeat.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Bucket {
    /// Count is the number of items till this bucket.
    pub count: i64,
    /// Repeat is the number of times the upper-bound value of the bucket appears in the data.
    /// For example, in the range [x, y], Repeat indicates how many times y appears.
    /// It is used to estimate the row count of values equal to the upper bound of the bucket, similar to TopN.
    pub repeat: i64,
    /// NDV is the number of distinct values in the bucket.
    pub ndv: i64,
}

/// EmptyBucketSize is the size of empty bucket, 3*8=24 now.
pub const EmptyBucketSize: i64 = 24;

/// NewHistogram creates a new histogram.
///
/// `tp` is Go's `*types.FieldType` argument; for a string type Go clones it before changing
/// the collation, here `Arc::make_mut` does (the caller's `Arc` is not written).
// go: NewHistogram L1 [M-SHARE, M-POOL]
pub fn new_histogram(
    id: i64,
    ndv: i64,
    null_count: i64,
    version: u64,
    mut tp: Arc<FieldType>,
    bucket_size: usize,
    tot_col_size: i64,
) -> Histogram {
    if tp.eval_type() == ETString {
        // The histogram will store the string value's 'sort key' representation of its collation.
        // If we directly set the field type's collation to its original one. We would decode the Key representation using its collation.
        // This would cause panic. So we apply a little trick here to avoid decoding it by explicitly changing the collation to 'CollationBin'.
        Arc::make_mut(&mut tp).set_collate(CollationBin);
    }
    let bounds = new_chunk_with_capacity(&[&*tp], 2 * bucket_size);
    Histogram {
        id,
        ndv,
        null_count,
        last_update_version: version,
        tp,
        bounds,
        buckets: Vec::with_capacity(bucket_size),
        scalars: Vec::new(),
        tot_col_size,
        correlation: 0.0,
    }
}

impl Histogram {
    /// GetLower gets the lower bound of bucket `idx`.
    // go: Histogram.GetLower L1
    pub fn get_lower(&self, idx: usize) -> Datum {
        self.bounds.get_row(2 * idx).get_datum(0, &self.tp)
    }

    /// LowerToDatum gets the lower bound of bucket `idx` to datum.
    // go: Histogram.LowerToDatum L1
    pub fn lower_to_datum(&self, idx: usize, d: &mut Datum) {
        *d = self.bounds.get_row(2 * idx).get_datum(0, &self.tp);
    }

    /// GetUpper gets the upper bound of bucket `idx`.
    // go: Histogram.GetUpper L1
    pub fn get_upper(&self, idx: usize) -> Datum {
        self.bounds.get_row(2 * idx + 1).get_datum(0, &self.tp)
    }

    /// UpperToDatum gets the upper bound of bucket `idx` to datum.
    // go: Histogram.UpperToDatum L1
    pub fn upper_to_datum(&self, idx: usize, d: &mut Datum) {
        *d = self.bounds.get_row(2 * idx + 1).get_datum(0, &self.tp);
    }

    /// MemoryUsage returns the total memory usage of this Histogram.
    ///
    /// The capacities are the Rust `Vec`'s; Go's `append` grows slices by other steps, so a
    /// histogram built by appending can report a different capacity (spec §4).
    // go: Histogram.MemoryUsage L1
    pub fn memory_usage(&self) -> i64 {
        if self.buckets.is_empty() && self.scalars.is_empty() && self.bounds.capacity() == 0 {
            return 0;
        }
        EmptyHistogramSize
            + self.bounds.memory_usage()
            + self.buckets.capacity() as i64 * EmptyBucketSize
            + self.scalars.capacity() as i64 * crate::scalar::EmptyScalarSize
    }

    /// AppendBucket appends a bucket into `hg`.
    // go: Histogram.AppendBucket L1
    pub fn append_bucket(&mut self, lower: &Datum, upper: &Datum, count: i64, repeat: i64) {
        self.append_bucket_with_ndv(lower, upper, count, repeat, 0);
    }

    /// AppendBucketWithNDV appends a bucket into `hg` and set value for field `NDV`.
    // go: Histogram.AppendBucketWithNDV L1
    pub fn append_bucket_with_ndv(
        &mut self,
        lower: &Datum,
        upper: &Datum,
        count: i64,
        repeat: i64,
        ndv: i64,
    ) {
        self.buckets.push(Bucket { count, repeat, ndv });
        self.bounds.append_datum(0, lower);
        self.bounds.append_datum(0, upper);
    }

    /// Len is the number of buckets in the histogram.
    // go: Histogram.Len L1
    pub fn len(&self) -> usize {
        self.buckets.len()
    }

    /// Whether the histogram has no bucket (`Len() == 0`).
    // glue: clippy len_without_is_empty
    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }

    /// ToString gets the string representation for the histogram.
    // go: Histogram.ToString L1
    pub fn to_string(&self, idx_cols: usize) -> String {
        let mut strs: Vec<String> = Vec::with_capacity(self.len() + 1);
        if idx_cols > 0 {
            strs.push(format!("index:{} ndv:{}", self.id, self.ndv));
        } else {
            strs.push(format!(
                "column:{} ndv:{} totColSize:{}",
                self.id, self.ndv, self.tot_col_size
            ));
        }
        for i in 0..self.len() {
            strs.push(self.bucket_to_string(i, idx_cols));
        }
        strs.join("\n")
    }

    /// BucketToString change the given bucket to string format.
    ///
    /// Go logs the errors of `ValueToString` (`terror.Log`) and prints the value it got.
    // go: Histogram.BucketToString L1
    pub fn bucket_to_string(&self, bkt_id: usize, idx_cols: usize) -> String {
        let (upper_val, err) = value_to_string(&self.get_upper(bkt_id), idx_cols);
        tidb_base::terror::log(err.err().map(tidb_base::errors::trace).as_ref());
        let (lower_val, err) = value_to_string(&self.get_lower(bkt_id), idx_cols);
        tidb_base::terror::log(err.err().map(tidb_base::errors::trace).as_ref());
        format!(
            "num: {} lower_bound: {} upper_bound: {} repeats: {} ndv: {}",
            self.bucket_count(bkt_id),
            String::from_utf8_lossy(&lower_val),
            String::from_utf8_lossy(&upper_val),
            self.buckets[bkt_id].repeat,
            self.buckets[bkt_id].ndv
        )
    }

    /// EqualRowCount estimates the row count where the column equals to value.
    /// matched: return true if this returned row count is from Bucket.Repeat or bucket NDV, which is more accurate than if not.
    ///
    /// Go's `sctx` argument is only for the debug trace (spec §2, not ported).
    // go: Histogram.EqualRowCount L1
    pub fn equal_row_count(&self, value: &Datum, has_bucket_ndv: bool) -> (f64, bool) {
        let (_, bucket_idx, in_bucket, matched) = self.locate_bucket(value);
        if !in_bucket {
            return (0.0, false);
        }
        if matched {
            return (self.buckets[bucket_idx].repeat as f64, true);
        }
        if has_bucket_ndv && self.buckets[bucket_idx].ndv > 1 {
            return (
                (self.bucket_count(bucket_idx) - self.buckets[bucket_idx].repeat) as f64
                    / (self.buckets[bucket_idx].ndv - 1) as f64,
                true,
            );
        }
        (self.not_null_count() / self.ndv as f64, false)
    }

    /// GreaterRowCount estimates the row count where the column greater than value.
    /// It's deprecated. Only used for test.
    // go: Histogram.GreaterRowCount L1
    pub fn greater_row_count(&self, value: &Datum) -> f64 {
        let (hist_row_count, _) = self.equal_row_count(value, false);
        let gt_count = self.not_null_count() - self.less_row_count(value) - hist_row_count;
        go_max(0.0, gt_count)
    }

    /// LocateBucket locates where a value falls in the range of the Histogram.
    ///
    /// Return value:
    /// exceed: if the value is larger than the upper bound of the last Bucket of the Histogram.
    /// bucketIdx: assuming exceed if false, which Bucket does this value fall in (note: the range before a Bucket is also
    /// considered belong to this Bucket).
    /// inBucket: assuming exceed if false, whether this value falls in this Bucket, instead of falls between
    /// this Bucket and the previous Bucket.
    /// matchLastValue: assuming inBucket is true, if this value is the last value in this Bucket, which has a counter (Bucket.Repeat).
    ///
    /// Examples:
    /// ```text
    /// val0 |<-[bkt0]->| |<-[bkt1]->val1(last value)| val2 |<--val3--[bkt2]->| |<-[bkt3]->| val4
    /// locateBucket(val0): false, 0, false, false
    /// locateBucket(val1): false, 1, true, true
    /// locateBucket(val2): false, 2, false, false
    /// locateBucket(val3): false, 2, true, false
    /// locateBucket(val4): true, 3, false, false
    /// ```
    ///
    /// The bucket index is Go's `int`; `hg.Len() - 1` of a histogram whose bounds are not
    /// empty is never negative (a bucket appends two bounds).
    // go: Histogram.LocateBucket L1
    pub fn locate_bucket(&self, value: &Datum) -> (bool, usize, bool, bool) {
        // Empty histogram
        if self.bounds.num_rows() == 0 {
            return (true, 0, false, false);
        }
        let (index, matched) = self.bounds.lower_bound(0, value);
        // The value is larger than the max value in the histogram (exceed is true)
        if index >= self.bounds.num_rows() {
            return (true, self.len() - 1, false, false);
        }
        let bucket_idx = index / 2;
        // The value is before this bucket
        if index % 2 == 0 && !matched {
            return (false, bucket_idx, false, false);
        }
        // The value matches the last value in this bucket
        // case 1: The LowerBound()'s return value tells us the value matches an upper bound of a bucket
        // case 2: We compare and find that the value is equal to the upper bound of this bucket. This might happen when
        //           the bucket's lower bound is equal to its upper bound.
        if (index % 2 == 1 && matched)
            || chunk_compare(self.bounds.get_row(bucket_idx * 2 + 1), 0, value) == 0
        {
            return (false, bucket_idx, true, true);
        }
        // The value is in the bucket and isn't the last value in this bucket
        (false, bucket_idx, true, false)
    }

    /// LessRowCountWithBktIdx estimates the row count where the column less than value.
    ///
    /// The bucket index is Go's `int` (`hg.Len()-1` is `-1` for an empty histogram whose
    /// bounds are not empty, which cannot be built).
    // go: Histogram.LessRowCountWithBktIdx L1 [M-FMA]
    pub fn less_row_count_with_bkt_idx(&self, value: &Datum) -> (f64, usize) {
        // All the values are null.
        if self.bounds.num_rows() == 0 {
            return (0.0, 0);
        }
        let (exceed, bucket_idx, in_bucket, matched) = self.locate_bucket(value);
        if exceed {
            return (self.not_null_count(), self.len() - 1);
        }
        let mut pre_count = 0.0;
        if bucket_idx > 0 {
            pre_count = self.buckets[bucket_idx - 1].count as f64;
        }
        if !in_bucket {
            return (pre_count, bucket_idx);
        }
        let (cur_count, cur_repeat) = (
            self.buckets[bucket_idx].count as f64,
            self.buckets[bucket_idx].repeat as f64,
        );
        if matched {
            return (cur_count - cur_repeat, bucket_idx);
        }
        // [M-FMA] histogram.go:555
        (
            madd(
                pre_count,
                self.calc_fraction(bucket_idx, value),
                cur_count - cur_repeat - pre_count,
            ),
            bucket_idx,
        )
    }

    /// LessRowCount estimates the row count where the column less than value.
    // go: Histogram.LessRowCount L1
    pub fn less_row_count(&self, value: &Datum) -> f64 {
        let (result, _) = self.less_row_count_with_bkt_idx(value);
        result
    }

    /// BetweenRowCount estimates the row count where column greater or equal to a and less than b.
    // go: Histogram.BetweenRowCount L1
    pub fn between_row_count(&self, a: &Datum, b: &Datum) -> f64 {
        let less_count_a = self.less_row_count(a);
        let less_count_b = self.less_row_count(b);
        let range_est = less_count_b - less_count_a;
        let (low_equal, _) = self.equal_row_count(a, false);
        let ndv_avg = self.not_null_count() / self.ndv as f64;
        // If values fall in the same bucket, we may underestimate the fractional result. So estimate the low value (a) as an equals, and
        // estimate the high value as the default (because the input high value may be "larger" than the true high value). The range should
        // not be less than both the low+high - or the lesser of the estimate for the individual range of a or b is used as a bound.
        if range_est < go_max(low_equal, ndv_avg) && self.ndv > 0 {
            let result = go_min(less_count_b, self.not_null_count() - less_count_a);
            return go_min(result, low_equal + ndv_avg);
        }
        range_est
    }

    /// TotalRowCount returns the total count of this histogram.
    // go: Histogram.TotalRowCount L1
    pub fn total_row_count(&self) -> f64 {
        self.not_null_count() + self.null_count as f64
    }

    /// AbsRowCountDifference returns the absolute difference between the realtime row count
    /// and the histogram's total row count, representing data changes since the last ANALYZE.
    // go: Histogram.AbsRowCountDifference L1
    pub fn abs_row_count_difference(&self, realtime_row_count: i64) -> f64 {
        (realtime_row_count as f64 - self.total_row_count()).abs()
    }

    /// NotNullCount indicates the count of non-null values in column histogram and single-column index histogram,
    /// for multi-column index histogram, since we cannot define null for the row, we treat all rows as non-null, that means,
    /// notNullCount would return same value as TotalRowCount for multi-column index histograms.
    // go: Histogram.NotNullCount L1
    pub fn not_null_count(&self) -> f64 {
        if self.is_empty() {
            return 0.0;
        }
        self.buckets[self.len() - 1].count as f64
    }

    /// BucketCount returns the count of the bucket with index idx.
    // go: Histogram.BucketCount L1
    pub fn bucket_count(&self, idx: usize) -> i64 {
        if idx == 0 {
            return self.buckets[0].count;
        }
        self.buckets[idx].count - self.buckets[idx - 1].count
    }

    /// Copy deep copies the histogram.
    ///
    /// Go's `newHist := *hg` shares `Scalars`; the Rust copy clones it (it is never written
    /// after `PreCalculateScalar`).
    // go: Histogram.Copy L1
    pub fn copy(&self) -> Histogram {
        Histogram {
            tp: Arc::clone(&self.tp),
            bounds: self.bounds.copy_construct(),
            buckets: {
                let mut b = Vec::with_capacity(self.buckets.len());
                b.extend_from_slice(&self.buckets);
                b
            },
            scalars: self.scalars.clone(),
            id: self.id,
            ndv: self.ndv,
            null_count: self.null_count,
            last_update_version: self.last_update_version,
            tot_col_size: self.tot_col_size,
            correlation: self.correlation,
        }
    }
}

/// ValueToString converts a possible encoded value to a formatted string. If the value is encoded, then
/// idxCols equals to number of origin values, else idxCols is 0.
///
/// Go's `vars` argument is nil at the only ported caller (`BucketToString`) and
/// `idxColumnTypes` is nil there too; both are dropped.
// go: ValueToString L1 [M-ERRVAL]
pub fn value_to_string(value: &Datum, idx_cols: usize) -> (Vec<u8>, Result<(), tidb_base::Error>) {
    if idx_cols == 0 {
        return match value.to_string() {
            Ok(s) => (s, Ok(())),
            Err(e) => (Vec::new(), Err(e)),
        };
    }
    // Ignore the error and treat remaining part that cannot decode successfully as bytes.
    let (mut decoded_vals, remained, _err) =
        tidb_codec::codec::decode_range(value.get_bytes(), idx_cols, None, None);
    // Ignore err explicit to pass errcheck.
    if !remained.is_empty() {
        decoded_vals.push(new_bytes_datum(remained.to_vec()));
    }
    match datums_to_string(&decoded_vals, true) {
        Ok(s) => (s, Ok(())),
        Err(e) => (Vec::new(), Err(e)),
    }
}

/// `z + x * y` the way Go gc compiles it at the sites of `go_fma_sites.tsv` marked fused on
/// arm64, ppc64le, s390x, riscv64, loong64 and not on amd64 (GOAMD64=v1): histogram.go:555
/// (mapping rule `M-FMA`). Rust never contracts by itself, so the unfused form is Go's amd64.
// glue: M-FMA helper
#[inline(always)]
pub(crate) fn madd(z: f64, x: f64, y: f64) -> f64 {
    #[cfg(any(
        target_arch = "aarch64",
        all(target_arch = "powerpc64", target_endian = "little"),
        target_arch = "riscv64",
        target_arch = "loongarch64"
    ))]
    {
        x.mul_add(y, z)
    }
    #[cfg(not(any(
        target_arch = "aarch64",
        all(target_arch = "powerpc64", target_endian = "little"),
        target_arch = "riscv64",
        target_arch = "loongarch64"
    )))]
    {
        z + x * y
    }
}

/// Go's `math.Max` (NaN and signed zeros as Go).
// glue: Go math.Max
pub(crate) fn go_max(x: f64, y: f64) -> f64 {
    tidb_base::gostd::math::max(x, y)
}

/// Go's `math.Min`.
// glue: Go math.Min
pub(crate) fn go_min(x: f64, y: f64) -> f64 {
    tidb_base::gostd::math::min(x, y)
}

/// The status of the column / index stats.
pub const AllLoaded: isize = 0;
/// AllEvicted indicates all statistics are evicted
pub const AllEvicted: isize = 1;

/// StatsLoadedStatus indicates the status of statistics
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StatsLoadedStatus {
    pub(crate) stats_initialized: bool,
    pub(crate) evicted_status: isize,
}

/// NewStatsFullLoadStatus returns the status that the column/index fully loaded
// go: NewStatsFullLoadStatus L1
pub fn new_stats_full_load_status() -> StatsLoadedStatus {
    StatsLoadedStatus {
        stats_initialized: true,
        evicted_status: AllLoaded,
    }
}

/// NewStatsAllEvictedStatus returns the status that only loads count/nullCount/NDV and doesn't load CMSketch/TopN/Histogram.
/// When we load table stats, column stats is in AllEvicted status by default. CMSketch/TopN/Histogram of column is only
/// loaded when we really need column stats.
// go: NewStatsAllEvictedStatus L1
pub fn new_stats_all_evicted_status() -> StatsLoadedStatus {
    StatsLoadedStatus {
        stats_initialized: true,
        evicted_status: AllEvicted,
    }
}

impl StatsLoadedStatus {
    /// Copy copies the status
    // go: StatsLoadedStatus.Copy L1
    pub fn copy(&self) -> StatsLoadedStatus {
        StatsLoadedStatus {
            stats_initialized: self.stats_initialized,
            evicted_status: self.evicted_status,
        }
    }

    /// IsStatsInitialized indicates whether the column/index's statistics was loaded from storage before.
    /// Note that `IsStatsInitialized` only can be set in initializing
    // go: StatsLoadedStatus.IsStatsInitialized L1
    pub fn is_stats_initialized(&self) -> bool {
        self.stats_initialized
    }

    /// IsLoadNeeded indicates whether it needs load statistics during LoadNeededHistograms or sync stats
    /// If the column/index was loaded and any statistics of it is evicting, it also needs re-load statistics.
    // go: StatsLoadedStatus.IsLoadNeeded L1
    pub fn is_load_needed(&self) -> bool {
        if self.stats_initialized {
            return self.evicted_status > AllLoaded;
        }
        // If statsInitialized is false, it means there is no stats for the column/index in the storage.
        // Hence, we don't need to trigger the task of loading the column/index stats.
        false
    }

    /// IsEssentialStatsLoaded indicates whether the essential statistics is loaded.
    /// If the column/index was loaded, and at least histogram and topN still exists, the necessary statistics is still loaded.
    // go: StatsLoadedStatus.IsEssentialStatsLoaded L1
    pub fn is_essential_stats_loaded(&self) -> bool {
        self.stats_initialized && (self.evicted_status < AllEvicted)
    }

    /// IsAllEvicted indicates whether all the stats got evicted or not.
    // go: StatsLoadedStatus.IsAllEvicted L1
    pub fn is_all_evicted(&self) -> bool {
        self.stats_initialized && self.evicted_status >= AllEvicted
    }

    /// IsFullLoad indicates whether the stats are full loaded
    // go: StatsLoadedStatus.IsFullLoad L1
    pub fn is_full_load(&self) -> bool {
        self.stats_initialized && self.evicted_status == AllLoaded
    }

    /// StatusToString gets the string info of StatsLoadedStatus
    // go: pkg/statistics/column.go StatsLoadedStatus.StatusToString L1
    pub fn status_to_string(&self) -> &'static str {
        if !self.stats_initialized {
            return "unInitialized";
        }
        match self.evicted_status {
            AllLoaded => "allLoaded",
            AllEvicted => "allEvicted",
            _ => "unknown",
        }
    }
}

/// Version0 is the state that no statistics is actually collected, only the meta info.(the total count and the average col size)
pub const Version0: i64 = 0;
/// Version1 maintains the statistics in the following way.
/// Column stats: CM Sketch is built in TiKV using full data. Histogram is built from samples. TopN is extracted from CM Sketch.
/// TopN + CM Sketch represent all data. Histogram also represents all data.
/// Index stats: CM Sketch and Histogram is built in TiKV using full data. TopN is extracted from histogram. Then values covered by TopN is removed from CM Sketch.
/// TopN + CM Sketch represent all data. Histogram also represents all data.
/// Int PK column stats is always Version1 because it only has histogram built from full data.
/// Fast analyze is always Version1 currently.
pub const Version1: i64 = 1;
/// Version2 maintains the statistics in the following way.
/// Column stats: CM Sketch is not used. TopN and Histogram are built from samples. TopN + Histogram represent all data.
/// Index stats: CM SKetch is not used. TopN and Histograms are built from samples.
/// Then values covered by TopN is removed from Histogram. TopN + Histogram represent all data.
/// Both Column and Index's NDVs are collected by full scan.
pub const Version2: i64 = 2;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_histogram_reads() {
        let hg = new_histogram(
            1,
            0,
            0,
            0,
            Arc::new(tidb_types::field_type::new_field_type(
                tidb_base::mysql::TypeLonglong,
            )),
            0,
            0,
        );
        assert_eq!(hg.len(), 0);
        assert_eq!(hg.total_row_count(), 0.0);
        assert_eq!(hg.less_row_count(&new_int_datum(1)), 0.0);
        assert_eq!(hg.memory_usage(), 0);
        assert_eq!(hg.locate_bucket(&new_int_datum(1)), (true, 0, false, false));
    }
}
