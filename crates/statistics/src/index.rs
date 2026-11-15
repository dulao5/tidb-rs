//! go-file: pkg/statistics/index.go
//!
//! D11.1 part 1 (VS1-29): the index statistics and their read methods. `Info` is the same
//! `Arc<IndexInfo>` as the table's `TableInfo.Indices[i]` (N13 `M-SHARE`); the embedded
//! `Histogram` / `StatsLoadedStatus` are fields as in [`crate::column`].

use std::sync::Arc;

use tidb_model::{IndexInfo, TableItemID};
use tidb_sessionctx::variable::session::SessionVars;
use tidb_types::datum::{Datum, new_bytes_datum};

use crate::asyncload;
use crate::cmsketch::{CMSketch, TopN, top_n_total_count};
use crate::fmsketch::FMSketch;
use crate::histogram::{AllEvicted, AllLoaded, Histogram, StatsLoadedStatus, Version0, Version2};
use crate::table::{HistColl, IndexMemUsage};

/// Index represents an index histogram.
#[derive(Clone, Debug)]
pub struct Index {
    /// LastAnalyzePos.
    pub last_analyze_pos: Datum,
    /// CMSketch.
    pub cm_sketch: Option<Arc<CMSketch>>,
    /// TopN.
    pub top_n: Option<Arc<TopN>>,
    /// FMSketch.
    pub fm_sketch: Option<Arc<FMSketch>>,
    /// Info.
    pub info: Option<Arc<IndexInfo>>,
    /// The embedded `Histogram`.
    pub histogram: Histogram,
    /// The embedded `StatsLoadedStatus`.
    pub stats_loaded_status: StatsLoadedStatus,
    /// StatsVer is the version of the current stats, used to maintain compatibility
    pub stats_ver: i64,
    /// Flag.
    pub flag: i64,
    /// PhysicalID is the physical table id,
    /// or it could possibly be -1, which means "stats not available".
    /// The -1 case could happen in a pseudo stats table, and in this case, this stats should not trigger stats loading.
    pub physical_id: i64,
}

impl Index {
    /// An index with the given histogram and zero values elsewhere (Go's composite literal).
    // glue: Go composite literal of statistics.Index
    pub fn with_histogram(histogram: Histogram) -> Index {
        Index {
            last_analyze_pos: Datum::default(),
            cm_sketch: None,
            top_n: None,
            fm_sketch: None,
            info: None,
            histogram,
            stats_loaded_status: StatsLoadedStatus::default(),
            stats_ver: 0,
            flag: 0,
            physical_id: 0,
        }
    }

    /// Copy copies the index.
    // go: Index.Copy L1 [M-SHARE]
    // CMSketch / FMSketch have no values in part 1: the code after their methods is unreachable.
    #[allow(unreachable_code)]
    pub fn copy(&self) -> Index {
        let mut nc = Index::with_histogram(self.histogram.copy());
        nc.physical_id = self.physical_id;
        nc.flag = self.flag;
        nc.stats_ver = self.stats_ver;
        nc.last_analyze_pos = self.last_analyze_pos.clone();
        if let Some(c) = &self.cm_sketch {
            nc.cm_sketch = Some(Arc::new(c.copy()));
        }
        if let Some(t) = &self.top_n {
            nc.top_n = Some(Arc::new(t.copy()));
        }
        if let Some(f) = &self.fm_sketch {
            nc.fm_sketch = Some(Arc::new(f.copy()));
        }
        if let Some(info) = &self.info {
            nc.info = Some(Arc::new(IndexInfo::clone(info)));
        }
        nc.stats_loaded_status = self.stats_loaded_status.copy();
        nc
    }

    /// ItemID implements TableCacheItem
    // go: Index.ItemID L1
    pub fn item_id(&self) -> i64 {
        self.info.as_ref().expect("Index.Info").id
    }

    /// IsAllEvicted indicates whether all stats evicted
    ///
    /// Go's nil receiver is `idx: None`.
    // go: Index.IsAllEvicted L1
    pub fn is_all_evicted(idx: Option<&Index>) -> bool {
        match idx {
            None => true,
            Some(idx) => {
                idx.stats_loaded_status.stats_initialized
                    && idx.stats_loaded_status.evicted_status >= AllEvicted
            }
        }
    }

    /// GetEvictedStatus returns the evicted status
    // go: Index.GetEvictedStatus L1
    pub fn get_evicted_status(&self) -> isize {
        self.stats_loaded_status.evicted_status
    }

    /// isStatsInitialized; also Go's promoted `StatsLoadedStatus.IsStatsInitialized` (the
    /// same field).
    // go: Index.isStatsInitialized L1
    pub fn is_stats_initialized(&self) -> bool {
        self.stats_loaded_status.stats_initialized
    }

    /// GetStatsVer returns the version of the current stats
    // go: Index.GetStatsVer L1
    pub fn get_stats_ver(&self) -> i64 {
        self.stats_ver
    }

    /// IsCMSExist returns whether CMSketch exists.
    // go: Index.IsCMSExist L1
    pub fn is_cms_exist(&self) -> bool {
        self.cm_sketch.is_some()
    }

    /// IsEvicted returns whether index statistics got evicted
    // go: Index.IsEvicted L1
    pub fn is_evicted(&self) -> bool {
        self.stats_loaded_status.evicted_status != AllLoaded
    }

    /// String.
    ///
    /// Go reads `len(idx.Info.Columns)` (a nil `Info` panics); so does this.
    // go: Index.String L1 [M-NAME]
    #[allow(clippy::inherent_to_string)] // M-NAME: Go String()
    pub fn to_string(&self) -> String {
        let n = self
            .info
            .as_ref()
            .expect("Index.Info")
            .columns
            .as_ref()
            .map_or(0, Vec::len);
        self.histogram.to_string(n)
    }

