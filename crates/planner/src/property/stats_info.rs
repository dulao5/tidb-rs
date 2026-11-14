//! go-file: pkg/planner/property/stats_info.go
//!
//! `StatsInfo.HistColl` (`*statistics.HistColl`) is not a field yet: the statistics
//! structures are block D11.1 (VS1-29), which adds it; `Scale` and `DeriveLimitStats` copy it
//! as Go does once it exists. Go's `map[int64]float64` is a `HashMap` (Go prints maps sorted
//! by key, so the `String` output does not depend on the iteration order, `M-MAPORDER`).

use std::collections::HashMap;
use std::sync::Arc;

use tidb_base::gofmt::{Arg, sprintf};
use tidb_base::gostd::conv::f64_to_i64;
use tidb_expression::{Column, sort_columns};

/// GroupNDV stores the NDV of a group of columns.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GroupNDV {
    /// Cols are the UniqueIDs of columns.
    pub cols: Vec<i64>,
    /// NDV.
    pub ndv: f64,
}

/// `%v` of a `GroupNDV` value (a struct without methods: `{[1 2] 3}`).
// glue: Go fmt argument of a GroupNDV
fn group_ndv_arg(g: &GroupNDV) -> Arg {
    Arg::Struct(
        vec![
            Arg::Array(
                g.cols.iter().map(|c| Arg::Int(*c, "int64")).collect(),
                "[]int64",
            ),
            Arg::Float(g.ndv, 64),
        ],
        "property.GroupNDV",
    )
}

/// ToString prints GroupNDV slice. It is only used for test.
// go: ToString L1
pub fn to_string(ndvs: &[GroupNDV]) -> String {
    let arg = Arg::Array(
        ndvs.iter().map(group_ndv_arg).collect(),
        "[]property.GroupNDV",
    );
    String::from_utf8_lossy(&sprintf("%v", &[arg])).into_owned()
}

/// StatsInfo stores the basic information of statistics for the plan's output. It is used for cost estimation.
#[derive(Clone, Debug, Default)]
pub struct StatsInfo {
    /// RowCount.
    pub row_count: f64,

    /// Column.UniqueID -> NDV
    pub col_ndvs: HashMap<i64, f64>,

    // HistColl *statistics.HistColl: added with D11.1 (VS1-29), see the module doc.
    /// StatsVersion indicates the statistics version of a table.
    /// If the StatsInfo is calculated using the pseudo statistics on a table, StatsVersion will be PseudoVersion.
    pub stats_version: u64,

    /// GroupNDVs stores the NDV of column groups.
    pub group_ndvs: Vec<GroupNDV>,
}

/// Go's `%v` of a `map[int64]float64`: `map[k:v k:v]`, keys sorted.
// glue: Go fmt of a map (fmt sorts map keys)
fn map_string(m: &HashMap<i64, f64>) -> Vec<u8> {
    let mut keys: Vec<&i64> = m.keys().collect();
    keys.sort();
    let mut out = b"map[".to_vec();
    for (i, k) in keys.into_iter().enumerate() {
        if i > 0 {
            out.push(b' ');
        }
        out.extend(sprintf(
            "%v:%v",
            &[Arg::Int(*k, "int64"), Arg::Float(m[k], 64)],
        ));
    }
    out.push(b']');
    out
}

impl StatsInfo {
    /// String implements fmt.Stringer interface.
    // go: StatsInfo.String L1 [M-NAME]
    pub fn to_string(&self) -> Vec<u8> {
        sprintf(
            "count %v, ColNDVs %s",
            &[
                Arg::Float(self.row_count, 64),
                Arg::Bytes(map_string(&self.col_ndvs)),
            ],
        )
    }

    /// Count gets the RowCount in the StatsInfo.
    // go: StatsInfo.Count L1 [M-F2I]
    pub fn count(&self) -> i64 {
        f64_to_i64(self.row_count) // [M-F2I]
    }

    /// Scale receives a selectivity and multiplies it with RowCount and NDV.
    // go: StatsInfo.Scale L1
    pub fn scale(&self, factor: f64) -> StatsInfo {
        let mut profile = StatsInfo {
            row_count: self.row_count * factor,
            col_ndvs: HashMap::with_capacity(self.col_ndvs.len()),
            stats_version: self.stats_version,
            group_ndvs: Vec::with_capacity(self.group_ndvs.len()),
        };
        for (id, c) in &self.col_ndvs {
            profile.col_ndvs.insert(*id, c * factor);
        }
        for g in &self.group_ndvs {
            let mut g2 = g.clone();
            g2.ndv = g.ndv * factor;
            profile.group_ndvs.push(g2);
        }
        profile
    }

    /// ScaleByExpectCnt tries to Scale StatsInfo to an expectCnt which must be
    /// smaller than the derived cnt.
    /// TODO: try to use a better way to do this.
    ///
    /// Go returns `s` itself when it does not scale; the `Arc` is returned as is.
    // go: StatsInfo.ScaleByExpectCnt L1
    pub fn scale_by_expect_cnt(self: &Arc<Self>, expect_cnt: f64) -> Arc<StatsInfo> {
        if expect_cnt >= self.row_count {
            return Arc::clone(self);
        }
        if self.row_count > 1.0 {
            // if s.RowCount is too small, it will cause overflow
            return Arc::new(self.scale(expect_cnt / self.row_count));
        }
        Arc::clone(self)
    }

    /// GetGroupNDV4Cols gets the GroupNDV for the given columns.
    ///
    /// Go's nil receiver is `s: None`.
    // go: StatsInfo.GetGroupNDV4Cols L1
    pub fn get_group_ndv4_cols<'a>(
        s: Option<&'a StatsInfo>,
        cols: &[Arc<Column>],
    ) -> Option<&'a GroupNDV> {
        let s = s?;
        if cols.is_empty() || s.group_ndvs.is_empty() {
            return None;
        }
        let cols = sort_columns(cols);
        for group_ndv in &s.group_ndvs {
            if cols.len() != group_ndv.cols.len() {
                continue;
            }
            let mut matched = true;
            for (i, col) in group_ndv.cols.iter().enumerate() {
                if *col != cols[i].unique_id {
                    matched = false;
                    break;
                }
            }
            if matched {
                return Some(group_ndv);
            }
        }
        None
    }
}
