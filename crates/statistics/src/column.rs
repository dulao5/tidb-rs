//! go-file: pkg/statistics/column.go
//!
//! D11.1 part 1 (VS1-29): the column statistics and their read methods.
//!
//! - `Info *model.ColumnInfo` is `Option<Arc<ColumnInfo>>`, the same `Arc` as the table's
//!   `TableInfo.Columns[i]` (N13 `M-SHARE`); Go's `Copy` deep-copies it (`Info.Clone()`).
//! - Go embeds `Histogram` and `StatsLoadedStatus`; they are the fields `histogram` and
//!   `stats_loaded_status`, and the promoted methods Go calls through a `*Column` are
//!   delegating methods here (`M-SELF`).
//! - The debug trace of `ColumnStatsIsInvalid` is not ported: the entry points of the
//!   estimation (`tidb-planner` `cardinality`) refuse a statement with the optimizer debug
//!   trace on (specs/D11.1-statistics §2).

use std::sync::Arc;

use tidb_model::{ColumnInfo, TableItemID};
use tidb_sessionctx::variable::session::SessionVars;
use tidb_types::datum::Datum;

use crate::asyncload;
use crate::cmsketch::{CMSketch, TopN, top_n_total_count};
use crate::fmsketch::FMSketch;
use crate::histogram::{
    AllEvicted, Histogram, StatsLoadedStatus, Version0, Version2, new_histogram,
};
use crate::table::{ColumnMemUsage, HistColl};

/// Column represents a column histogram.
#[derive(Clone, Debug)]
pub struct Column {
    /// LastAnalyzePos.
    pub last_analyze_pos: Datum,
    /// CMSketch.
    pub cm_sketch: Option<Arc<CMSketch>>,
    /// TopN.
    pub top_n: Option<Arc<TopN>>,
    /// FMSketch.
    pub fm_sketch: Option<Arc<FMSketch>>,
    /// Info.
    pub info: Option<Arc<ColumnInfo>>,
    /// The embedded `Histogram`.
    pub histogram: Histogram,

    /// StatsLoadedStatus indicates the status of column statistics
    pub stats_loaded_status: StatsLoadedStatus,
    /// PhysicalID is the physical table id,
    /// or it could possibly be -1, which means "stats not available".
    /// The -1 case could happen in a pseudo stats table, and in this case, this stats should not trigger stats loading.
    pub physical_id: i64,
    /// Flag.
    pub flag: i64,
    /// StatsVer is the version of the current stats, used to maintain compatibility
    pub stats_ver: i64,

    /// IsHandle.
    pub is_handle: bool,
}

impl Column {
    /// A column with the given histogram and zero values elsewhere (Go's `&Column{...}`
    /// composite literal with the omitted fields zero).
    // glue: Go composite literal of statistics.Column
    pub fn with_histogram(histogram: Histogram) -> Column {
        Column {
            last_analyze_pos: Datum::default(),
            cm_sketch: None,
            top_n: None,
            fm_sketch: None,
            info: None,
            histogram,
            stats_loaded_status: StatsLoadedStatus::default(),
            physical_id: 0,
            flag: 0,
            stats_ver: 0,
            is_handle: false,
        }
    }

