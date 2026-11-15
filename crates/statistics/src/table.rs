//! go-file: pkg/statistics/table.go
//!
//! D11.1 part 1 (VS1-29): `Table`, `HistColl`, `ColAndIdxExistenceMap`, `PseudoTable` and the
//! read methods the planner uses.
//!
//! Sharing (N13 `M-SHARE`, specs/D11.1-statistics §3):
//! - A `Table` in the stats cache (D11.2) is an `Arc<Table>`: read-only, shared by every
//!   statement that reads it. An update builds a new `Table` (or copies the old one) and
//!   replaces the cache entry; nobody writes a cached `Table` in place.
//! - The column / index maps are `Arc<BTreeMap<_, Arc<Column>>>`: Go's `ShallowCopy` shares
//!   the maps (here: clones the `Arc`), `SetCol` / `DelCol` copy the map first if it is shared
//!   (`Arc::make_mut`), so a write never shows through another holder (Go writes the shared
//!   map; its callers copy first, spec §3).
//! - `Column.Info` / `Index.Info` are the `Arc`s of the `TableInfo`.
//! - No interior mutability in any of these types (N13 rule 3; `check_model_mutability.py`
//!   covers `crates/statistics/src`).
//! - Go's maps iterate in random order; the Rust maps are `BTreeMap`s (ascending id). Every
//!   ported reader either sorts or does not depend on the order (`M-MAPORDER`, spec §2).
//! - Go's `struct` copy `nt := *t` is `Clone` (derived: shares the `Arc`s); Go's `Copy`
//!   method (a deep copy) is [`Table::copy`].

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use tidb_base::Error;
use tidb_model::{ColumnInfo, IndexInfo, TableInfo};

use crate::cmsketch::{CMSketch, TopN};
use crate::column::Column;
use crate::fmsketch::FMSketch;
use crate::histogram::{Histogram, new_histogram};
use crate::index::Index;

/// PseudoVersion means the pseudo statistics version is 0.
pub const PseudoVersion: u64 = 0;

/// PseudoRowCount export for other pkg to use.
/// When we haven't analyzed a table, we use pseudo statistics to estimate costs.
/// It has row count 10000, equal condition selects 1/1000 of total rows, less condition selects 1/3 of total rows,
/// between condition selects 1/40 of total rows.
pub const PseudoRowCount: i64 = 10000;

/// AutoAnalyzeMinCnt means if the count of table is less than this value, we don't need to do auto analyze.
/// Exported for testing.
///
/// A Go package variable (tests change it): an atomic, not a field of a shared struct.
pub static AutoAnalyzeMinCnt: AtomicI64 = AtomicI64::new(1000); // N13-allow: Go package variable

// Below functions are used to solve cycle import problem (Go `GetRowCountByIndexRanges`,
// `GetRowCountByIntColumnRanges`, `GetRowCountByColumnRanges` function variables, set by
// cardinality's `init`): the Rust crates have no such cycle, the planner calls
// `tidb_planner::cardinality` directly and nothing in this crate calls them (spec §2).

/// Table represents statistics for a table.
#[derive(Clone, Debug, Default)]
pub struct Table {
    /// ExtendedStats.
    pub extended_stats: Option<Arc<ExtendedStatsColl>>,

    /// ColAndIdxExistenceMap.
    pub col_and_idx_existence_map: Option<Arc<ColAndIdxExistenceMap>>,
    /// The embedded `HistColl`.
    pub hist_coll: HistColl,
    /// Version.
    pub version: u64,
    /// It's the timestamp of the last analyze time.
    /// We used it in auto-analyze to determine if this table has been analyzed.
    /// The source of this field comes from two parts:
    /// 1. Initialized by snapshot when loading stats_meta.
    /// 2. Updated by the analysis time of a specific column or index when loading the histogram of the column or index.
    pub last_analyze_version: u64,
    /// LastStatsHistVersion is the mvcc version of the last update of histograms.
    /// It differs from LastAnalyzeVersion because it can be influenced by some DDL.
    /// e.g. When we execute ALTER TABLE ADD COLUMN, there'll be new record inserted into mysql.stats_histograms.
    ///      We need to load the corresponding one into memory too.
    /// It's used to skip redundant loading of stats, i.e, if the cached stats is already update-to-date with mysql.stats_xxx tables,
    /// and the schema of the table does not change, we don't need to load the stats for this table again.
    /// Stats' sync load/async load should not change this field since they are not table-level update.
    /// It's hard to deal with the upgrade compatibility of this field, the field will not take effect unless
    /// auto analyze or DDL happened on the table.
    pub last_stats_hist_version: u64,
    /// TblInfoUpdateTS is the UpdateTS of the TableInfo used when filling this struct.
    /// It is the schema version of the corresponding table. It is used to skip redundant
    /// loading of stats, i.e, if the cached stats is already update-to-date with mysql.stats_xxx tables,
    /// and the schema of the table does not change, we don't need to load the stats for this
    /// table again.
    /// TODO: it can be removed now that we've have LastAnalyseVersion and LastStatsHistVersion.
    pub tbl_info_update_ts: u64,

    /// IsPkIsHandle.
    pub is_pk_is_handle: bool,
}

impl std::ops::Deref for Table {
    type Target = HistColl;
    // glue: M-SELF (Go embeds HistColl in Table)
    fn deref(&self) -> &HistColl {
        &self.hist_coll
    }
}

impl std::ops::DerefMut for Table {
    // glue: M-SELF (Go embeds HistColl in Table)
    fn deref_mut(&mut self) -> &mut HistColl {
        &mut self.hist_coll
    }
}

/// ColAndIdxExistenceMap is the meta map for statistics.Table.
/// It can tell whether a column/index really has its statistics. So we won't send useless kv request when we do online stats loading.
///
/// `Clone` is Go's `Clone` (the maps hold `bool`s, a deep copy is a value copy).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ColAndIdxExistenceMap {
    col_analyzed: BTreeMap<i64, bool>,
    idx_analyzed: BTreeMap<i64, bool>,
}

impl ColAndIdxExistenceMap {
    /// DeleteColNotFound deletes the column with the given id.
    // go: ColAndIdxExistenceMap.DeleteColNotFound L1
    pub fn delete_col_not_found(&mut self, id: i64) {
        self.col_analyzed.remove(&id);
    }

    /// DeleteIdxNotFound deletes the index with the given id.
    // go: ColAndIdxExistenceMap.DeleteIdxNotFound L1
    pub fn delete_idx_not_found(&mut self, id: i64) {
        self.idx_analyzed.remove(&id);
    }