    /// TotalRowCount returns the total count of this index.
    // go: Index.TotalRowCount L1
    pub fn total_row_count(&self) -> f64 {
        if self.stats_ver >= Version2 {
            return self.histogram.total_row_count()
                + top_n_total_count(self.top_n.as_deref()) as f64;
        }
        self.histogram.total_row_count()
    }

    /// EvictAllStats evicts all stats
    /// Note that this function is only used for test
    ///
    /// Go writes the index in place; here the caller holds it mutably (a shared index is
    /// copied first with `Arc::make_mut`, N13 rule 2).
    // go: Index.EvictAllStats L1
    pub fn evict_all_stats(&mut self) {
        self.histogram.buckets = Vec::new();
        self.cm_sketch = None;
        self.top_n = None;
        self.stats_loaded_status.evicted_status = AllEvicted;
    }

    /// MemoryUsage returns the total memory usage of a Histogram and CMSketch in Index.
    /// We ignore the size of other metadata in Index.
    // go: Index.MemoryUsage L1
    // CMSketch / FMSketch have no values in part 1: the code after their methods is unreachable.
    #[allow(unreachable_code)]
    pub fn memory_usage(&self) -> IndexMemUsage {
        let mut index_mem_usage = IndexMemUsage {
            index_id: self.info.as_ref().expect("Index.Info").id,
            ..IndexMemUsage::default()
        };
        let hist_mem_usage = self.histogram.memory_usage();
        index_mem_usage.histogram_mem_usage = hist_mem_usage;
        let mut sum = hist_mem_usage;
        if let Some(c) = &self.cm_sketch {
            let cm_sketch_mem_usage = c.memory_usage();
            index_mem_usage.cm_sketch_mem_usage = cm_sketch_mem_usage;
            sum += cm_sketch_mem_usage;
        }
        if let Some(t) = &self.top_n {
            let topn_mem_usage = t.memory_usage();
            index_mem_usage.top_n_mem_usage = topn_mem_usage;
            sum += topn_mem_usage;
        }
        index_mem_usage.total_mem_usage = sum;
        index_mem_usage
    }

    /// QueryBytes is used to query the count of specified bytes.
    ///
    /// Go hashes `d` (murmur3) first for the CM sketch; part 1 has no CM sketch (the hash is
    /// not computed) and no TopN query (both D11.1 part 2), so an index with a TopN is a
    /// boundary error (the `Result` is for it).
    // go: Index.QueryBytes L2 (Result for the part-2 TopN boundary; M-F2I)
    // CMSketch / FMSketch have no values in part 1: the code after their methods is unreachable.
    #[allow(unreachable_code)]
    pub fn query_bytes(&self, d: &[u8]) -> Result<u64, tidb_base::Error> {
        if self.top_n.is_some() {
            return Err(tidb_base::blocked::blocked("D11.1", "TopN.QueryTopN"));
        }
        if let Some(c) = &self.cm_sketch {
            match **c {}
        }
        let (v, _) = self
            .histogram
            .equal_row_count(&new_bytes_datum(d.to_vec()), self.stats_ver >= Version2);
        Ok(tidb_base::gostd::conv::f64_to_u64(v)) // [M-F2I]
    }

    /// GetIncreaseFactor get the increase factor to adjust the final estimated count when the table is modified.
    // go: Index.GetIncreaseFactor L1
    pub fn get_increase_factor(&self, realtime_row_count: i64) -> f64 {
        let column_count = self.total_row_count();
        if column_count == 0.0 {
            return 1.0;
        }
        realtime_row_count as f64 / column_count
    }

    /// GetHistogram returns the histogram for this index.
    // go: Index.GetHistogram L1
    pub fn get_histogram(&self) -> &Histogram {
        &self.histogram
    }

    /// GetTopN returns the TopN for this index.
    // go: Index.GetTopN L1
    pub fn get_top_n(&self) -> Option<&TopN> {
        self.top_n.as_deref()
    }

    /// IsAnalyzed indicates whether the index is analyzed.
    // go: Index.IsAnalyzed L1
    pub fn is_analyzed(&self) -> bool {
        self.stats_ver != Version0
    }

    /// The promoted `StatsLoadedStatus.IsFullLoad`.
    // glue: M-SELF (promoted method of the embedded StatsLoadedStatus)
    pub fn is_full_load(&self) -> bool {
        self.stats_loaded_status.is_full_load()
    }
}

/// IndexStatsIsInvalid checks whether the index has valid stats or not.
///
/// Go's `sctx` is `vars` (only the session variables are read; Go dereferences it).
// go: IndexStatsIsInvalid L1
pub fn index_stats_is_invalid(
    vars: &SessionVars,
    idx_stats: Option<&Index>,
    coll: &HistColl,
    cid: i64,
) -> bool {
    // If the given index statistics is nil or we found that the index's statistics hasn't been fully loaded, we add this index to NeededItems.
    // Also, we need to check that this HistColl has its physical ID and it is permitted to trigger the stats loading.
    if idx_stats.is_none_or(|i| !i.is_full_load()) && !coll.can_not_trigger_load {
        asyncload::insert_needed_item(TableItemID {
            table_id: coll.physical_id,
            id: cid,
            is_index: true,
            is_sync_load_failed: !vars.stmt_ctx().stats_load_timeout.is_zero(),
        });
        // TODO: we can return true here. But need to fix some tests first.
    }
    let Some(idx_stats) = idx_stats else {
        return true;
    };
    let total_count = idx_stats.total_row_count();
    coll.pseudo || total_count == 0.0
}

// glue: stub of Index.DropUnnecessaryData, blocked-by:D11.2 (stats cache eviction; the cache copies the table first, spec §3)