    /// Copy copies the column.
    // go: Column.Copy L1 [M-SHARE]
    // CMSketch / FMSketch have no values in part 1: the code after their methods is unreachable.
    #[allow(unreachable_code)]
    pub fn copy(&self) -> Column {
        let mut nc = Column::with_histogram(self.histogram.copy());
        nc.physical_id = self.physical_id;
        nc.flag = self.flag;
        nc.stats_ver = self.stats_ver;
        nc.is_handle = self.is_handle;
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
            nc.info = Some(Arc::new(ColumnInfo::clone(info)));
        }
        nc.stats_loaded_status = self.stats_loaded_status.copy();
        nc
    }

    /// String.
    // go: Column.String L1 [M-NAME]
    #[allow(clippy::inherent_to_string)] // M-NAME: Go String()
    pub fn to_string(&self) -> String {
        self.histogram.to_string(0)
    }

    /// TotalRowCount returns the total count of this column.
    // go: Column.TotalRowCount L1
    pub fn total_row_count(&self) -> f64 {
        if self.stats_ver >= Version2 {
            return self.histogram.total_row_count()
                + top_n_total_count(self.top_n.as_deref()) as f64;
        }
        self.histogram.total_row_count()
    }

    /// NotNullCount returns the count of this column which is not null.
    // go: Column.NotNullCount L1
    pub fn not_null_count(&self) -> f64 {
        if self.stats_ver >= Version2 {
            return self.histogram.not_null_count()
                + top_n_total_count(self.top_n.as_deref()) as f64;
        }
        self.histogram.not_null_count()
    }

    /// GetIncreaseFactor get the increase factor to adjust the final estimated count when the table is modified.
    // go: Column.GetIncreaseFactor L1
    pub fn get_increase_factor(&self, realtime_row_count: i64) -> f64 {
        let column_count = self.total_row_count();
        if column_count == 0.0 {
            // avoid dividing by 0
            return 1.0;
        }
        realtime_row_count as f64 / column_count
    }

    /// MemoryUsage returns the total memory usage of Histogram, CMSketch, FMSketch in Column.
    /// We ignore the size of other metadata in Column
    ///
    /// Go reads `c.Info.ID` (a nil `Info` panics); so does this.
    // go: Column.MemoryUsage L1
    // CMSketch / FMSketch have no values in part 1: the code after their methods is unreachable.
    #[allow(unreachable_code)]
    pub fn memory_usage(&self) -> ColumnMemUsage {
        let mut column_mem_usage = ColumnMemUsage {
            column_id: self.info.as_ref().expect("Column.Info").id,
            ..ColumnMemUsage::default()
        };
        let histogram_mem_usage = self.histogram.memory_usage();
        column_mem_usage.histogram_mem_usage = histogram_mem_usage;
        let mut sum = histogram_mem_usage;
        if let Some(c) = &self.cm_sketch {
            let cm_sketch_mem_usage = c.memory_usage();
            column_mem_usage.cm_sketch_mem_usage = cm_sketch_mem_usage;
            sum += cm_sketch_mem_usage;
        }
        if let Some(t) = &self.top_n {
            let topn_mem_usage = t.memory_usage();
            column_mem_usage.top_n_mem_usage = topn_mem_usage;
            sum += topn_mem_usage;
        }
        if let Some(f) = &self.fm_sketch {
            let fm_sketch_mem_usage = f.memory_usage();
            column_mem_usage.fm_sketch_mem_usage = fm_sketch_mem_usage;
            sum += fm_sketch_mem_usage;
        }
        column_mem_usage.total_mem_usage = sum;
        column_mem_usage
    }

    /// ItemID implements TableCacheItem
    // go: Column.ItemID L1
    pub fn item_id(&self) -> i64 {
        self.info.as_ref().expect("Column.Info").id
    }

    /// IsAllEvicted indicates whether all stats evicted
    ///
    /// Go's nil receiver is `c: None`.
    // go: Column.IsAllEvicted L1
    pub fn is_all_evicted(c: Option<&Column>) -> bool {
        match c {
            None => true,
            Some(c) => {
                c.stats_loaded_status.stats_initialized
                    && c.stats_loaded_status.evicted_status >= AllEvicted
            }
        }
    }

    /// GetEvictedStatus indicates the evicted status
    // go: Column.GetEvictedStatus L1
    pub fn get_evicted_status(&self) -> isize {
        self.stats_loaded_status.evicted_status
    }

    /// IsStatsInitialized indicates whether stats is initialized
    // go: Column.IsStatsInitialized L1
    pub fn is_stats_initialized(&self) -> bool {
        self.stats_loaded_status.stats_initialized
    }

    /// GetStatsVer indicates the stats version
    // go: Column.GetStatsVer L1
    pub fn get_stats_ver(&self) -> i64 {
        self.stats_ver
    }

    /// IsCMSExist indicates whether CMSketch exists
    // go: Column.IsCMSExist L1
    pub fn is_cms_exist(&self) -> bool {
        self.cm_sketch.is_some()
    }

    /// IsAnalyzed indicates whether the column is analyzed.
    /// The set of IsAnalyzed columns is a subset of the set of StatsAvailable columns.
    // go: Column.IsAnalyzed L1
    pub fn is_analyzed(&self) -> bool {
        self.get_stats_ver() != Version0
    }

    /// StatsAvailable indicates whether the column stats are collected.
    /// Note:
    ///  1. The function merely talks about whether the stats are collected, regardless of the stats loaded status.
    ///  2. The function is used to decide StatsLoadedStatus.statsInitialized when reading the column stats from storage.
    ///  3. There are two cases that StatsAvailable is true:
    ///     a. IsAnalyzed is true.
    ///     b. The column is newly-added/modified and its stats are generated according to the default value.
    // go: Column.StatsAvailable L1
    pub fn stats_available(&self) -> bool {
        // Typically, when the column is analyzed, StatsVer is set to Version1/Version2, so we check IsAnalyzed().
        // However, when we add/modify a column, its stats are generated according to the default value without setting
        // StatsVer, so we check NDV > 0 || NullCount > 0 for the case.
        self.is_analyzed() || self.histogram.ndv > 0 || self.histogram.null_count > 0
    }

    /// GetHistogram returns the histogram for this column.
    // go: Column.GetHistogram L1
    pub fn get_histogram(&self) -> &Histogram {
        &self.histogram
    }

    /// GetTopN returns the TopN for this column.
    // go: Column.GetTopN L1
    pub fn get_top_n(&self) -> Option<&TopN> {
        self.top_n.as_deref()
    }

    /// The promoted `StatsLoadedStatus.IsFullLoad`.
    // glue: M-SELF (promoted method of the embedded StatsLoadedStatus)
    pub fn is_full_load(&self) -> bool {
        self.stats_loaded_status.is_full_load()
    }

    /// The promoted `StatsLoadedStatus.IsLoadNeeded`.
    // glue: M-SELF (promoted method of the embedded StatsLoadedStatus)
    pub fn is_load_needed(&self) -> bool {
        self.stats_loaded_status.is_load_needed()
    }

    /// The promoted `StatsLoadedStatus.IsEssentialStatsLoaded`.
    // glue: M-SELF (promoted method of the embedded StatsLoadedStatus)
    pub fn is_essential_stats_loaded(&self) -> bool {
        self.stats_loaded_status.is_essential_stats_loaded()
    }

    /// GreaterRowCount (promoted from the embedded `Histogram`).
    // glue: M-SELF (promoted method of the embedded Histogram)
    pub fn greater_row_count(&self, value: &Datum) -> f64 {
        self.histogram.greater_row_count(value)
    }

    /// LessRowCount (promoted from the embedded `Histogram`).
    // glue: M-SELF (promoted method of the embedded Histogram)
    pub fn less_row_count(&self, value: &Datum) -> f64 {
        self.histogram.less_row_count(value)
    }
}