    /// HasAnalyzed checks whether a column/index stats exists and it has stats.
    /// TODO: the map should only keep the analyzed cols.
    /// There's three possible status of column/index's statistics:
    ///  1. We don't have this column/index.
    ///  2. We have it, but it hasn't been analyzed yet.
    ///  3. We have it and its statistics.
    ///
    /// To figure out three status, we use HasAnalyzed's TRUE value to represents the status 3. The Has's FALSE to represents the status 1.
    /// Begin from v8.5.2, the 1. case becomes a nearly invalid case. It's just a middle state between happening of the DDL and the completion of the stats' ddl handler.
    /// But we may need to deal with the 1. for the upgrade compatibility.
    // go: ColAndIdxExistenceMap.HasAnalyzed L1
    pub fn has_analyzed(&self, id: i64, is_index: bool) -> bool {
        if is_index {
            return self.idx_analyzed.get(&id).copied().unwrap_or(false);
        }
        self.col_analyzed.get(&id).copied().unwrap_or(false)
    }

    /// Has checks whether a column/index stats exists.
    // go: ColAndIdxExistenceMap.Has L1
    pub fn has(&self, id: i64, is_index: bool) -> bool {
        if is_index {
            return self.idx_analyzed.contains_key(&id);
        }
        self.col_analyzed.contains_key(&id)
    }

    /// InsertCol inserts a column with its meta into the map.
    // go: ColAndIdxExistenceMap.InsertCol L1
    pub fn insert_col(&mut self, id: i64, analyzed: bool) {
        self.col_analyzed.insert(id, analyzed);
    }

    /// InsertIndex inserts an index with its meta into the map.
    // go: ColAndIdxExistenceMap.InsertIndex L1
    pub fn insert_index(&mut self, id: i64, analyzed: bool) {
        self.idx_analyzed.insert(id, analyzed);
    }

    /// IsEmpty checks whether the map is empty.
    // go: ColAndIdxExistenceMap.IsEmpty L1
    pub fn is_empty(&self) -> bool {
        self.col_analyzed.len() + self.idx_analyzed.len() == 0
    }

    /// ColNum returns the number of columns in the map.
    // go: ColAndIdxExistenceMap.ColNum L1
    pub fn col_num(&self) -> usize {
        self.col_analyzed.len()
    }

    /// Clone deeply copies the map.
    // go: ColAndIdxExistenceMap.Clone L1 [M-NAME]
    pub fn clone_map(&self) -> ColAndIdxExistenceMap {
        ColAndIdxExistenceMap {
            col_analyzed: self.col_analyzed.clone(),
            idx_analyzed: self.idx_analyzed.clone(),
        }
    }

    /// The entries in id order, for tests and vectors.
    // glue: test accessor
    pub fn entries(&self) -> (&BTreeMap<i64, bool>, &BTreeMap<i64, bool>) {
        (&self.col_analyzed, &self.idx_analyzed)
    }
}

/// NewColAndIndexExistenceMapWithoutSize return a new object with default capacity.
// go: NewColAndIndexExistenceMapWithoutSize L1
pub fn new_col_and_index_existence_map_without_size() -> ColAndIdxExistenceMap {
    ColAndIdxExistenceMap::default()
}

/// NewColAndIndexExistenceMap return a new object with the given capcity.
///
/// A `BTreeMap` has no capacity; the arguments are ignored.
// go: NewColAndIndexExistenceMap L1
pub fn new_col_and_index_existence_map(_col_cap: usize, _idx_cap: usize) -> ColAndIdxExistenceMap {
    ColAndIdxExistenceMap::default()
}

/// ColAndIdxExistenceMapIsEqual is used in testing, checking whether the two are equal.
// go: ColAndIdxExistenceMapIsEqual L1
pub fn col_and_idx_existence_map_is_equal(
    m1: &ColAndIdxExistenceMap,
    m2: &ColAndIdxExistenceMap,
) -> bool {
    m1.col_analyzed == m2.col_analyzed && m1.idx_analyzed == m2.idx_analyzed
}

/// ExtendedStatsItem is the cached item of a mysql.stats_extended record.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExtendedStatsItem {
    /// StringVals.
    pub string_vals: String,
    /// ColIDs.
    pub col_ids: Vec<i64>,
    /// ScalarVals.
    pub scalar_vals: f64,
    /// Tp.
    pub tp: u8,
}

/// ExtendedStatsColl is a collection of cached items for mysql.stats_extended records.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExtendedStatsColl {
    /// Stats.
    pub stats: BTreeMap<String, Arc<ExtendedStatsItem>>,
    /// LastUpdateVersion.
    pub last_update_version: u64,
}

/// NewExtendedStatsColl allocate an ExtendedStatsColl struct.
// go: NewExtendedStatsColl L1
pub fn new_extended_stats_coll() -> ExtendedStatsColl {
    ExtendedStatsColl::default()
}

/// ExtendedStatsInited is the status for extended stats which are just registered but have not been analyzed yet.
pub const ExtendedStatsInited: u8 = 0;
/// ExtendedStatsAnalyzed is the status for extended stats which have been collected in analyze.
pub const ExtendedStatsAnalyzed: u8 = 1;
/// ExtendedStatsDeleted is the status for extended stats which were dropped. These "deleted" records would be removed from storage by GCStats().
pub const ExtendedStatsDeleted: u8 = 2;

/// Go `PrepareCols4MVIndex` (a package function variable set by planner/core, to avoid an
/// import cycle): identifies the columns of an MV index. planner/core's implementation is
/// D10.7 (index merge paths); until it registers one, a `HistColl` cannot be generated for a
/// table with an MV index (spec §4 item 3).
pub type PrepareCols4MVIndexFn = fn(
    table_info: &TableInfo,
    mv_index: &IndexInfo,
    tbl_cols_by_id: &HashMap<i64, Arc<tidb_expression::Column>>,
    check_only1_array_type_col: bool,
) -> (Vec<Arc<tidb_expression::Column>>, bool);

/// PrepareCols4MVIndex helps to identify the columns of an MV index. We need this information for estimation.
/// This logic is shared between the estimation logic and the access path generation logic. We'd like to put the mv index
/// related functions together in the planner/core package. So we use this trick here to avoid the import cycle.
pub static PrepareCols4MVIndex: OnceLock<PrepareCols4MVIndexFn> = OnceLock::new(); // N13-allow: Go package function variable, set once

/// HistColl is a collection of histograms. It collects enough information for plan to calculate the selectivity.
#[derive(Clone, Debug, Default)]
pub struct HistColl {
    /// Note that when used in a query, Column use UniqueID as the key while Indices use the index ID in the
    /// metadata. (See GenerateHistCollFromColumnInfo() for details)
    columns: Arc<BTreeMap<i64, Arc<Column>>>,
    indices: Arc<BTreeMap<i64, Arc<Index>>>,
    /// PhysicalID.
    pub physical_id: i64,
    // TODO: add AnalyzeCount here
    /// RealtimeCount is the current table row count, maintained by applying stats delta based on AnalyzeCount.
    pub realtime_count: i64,
    /// Total modify count in a table.
    pub modify_count: i64,

    /// The version of the statistics, refer to Version0, Version1, Version2 and so on.
    pub stats_ver: isize,
    /// HavePhysicalID is true means this HistColl is from single table and have its ID's information.
    /// The physical id is used when try to load column stats from storage.
    pub have_physical_id: bool,
    /// Pseudo.
    pub pseudo: bool,

    // Fields below are only used in a query, like for estimation, and they will be useless when stored in
    // the stats cache. (See GenerateHistCollFromColumnInfo() for details)
    /// CanNotTriggerLoad.
    pub can_not_trigger_load: bool,
    /// Idx2ColUniqueIDs maps the index id to its column UniqueIDs. It's used to calculate the selectivity in planner.
    pub idx2_col_unique_ids: BTreeMap<i64, Vec<i64>>,
    /// ColUniqueID2IdxIDs maps the column UniqueID to a list index ids whose first column is it.
    /// It's used to calculate the selectivity in planner.
    pub col_unique_id2_idx_ids: BTreeMap<i64, Vec<i64>>,
    /// UniqueID2colInfoID maps the column UniqueID to its ID in the metadata.
    pub unique_id2_col_info_id: BTreeMap<i64, i64>,
    /// MVIdx2Columns maps the index id to its columns by expression.Column.
    /// For normal index, the column id is enough, as we already have in Idx2ColUniqueIDs. But currently, mv index needs more
    /// information to match the filter against the mv index columns, and we need this map to provide this information.
    pub mv_idx2_columns: BTreeMap<i64, Vec<Arc<tidb_expression::Column>>>,
}

/// NewHistColl creates a new HistColl.
// go: NewHistColl L1
pub fn new_hist_coll(
    id: i64,
    have_physical_id: bool,
    realtime_cnt: i64,
    modify_cnt: i64,
    _col_num: usize,
    _idx_num: usize,
) -> HistColl {
    HistColl {
        physical_id: id,
        have_physical_id,
        realtime_count: realtime_cnt,
        modify_count: modify_cnt,
        ..HistColl::default()
    }
}

/// NewHistCollWithColsAndIdxs creates a new HistColl with given columns and indices.
// go: NewHistCollWithColsAndIdxs L1
pub fn new_hist_coll_with_cols_and_idxs(
    id: i64,
    have_physical_id: bool,
    realtime_cnt: i64,
    modify_cnt: i64,
    cols: BTreeMap<i64, Arc<Column>>,
    idxs: BTreeMap<i64, Arc<Index>>,
) -> HistColl {
    HistColl {
        columns: Arc::new(cols),
        indices: Arc::new(idxs),
        physical_id: id,
        have_physical_id,
        realtime_count: realtime_cnt,
        modify_count: modify_cnt,
        ..HistColl::default()
    }
}

impl HistColl {
    /// SetCol sets the column with the given id.
    ///
    /// Copies the map first when another holder shares it (Go writes the shared map, spec §3).
    // go: HistColl.SetCol L1 [M-SHARE]
    pub fn set_col(&mut self, id: i64, col: Arc<Column>) {
        Arc::make_mut(&mut self.columns).insert(id, col);
    }

    /// SetIdx sets the index with the given id.
    // go: HistColl.SetIdx L1 [M-SHARE]
    pub fn set_idx(&mut self, id: i64, idx: Arc<Index>) {
        Arc::make_mut(&mut self.indices).insert(id, idx);
    }

    /// GetCol gets the column with the given id.
    // go: HistColl.GetCol L1
    pub fn get_col(&self, id: i64) -> Option<&Arc<Column>> {
        self.columns.get(&id)
    }

    /// GetIdx gets the index with the given id.
    // go: HistColl.GetIdx L1
    pub fn get_idx(&self, id: i64) -> Option<&Arc<Index>> {
        self.indices.get(&id)
    }

    /// ForEachColumnImmutable iterates all columns in the HistColl.
    /// The bool return value of f is used to control the iteration. If f returns true, the iteration will be stopped.
    /// Warning: Don't change the content when calling this function.
    ///
    /// The order is ascending id (Go: the map's random order, `M-MAPORDER`).
    // go: HistColl.ForEachColumnImmutable L1 [M-MAPORDER]
    pub fn for_each_column_immutable(&self, mut f: impl FnMut(i64, &Arc<Column>) -> bool) {
        for (id, col) in self.columns.iter() {
            if f(*id, col) {
                return;
            }
        }
    }

    /// ForEachIndexImmutable iterates all columns in the HistColl.
    /// The bool return value of f is used to control the iteration. If f returns true, the iteration will be stopped.
    /// WARNING: Don't change the content when calling this function.
    // go: HistColl.ForEachIndexImmutable L1 [M-MAPORDER]
    pub fn for_each_index_immutable(&self, mut f: impl FnMut(i64, &Arc<Index>) -> bool) {
        for (id, idx) in self.indices.iter() {
            if f(*id, idx) {
                return;
            }
        }
    }

    /// ColNum returns the number of columns in the HistColl.
    // go: HistColl.ColNum L1
    pub fn col_num(&self) -> usize {
        self.columns.len()
    }

    /// IdxNum returns the number of indices in the HistColl.
    // go: HistColl.IdxNum L1
    pub fn idx_num(&self) -> usize {
        self.indices.len()
    }

    /// StableOrderColSlice returns a slice of columns in stable order.
    ///
    /// Go sorts by `Column.ID` (the histogram id), not by the map key.
    // go: HistColl.StableOrderColSlice L1
    pub fn stable_order_col_slice(&self) -> Vec<Arc<Column>> {
        let mut cols: Vec<Arc<Column>> = self.columns.values().map(Arc::clone).collect();
        cols.sort_by_key(|c| c.histogram.id);
        cols
    }