/// ColumnStatsIsInvalid checks if this column is invalid.
/// If this column has histogram but not loaded yet,
/// then we mark it as need histogram.
///
/// Go's `sctx` is `vars` (`None` for Go's nil): only the session variables are read.
// go: ColumnStatsIsInvalid L1
pub fn column_stats_is_invalid(
    col_stats: Option<&Column>,
    vars: Option<&SessionVars>,
    hist_coll: &HistColl,
    cid: i64,
) -> bool {
    if let Some(vars) = vars {
        // Internal pseudo columns (eg: _tidb_rowid with ID -1) don't have persisted column stats.
        let is_non_internal_column_id = cid > 0;
        if (col_stats.is_none_or(|c| !c.is_stats_initialized() || c.is_load_needed()))
            && is_non_internal_column_id
            && !hist_coll.can_not_trigger_load
        {
            asyncload::insert_needed_item(TableItemID {
                table_id: hist_coll.physical_id,
                id: cid,
                is_index: false,
                is_sync_load_failed: !vars.stmt_ctx().stats_load_timeout.is_zero(),
            });
        }
    }
    if hist_coll.pseudo {
        return true;
    }
    let Some(col_stats) = col_stats else {
        return true;
    };
    // In some cases, some statistics in column would be evicted
    // For example: the cmsketch of the column might be evicted while the histogram and the topn are still exists
    // In this case, we will think this column as valid due to we can still use the rest of the statistics to do optimize.
    let total_count = col_stats.total_row_count();
    let essential_loaded = col_stats.is_essential_stats_loaded();
    let ndv = col_stats.histogram.ndv;
    total_count == 0.0 || (!essential_loaded && ndv > 0)
}

/// EmptyColumn creates an empty column object. It may be used for pseudo estimation or to stop loading unexisting stats.
// go: EmptyColumn L1 [M-SHARE]
pub fn empty_column(tid: i64, pk_is_handle: bool, col_info: &Arc<ColumnInfo>) -> Column {
    let mut c = Column::with_histogram(new_histogram(
        col_info.id,
        0,
        0,
        0,
        Arc::new(col_info.field_type.clone()),
        0,
        0,
    ));
    c.physical_id = tid;
    c.info = Some(Arc::clone(col_info));
    c.is_handle = pk_is_handle && tidb_base::mysql::has_pri_key_flag(col_info.get_flag());
    c
}

// glue: stub of Column.DropUnnecessaryData, blocked-by:D11.2 (stats cache eviction; the cache copies the table first, spec §3)