    /// GetColSlice returns a slice of columns without order.
    // go: HistColl.GetColSlice L1 [M-MAPORDER]
    pub fn get_col_slice(&self) -> Vec<Arc<Column>> {
        self.columns.values().map(Arc::clone).collect()
    }

    /// StableOrderIdxSlice returns a slice of indices in stable order.
    // go: HistColl.StableOrderIdxSlice L1
    pub fn stable_order_idx_slice(&self) -> Vec<Arc<Index>> {
        let mut idxs: Vec<Arc<Index>> = self.indices.values().map(Arc::clone).collect();
        idxs.sort_by_key(|i| i.histogram.id);
        idxs
    }

    /// GetIdxSlice returns a slice of indices without order.
    // go: HistColl.GetIdxSlice L1 [M-MAPORDER]
    pub fn get_idx_slice(&self) -> Vec<Arc<Index>> {
        self.indices.values().map(Arc::clone).collect()
    }

    /// CalcPreScalar calculates the pre-calculated scalar for all columns and indices.
    ///
    /// Go writes every histogram in place; it runs on a table being built (bootstrap) that no
    /// one else holds. Here a shared column / index is copied first (`Arc::make_mut`).
    // go: HistColl.CalcPreScalar L1 [M-SHARE]
    pub fn calc_pre_scalar(&mut self) {
        for idx in Arc::make_mut(&mut self.indices).values_mut() {
            let idx = Arc::make_mut(idx);
            for i in 1..idx.histogram.len() {
                let prev = idx.histogram.buckets[i - 1].count;
                idx.histogram.buckets[i].count += prev;
            }
            idx.histogram.pre_calculate_scalar();
        }
        for col in Arc::make_mut(&mut self.columns).values_mut() {
            let col = Arc::make_mut(col);
            for i in 1..col.histogram.len() {
                let prev = col.histogram.buckets[i - 1].count;
                col.histogram.buckets[i].count += prev;
            }
            col.histogram.pre_calculate_scalar();
        }
    }

    /// GetAnalyzeRowCount tries to get the row count of a column or an index if possible.
    /// This method is useful because this row count doesn't consider the modify count.
    // go: HistColl.GetAnalyzeRowCount L1
    pub fn get_analyze_row_count(&self) -> f64 {
        // BTreeMap keys are sorted (Go: maps.Keys + slices.Sort).
        for col in self.columns.values() {
            if col.is_full_load() {
                return col.total_row_count();
            }
        }
        for idx in self.indices.values() {
            if idx.info.as_ref().is_some_and(|i| i.mv_index) {
                continue;
            }
            if idx.is_full_load() {
                return idx.total_row_count();
            }
        }
        -1.0
    }

    /// GetScaledRealtimeAndModifyCnt scale the RealtimeCount and ModifyCount for some special indexes where the total row
    /// count is different from the total row count of the table. Currently, only the mv index is this case.
    /// Because we will use the RealtimeCount and ModifyCount during the estimation for ranges on this index (like the upper
    /// bound for the out-of-range estimation logic and the IncreaseFactor logic), we can't directly use the RealtimeCount and
    /// ModifyCount of the table. Instead, we should scale them before using.
    /// For example, if the table analyze row count is 1000 and realtime row count is 1500, and the mv index total count is 5000,
    /// when calculating the IncreaseFactor, it should be 1500/1000 = 1.5 for normal columns/indexes, and we should use the
    /// same 1.5 for mv index. But obviously, use 1500/5000 would be wrong, the correct calculation should be 7500/5000 = 1.5.
    /// So we add this function to get this 7500.
    // go: HistColl.GetScaledRealtimeAndModifyCnt L1 [M-F2I]
    pub fn get_scaled_realtime_and_modify_cnt(&self, idx_stats: Option<&Index>) -> (i64, i64) {
        // In theory, we can apply this scale logic on all indexes. But currently, we only apply it on the mv index to avoid
        // any unexpected changes caused by factors like precision difference.
        let Some(idx_stats) = idx_stats else {
            return (self.realtime_count, self.modify_count);
        };
        if idx_stats.info.as_ref().is_none_or(|i| !i.mv_index) || !idx_stats.is_full_load() {
            return (self.realtime_count, self.modify_count);
        }
        let analyze_row_count = self.get_analyze_row_count();
        if analyze_row_count <= 0.0 {
            return (self.realtime_count, self.modify_count);
        }
        let idx_total_row_count = idx_stats.total_row_count();
        if idx_total_row_count <= 0.0 {
            return (self.realtime_count, self.modify_count);
        }
        let scale = idx_total_row_count / analyze_row_count;
        (
            tidb_base::gostd::conv::f64_to_i64(self.realtime_count as f64 * scale), // [M-F2I]
            tidb_base::gostd::conv::f64_to_i64(self.modify_count as f64 * scale),   // [M-F2I]
        )
    }

    /// ID2UniqueID generates a new HistColl whose `Columns` is built from UniqueID of given columns.
    // go: HistColl.ID2UniqueID L1
    pub fn id2_unique_id(&self, columns: &[Arc<tidb_expression::Column>]) -> HistColl {
        let mut cols = BTreeMap::new();
        for col in columns {
            if let Some(col_hist) = self.columns.get(&col.id) {
                cols.insert(col.unique_id, Arc::clone(col_hist));
            }
        }
        HistColl {
            physical_id: self.physical_id,
            have_physical_id: self.have_physical_id,
            pseudo: self.pseudo,
            realtime_count: self.realtime_count,
            modify_count: self.modify_count,
            columns: Arc::new(cols),
            ..HistColl::default()
        }
    }

    /// GenerateHistCollFromColumnInfo generates a new HistColl whose ColUniqueID2IdxIDs and Idx2ColUniqueIDs is built from the given parameter.
    ///
    /// Returns an error only for an MV index while `PrepareCols4MVIndex` is not registered
    /// (D10.7; Go cannot fail here, spec §4 item 3).
    // go: HistColl.GenerateHistCollFromColumnInfo L2 (Result for the MV index boundary)
    pub fn generate_hist_coll_from_column_info(
        &self,
        tbl_info: &TableInfo,
        columns: &[Arc<tidb_expression::Column>],
    ) -> Result<HistColl, Error> {
        let mut new_col_hist_map = BTreeMap::new();
        let mut col_info_id2_col: HashMap<i64, Arc<tidb_expression::Column>> =
            HashMap::with_capacity(columns.len());
        let mut col_info_id2_unique_id: HashMap<i64, i64> = HashMap::with_capacity(columns.len());
        let mut unique_id2col_info_id: BTreeMap<i64, i64> = BTreeMap::new();
        let mut idx_id2idx_info: HashMap<i64, &Arc<IndexInfo>> = HashMap::new();
        for col in columns {
            col_info_id2_col.insert(col.id, Arc::clone(col));
            col_info_id2_unique_id.insert(col.id, col.unique_id);
            unique_id2col_info_id.insert(col.unique_id, col.id);
        }
        for (id, col_hist) in self.columns.iter() {
            // Collect the statistics by the given columns.
            if let Some(unique_id) = col_info_id2_unique_id.get(id) {
                new_col_hist_map.insert(*unique_id, Arc::clone(col_hist));
            }
        }
        for idx_info in tbl_info.indices.iter().flatten() {
            idx_id2idx_info.insert(idx_info.id, idx_info);
        }
        let tbl_columns: &[Arc<ColumnInfo>] = tbl_info.columns.as_deref().unwrap_or(&[]);
        let mut new_idx_hist_map = BTreeMap::new();
        let mut idx2_columns: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
        let mut col_id2_idx_ids: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
        let mut mv_idx2_columns: BTreeMap<i64, Vec<Arc<tidb_expression::Column>>> = BTreeMap::new();
        for (id, idx_hist) in self.indices.iter() {
            let Some(idx_info) = idx_id2idx_info.get(id) else {
                continue;
            };
            let idx_cols = idx_info.columns.as_deref().unwrap_or(&[]);
            let mut ids = Vec::with_capacity(idx_cols.len());
            for idx_col in idx_cols {
                let Some(unique_id) =
                    col_info_id2_unique_id.get(&tbl_columns[idx_col.offset as usize].id)
                else {
                    break;
                };
                ids.push(*unique_id);
            }
            // If the length of the id list is 0, this index won't be used in this query.
            if ids.is_empty() {
                continue;
            }
            col_id2_idx_ids
                .entry(ids[0])
                .or_default()
                .push(idx_hist.histogram.id);
            new_idx_hist_map.insert(idx_hist.histogram.id, Arc::clone(idx_hist));
            idx2_columns.insert(idx_hist.histogram.id, ids);
            if idx_info.mv_index {
                let Some(prepare) = PrepareCols4MVIndex.get() else {
                    return Err(tidb_base::blocked::blocked("D10.7", "PrepareCols4MVIndex"));
                };
                let (cols, ok) = prepare(tbl_info, idx_info, &col_info_id2_col, true);
                if ok {
                    mv_idx2_columns.insert(*id, cols);
                }
            }
        }
        for idx_ids in col_id2_idx_ids.values_mut() {
            idx_ids.sort();
        }
        Ok(HistColl {
            physical_id: self.physical_id,
            have_physical_id: self.have_physical_id,
            pseudo: self.pseudo,
            realtime_count: self.realtime_count,
            modify_count: self.modify_count,
            columns: Arc::new(new_col_hist_map),
            indices: Arc::new(new_idx_hist_map),
            col_unique_id2_idx_ids: col_id2_idx_ids,
            idx2_col_unique_ids: idx2_columns,
            unique_id2_col_info_id: unique_id2col_info_id,
            mv_idx2_columns,
            ..HistColl::default()
        })
    }
}

/// CacheItemMemoryUsage indicates the memory usage of TableCacheItem
pub trait CacheItemMemoryUsage {
    /// ItemID.
    // glue: interface method of statistics.CacheItemMemoryUsage
    fn item_id(&self) -> i64;
    /// TotalMemoryUsage.
    // glue: interface method of statistics.CacheItemMemoryUsage
    fn total_memory_usage(&self) -> i64;
    /// TrackingMemUsage.
    // glue: interface method of statistics.CacheItemMemoryUsage
    fn tracking_mem_usage(&self) -> i64;
    /// HistMemUsage.
    // glue: interface method of statistics.CacheItemMemoryUsage
    fn hist_mem_usage(&self) -> i64;
    /// TopnMemUsage.
    // glue: interface method of statistics.CacheItemMemoryUsage
    fn topn_mem_usage(&self) -> i64;
    /// CMSMemUsage.
    // glue: interface method of statistics.CacheItemMemoryUsage
    fn cms_mem_usage(&self) -> i64;
}

/// ColumnMemUsage records column memory usage
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ColumnMemUsage {
    /// ColumnID.
    pub column_id: i64,
    /// HistogramMemUsage.
    pub histogram_mem_usage: i64,
    /// CMSketchMemUsage.
    pub cm_sketch_mem_usage: i64,
    /// FMSketchMemUsage.
    pub fm_sketch_mem_usage: i64,
    /// TopNMemUsage.
    pub top_n_mem_usage: i64,
    /// TotalMemUsage.
    pub total_mem_usage: i64,
}

impl CacheItemMemoryUsage for ColumnMemUsage {
    // go: ColumnMemUsage.ItemID L1
    fn item_id(&self) -> i64 {
        self.column_id
    }
    // go: ColumnMemUsage.TotalMemoryUsage L1
    fn total_memory_usage(&self) -> i64 {
        self.total_mem_usage
    }
    // go: ColumnMemUsage.TrackingMemUsage L1
    fn tracking_mem_usage(&self) -> i64 {
        self.cm_sketch_mem_usage + self.top_n_mem_usage + self.histogram_mem_usage
    }
    // go: ColumnMemUsage.HistMemUsage L1
    fn hist_mem_usage(&self) -> i64 {
        self.histogram_mem_usage
    }
    // go: ColumnMemUsage.TopnMemUsage L1
    fn topn_mem_usage(&self) -> i64 {
        self.top_n_mem_usage
    }
    // go: ColumnMemUsage.CMSMemUsage L1
    fn cms_mem_usage(&self) -> i64 {
        self.cm_sketch_mem_usage
    }
}

/// IndexMemUsage records index memory usage
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IndexMemUsage {
    /// IndexID.
    pub index_id: i64,
    /// HistogramMemUsage.
    pub histogram_mem_usage: i64,
    /// CMSketchMemUsage.
    pub cm_sketch_mem_usage: i64,
    /// TopNMemUsage.
    pub top_n_mem_usage: i64,
    /// TotalMemUsage.
    pub total_mem_usage: i64,
}

impl CacheItemMemoryUsage for IndexMemUsage {
    // go: IndexMemUsage.ItemID L1
    fn item_id(&self) -> i64 {
        self.index_id
    }
    // go: IndexMemUsage.TotalMemoryUsage L1
    fn total_memory_usage(&self) -> i64 {
        self.total_mem_usage
    }
    // go: IndexMemUsage.TrackingMemUsage L1
    fn tracking_mem_usage(&self) -> i64 {
        self.cm_sketch_mem_usage + self.top_n_mem_usage + self.histogram_mem_usage
    }
    // go: IndexMemUsage.HistMemUsage L1
    fn hist_mem_usage(&self) -> i64 {
        self.histogram_mem_usage
    }
    // go: IndexMemUsage.TopnMemUsage L1
    fn topn_mem_usage(&self) -> i64 {
        self.top_n_mem_usage
    }
    // go: IndexMemUsage.CMSMemUsage L1
    fn cms_mem_usage(&self) -> i64 {
        self.cm_sketch_mem_usage
    }
}

/// TableMemoryUsage records tbl memory usage
///
/// Go's `map[int64]CacheItemMemoryUsage` holds `*ColumnMemUsage` / `*IndexMemUsage`; the maps
/// hold those types (`M-IFACE`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TableMemoryUsage {
    /// ColumnsMemUsage.
    pub columns_mem_usage: BTreeMap<i64, ColumnMemUsage>,
    /// IndicesMemUsage.
    pub indices_mem_usage: BTreeMap<i64, IndexMemUsage>,
    /// TableID.
    pub table_id: i64,
    /// TotalMemUsage.
    pub total_mem_usage: i64,
}

impl TableMemoryUsage {
    /// TotalIdxTrackingMemUsage returns total indices' tracking memory usage
    // go: TableMemoryUsage.TotalIdxTrackingMemUsage L1
    pub fn total_idx_tracking_mem_usage(&self) -> i64 {
        self.indices_mem_usage
            .values()
            .map(CacheItemMemoryUsage::tracking_mem_usage)
            .sum()
    }

    /// TotalColTrackingMemUsage returns total columns' tracking memory usage
    // go: TableMemoryUsage.TotalColTrackingMemUsage L1
    pub fn total_col_tracking_mem_usage(&self) -> i64 {
        self.columns_mem_usage
            .values()
            .map(CacheItemMemoryUsage::tracking_mem_usage)
            .sum()
    }

    /// TotalTrackingMemUsage return total tracking memory usage
    // go: TableMemoryUsage.TotalTrackingMemUsage L1
    pub fn total_tracking_mem_usage(&self) -> i64 {
        self.total_idx_tracking_mem_usage() + self.total_col_tracking_mem_usage()
    }
}

/// The histogram and sketches of `GetStatsInfo`: borrowed from the table, or copies.
pub type StatsInfoItems<'a> = (
    Cow<'a, Histogram>,
    Option<Arc<CMSketch>>,
    Option<Arc<TopN>>,
    Option<Arc<FMSketch>>,
);

impl Table {
    /// DelCol deletes the column with the given id.
    // go: Table.DelCol L1 [M-SHARE]
    pub fn del_col(&mut self, id: i64) {
        Arc::make_mut(&mut self.hist_coll.columns).remove(&id);
        if let Some(m) = &mut self.col_and_idx_existence_map {
            Arc::make_mut(m).delete_col_not_found(id);
        }
    }

    /// DelIdx deletes the index with the given id.
    // go: Table.DelIdx L1 [M-SHARE]
    pub fn del_idx(&mut self, id: i64) {
        Arc::make_mut(&mut self.hist_coll.indices).remove(&id);
        if let Some(m) = &mut self.col_and_idx_existence_map {
            Arc::make_mut(m).delete_idx_not_found(id);
        }
    }

    /// MemoryUsage returns the total memory usage of this Table.
    /// it will only calc the size of Columns and Indices stats data of table.
    /// We ignore the size of other metadata in Table
    // go: Table.MemoryUsage L1
    pub fn memory_usage(&self) -> TableMemoryUsage {
        let mut t_mem_usage = TableMemoryUsage {
            table_id: self.physical_id,
            ..TableMemoryUsage::default()
        };
        for col in self.hist_coll.columns.values() {
            let col_mem_usage = col.memory_usage();
            t_mem_usage
                .columns_mem_usage
                .insert(col_mem_usage.item_id(), col_mem_usage);
            t_mem_usage.total_mem_usage += col_mem_usage.total_memory_usage();
        }
        for index in self.hist_coll.indices.values() {
            let idx_mem_usage = index.memory_usage();
            t_mem_usage
                .indices_mem_usage
                .insert(idx_mem_usage.item_id(), idx_mem_usage);
            t_mem_usage.total_mem_usage += idx_mem_usage.total_memory_usage();
        }
        t_mem_usage
    }

    /// Copy copies the current table.
    // go: Table.Copy L1 [M-SHARE]
    pub fn copy(&self) -> Table {
        let mut columns = BTreeMap::new();
        for (id, col) in self.hist_coll.columns.iter() {
            columns.insert(*id, Arc::new(col.copy()));
        }
        let mut indices = BTreeMap::new();
        for (id, idx) in self.hist_coll.indices.iter() {
            indices.insert(*id, Arc::new(idx.copy()));
        }
        let new_hist_coll = HistColl {
            physical_id: self.physical_id,
            have_physical_id: self.have_physical_id,
            realtime_count: self.realtime_count,
            columns: Arc::new(columns),
            indices: Arc::new(indices),
            pseudo: self.pseudo,
            modify_count: self.modify_count,
            stats_ver: self.stats_ver,
            ..HistColl::default()
        };
        let mut nt = Table {
            hist_coll: new_hist_coll,
            version: self.version,
            tbl_info_update_ts: self.tbl_info_update_ts,
            last_analyze_version: self.last_analyze_version,
            last_stats_hist_version: self.last_stats_hist_version,
            ..Table::default()
        };
        if let Some(ext) = &self.extended_stats {
            let mut new_ext_stats_coll = ExtendedStatsColl {
                stats: BTreeMap::new(),
                last_update_version: ext.last_update_version,
            };
            for (name, item) in &ext.stats {
                new_ext_stats_coll
                    .stats
                    .insert(name.clone(), Arc::clone(item));
            }
            nt.extended_stats = Some(Arc::new(new_ext_stats_coll));
        }
        if let Some(m) = &self.col_and_idx_existence_map {
            nt.col_and_idx_existence_map = Some(Arc::new(m.clone_map()));
        }
        nt
    }

    /// ShallowCopy copies the current table.
    /// It's different from Copy(). Only the struct Table (and also the embedded HistColl) is copied here.
    /// The internal containers, like t.Columns and t.Indices, and the stats, like TopN and Histogram are not copied.
    // go: Table.ShallowCopy L1 [M-SHARE]
    pub fn shallow_copy(&self) -> Table {
        let new_hist_coll = HistColl {
            physical_id: self.physical_id,
            have_physical_id: self.have_physical_id,
            realtime_count: self.realtime_count,
            columns: Arc::clone(&self.hist_coll.columns),
            indices: Arc::clone(&self.hist_coll.indices),
            pseudo: self.pseudo,
            modify_count: self.modify_count,
            stats_ver: self.stats_ver,
            ..HistColl::default()
        };
        Table {
            hist_coll: new_hist_coll,
            version: self.version,
            tbl_info_update_ts: self.tbl_info_update_ts,
            extended_stats: self.extended_stats.as_ref().map(Arc::clone),
            col_and_idx_existence_map: self.col_and_idx_existence_map.as_ref().map(Arc::clone),
            last_analyze_version: self.last_analyze_version,
            last_stats_hist_version: self.last_stats_hist_version,
            is_pk_is_handle: false,
        }
    }

    /// String implements Stringer interface.
    // go: Table.String L1 [M-NAME]
    #[allow(clippy::inherent_to_string)] // M-NAME: Go String()
    pub fn to_string(&self) -> String {
        let mut strs: Vec<String> = Vec::with_capacity(self.hist_coll.columns.len() + 1);
        strs.push(format!(
            "Table:{} RealtimeCount:{}",
            self.physical_id, self.realtime_count
        ));
        let mut cols: Vec<&Arc<Column>> = self.hist_coll.columns.values().collect();
        cols.sort_by_key(|c| c.histogram.id);
        for col in cols {
            strs.push(col.to_string());
        }
        let mut idxs: Vec<&Arc<Index>> = self.hist_coll.indices.values().collect();
        idxs.sort_by_key(|i| i.histogram.id);
        for idx in idxs {
            strs.push(idx.to_string());
        }
        // TODO: concat content of ExtendedStatsColl
        strs.join("\n")
    }

    /// IndexStartWithColumn finds the first index whose first column is the given column.
    ///
    /// Go returns the first match in the map's random order; here the lowest index id
    /// (`M-MAPORDER`, spec §2).
    // go: Table.IndexStartWithColumn L1 [M-MAPORDER]
    pub fn index_start_with_column(&self, col_name: &str) -> Option<&Arc<Index>> {
        self.hist_coll.indices.values().find(|index| {
            index
                .info
                .as_ref()
                .expect("Index.Info")
                .columns
                .as_deref()
                .unwrap_or(&[])[0]
                .name
                .l
                == col_name
        })
    }

    /// ColumnByName finds the statistics.Column for the given column.
    // go: Table.ColumnByName L1 [M-MAPORDER]
    pub fn column_by_name(&self, col_name: &str) -> Option<&Arc<Column>> {
        self.hist_coll
            .columns
            .values()
            .find(|c| c.info.as_ref().expect("Column.Info").name.l == col_name)
    }

    /// GetStatsInfo returns their statistics according to the ID of the column or index, including histogram, CMSketch, TopN and FMSketch.
    ///
    ///  needCopy: In order to protect the item in the cache from being damaged, we need to copy the item.
    ///
    /// Go's `(nil, nil, nil, nil, false)` is `None`.
    // go: Table.GetStatsInfo L1
    // CMSketch / FMSketch have no values in part 1: the code after their methods is unreachable.
    #[allow(unreachable_code)]
    pub fn get_stats_info(
        &self,
        id: i64,
        is_index: bool,
        need_copy: bool,
    ) -> Option<StatsInfoItems<'_>> {
        if is_index {
            // newly added index which is not analyzed yet
            let idx = self.hist_coll.indices.get(&id)?;
            if need_copy {
                return Some((
                    Cow::Owned(idx.histogram.copy()),
                    idx.cm_sketch.as_ref().map(|c| Arc::new(c.copy())),
                    idx.top_n.as_ref().map(|t| Arc::new(t.copy())),
                    idx.fm_sketch.as_ref().map(|f| Arc::new(f.copy())),
                ));
            }
            return Some((
                Cow::Borrowed(&idx.histogram),
                idx.cm_sketch.as_ref().map(Arc::clone),
                idx.top_n.as_ref().map(Arc::clone),
                idx.fm_sketch.as_ref().map(Arc::clone),
            ));
        }
        // newly added column which is not analyzed yet
        let col = self.hist_coll.columns.get(&id)?;
        if need_copy {
            return Some((
                Cow::Owned(col.histogram.copy()),
                col.cm_sketch.as_ref().map(|c| Arc::new(c.copy())),
                col.top_n.as_ref().map(|t| Arc::new(t.copy())),
                col.fm_sketch.as_ref().map(|f| Arc::new(f.copy())),
            ));
        }
        Some((
            Cow::Borrowed(&col.histogram),
            col.cm_sketch.as_ref().map(Arc::clone),
            col.top_n.as_ref().map(Arc::clone),
            col.fm_sketch.as_ref().map(Arc::clone),
        ))
    }

    /// IsAnalyzed checks whether the table is analyzed or not by checking its last analyze's timestamp value.
    /// A valid timestamp must be greater than 0.
    // go: Table.IsAnalyzed L1
    pub fn is_analyzed(&self) -> bool {
        self.last_analyze_version > 0
    }

    /// IsEligibleForAnalysis checks whether the table is eligible for analysis.
    // go: Table.IsEligibleForAnalysis L1
    pub fn is_eligible_for_analysis(t: Option<&Table>) -> bool {
        // 1. If the statistics are either not loaded or are classified as pseudo, there is no need for analyze.
        //    Pseudo statistics can be created by the optimizer, so we need to double check it.
        // 2. If the table is too small, we don't want to waste time to analyze it.
        //    Leave the opportunity to other bigger tables.
        if !Table::meet_auto_analyze_min_cnt(t) || t.is_some_and(|t| t.pseudo) {
            return false;
        }
        true
    }

    /// MeetAutoAnalyzeMinCnt checks whether the table meets the minimum count required for auto-analyze.
    // go: Table.MeetAutoAnalyzeMinCnt L1
    pub fn meet_auto_analyze_min_cnt(t: Option<&Table>) -> bool {
        t.is_some_and(|t| t.realtime_count >= AutoAnalyzeMinCnt.load(Ordering::Relaxed))
    }

    /// GetStatsHealthy calculates stats healthy if the table stats is not pseudo.
    /// If the table stats is pseudo, it returns 0, false, otherwise it returns stats healthy, true.
    ///
    /// Go's nil receiver is `t: None`.
    // go: Table.GetStatsHealthy L1 [M-F2I]
    pub fn get_stats_healthy(t: Option<&Table>) -> (i64, bool) {
        let Some(t) = t else {
            return (0, false);
        };
        if t.pseudo {
            return (0, false);
        }
        if !t.is_analyzed() {
            return (0, true);
        }
        let mut healthy: i64 = 0;
        let mut count = t.realtime_count as f64;
        let hist_count = t.get_analyze_row_count();
        if hist_count > 0.0 {
            count = hist_count;
        }
        if (t.modify_count as f64) < count {
            healthy =
                tidb_base::gostd::conv::f64_to_i64((1.0 - t.modify_count as f64 / count) * 100.0); // [M-F2I]
        } else if t.modify_count == 0 {
            healthy = 100;
        }
        (healthy, true)
    }

    /// IsInitialized returns true if any column/index stats of the table is initialized.
    // go: Table.IsInitialized L1
    pub fn is_initialized(&self) -> bool {
        for col in self.hist_coll.columns.values() {
            if col.is_stats_initialized() {
                return true;
            }
        }
        for idx in self.hist_coll.indices.values() {
            if idx.is_stats_initialized() {
                return true;
            }
        }
        false
    }

    /// IsOutdated returns true if the table stats is outdated.
    // go: Table.IsOutdated L1
    pub fn is_outdated(&self) -> bool {
        let mut rowcount = self.get_analyze_row_count();
        if rowcount < 0.0 {
            rowcount = self.realtime_count as f64;
        }
        if rowcount > 0.0 && self.modify_count as f64 / rowcount > ratio_of_pseudo_estimate() {
            return true;
        }
        false
    }

    /// ReleaseAndPutToPool releases data structures of Table and put itself back to pool.
    ///
    /// No pool (`M-POOL`); the maps are cleared (a shared map is replaced, not written).
    // go: Table.ReleaseAndPutToPool L1 [M-POOL]
    pub fn release_and_put_to_pool(&mut self) {
        self.hist_coll.columns = Arc::new(BTreeMap::new());
        self.hist_coll.indices = Arc::new(BTreeMap::new());
    }
}

/// RatioOfPseudoEstimate means if modifyCount / statsTblCount is greater than this ratio, we think the stats is invalid
/// and use pseudo estimation.
///
/// Go's `atomic.Float64` package variable: the bits of an `f64` (0.7).
pub static RatioOfPseudoEstimate: AtomicU64 = AtomicU64::new(0x3FE6_6666_6666_6666); // N13-allow: Go package variable (atomic)

/// `RatioOfPseudoEstimate.Load()`.
// glue: Go atomic.Float64.Load
pub fn ratio_of_pseudo_estimate() -> f64 {
    f64::from_bits(RatioOfPseudoEstimate.load(Ordering::Relaxed))
}

/// PseudoTable creates a pseudo table statistics.
/// Usually, we don't want to trigger stats loading for pseudo table.
/// But there are exceptional cases. In such cases, we should pass allowTriggerLoading as true.
/// Such case could possibly happen in getStatsTable().
///
/// `Column.Info` / `Index.Info` share the `TableInfo`'s `Arc`s (Go: the same pointers); the
/// column histograms copy the column's `FieldType` once (`&col.FieldType` in Go, N13 §8.2).
// go: PseudoTable L1 [M-SHARE]
pub fn pseudo_table(
    tbl_info: &TableInfo,
    allow_trigger_loading: bool,
    allow_fill_hist_meta: bool,
) -> Table {
    let tbl_columns = tbl_info.columns.as_deref().unwrap_or(&[]);
    let tbl_indices = tbl_info.indices.as_deref().unwrap_or(&[]);
    let pseudo_hist_coll = HistColl {
        realtime_count: PseudoRowCount,
        physical_id: tbl_info.id,
        have_physical_id: true,
        pseudo: true,
        can_not_trigger_load: !allow_trigger_loading,
        ..HistColl::default()
    };
    let mut existence = new_col_and_index_existence_map(tbl_columns.len(), tbl_indices.len());
    let mut columns = BTreeMap::new();
    let mut indices = BTreeMap::new();
    for col in tbl_columns {
        // The column is public to use. Also we should check the column is not hidden since hidden means that it's used by expression index.
        // We would not collect stats for the hidden column and we won't use the hidden column to estimate.
        // Thus we don't create pseudo stats for it.
        if col.state == tidb_model::StatePublic && !col.hidden {
            existence.insert_col(col.id, false);
            if allow_fill_hist_meta {
                let mut c = Column::with_histogram(new_histogram(
                    col.id,
                    0,
                    0,
                    0,
                    Arc::new(col.field_type.clone()),
                    0,
                    0,
                ));
                c.physical_id = tbl_info.id;
                c.info = Some(Arc::clone(col));
                c.is_handle =
                    tbl_info.pk_is_handle && tidb_base::mysql::has_pri_key_flag(col.get_flag());
                columns.insert(col.id, Arc::new(c));
            }
        }
    }
    for idx in tbl_indices {
        if idx.state == tidb_model::StatePublic {
            existence.insert_index(idx.id, false);
            if allow_fill_hist_meta {
                let mut i = Index::with_histogram(new_histogram(
                    idx.id,
                    0,
                    0,
                    0,
                    Arc::new(tidb_types::field_type::new_field_type(
                        tidb_base::mysql::TypeBlob,
                    )),
                    0,
                    0,
                ));
                i.physical_id = tbl_info.id;
                i.info = Some(Arc::clone(idx));
                indices.insert(idx.id, Arc::new(i));
            }
        }
    }
    let mut t = Table {
        hist_coll: pseudo_hist_coll,
        col_and_idx_existence_map: Some(Arc::new(existence)),
        ..Table::default()
    };
    t.hist_coll.columns = Arc::new(columns);
    t.hist_coll.indices = Arc::new(indices);
    t
}

// glue: stub of HistColl.SetAllIndexFullLoadForBootstrap, blocked-by:D11.2 (stats bootstrap)
// glue: stub of HistColl.DropEvicted, blocked-by:D11.2 (stats cache eviction; Go copies the table first, spec §3)
// glue: stub of Table.ColumnIsLoadNeeded, blocked-by:D11.2 (stats loading)
// glue: stub of Table.IndexIsLoadNeeded, blocked-by:D11.2 (stats loading)
// glue: stub of CheckAnalyzeVerOnTable, blocked-by:D11.1 (part 2: analyze / global stats)
