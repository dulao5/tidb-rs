//! go-file: pkg/planner/util/misc.go
//!
//! Mapping:
//! - Go's nil slice and empty slice are both the empty `Vec` (every caller only ranges over
//!   the clones).
//! - Expressions, columns and constants are immutable shared nodes (`ExprRef`,
//!   `Arc<Column>`; `M-EXPR`): Go's per-element `Clone()` shares the `Arc`.
//! - `*types.FieldName` is `Arc<FieldName>` (`types.NameSlice`); `CloneFieldNames` copies
//!   each name as Go does.

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_base::gostd::time::Time;
use tidb_expr_pushdown::infer_pushdown::{PushDownContext, new_push_down_context};
use tidb_expression::{Column, Constant, ExprRef, new_schema};
use tidb_kv::Handle;
use tidb_model::ColumnInfo;
use tidb_parser::model::CIStr;
use tidb_sessionctx::planctx::{BuildPBContext, PlanContext};
use tidb_sessionctx::ranger::types::Range;
use tidb_types::datum::Datum;
use tidb_types::field_name::FieldName;

use super::byitem::ByItems;
use super::handle_cols::HandleCols;
use crate::property::{SortItem, StatsInfo};

/// CloneFieldNames uses types.FieldName.Clone to clone a slice of types.FieldName.
// go: CloneFieldNames L1
pub fn clone_field_names(names: &[Arc<FieldName>]) -> Vec<Arc<FieldName>> {
    let mut cloned = Vec::with_capacity(names.len());
    for name in names {
        cloned.push(Arc::new((**name).clone()));
    }
    cloned
}

/// CloneCIStrs uses ast.CIStr.Clone to clone a slice of ast.CIStr.
// go: CloneCIStrs L1
pub fn clone_ci_strs(strs: &[CIStr]) -> Vec<CIStr> {
    strs.to_vec()
}

/// CloneExprs uses Expression.Clone to clone a slice of Expression.
// go: CloneExprs L1 [M-EXPR]
pub fn clone_exprs(exprs: &[ExprRef]) -> Vec<ExprRef> {
    let mut cloned = Vec::with_capacity(exprs.len());
    for e in exprs {
        cloned.push(Arc::clone(e));
    }
    cloned
}

/// CloneExpressions uses CloneExprs to clone a slice of expression.Expression.
// go: CloneExpressions L1
pub fn clone_expressions(exprs: &[ExprRef]) -> Vec<ExprRef> {
    clone_exprs(exprs)
}

/// CloneExpression2D uses CloneExprs to clone a 2D slice of expression.Expression.
// go: CloneExpression2D L1
pub fn clone_expression2_d(exprs: &[Vec<ExprRef>]) -> Vec<Vec<ExprRef>> {
    let mut cloned = Vec::with_capacity(exprs.len());
    for e in exprs {
        cloned.push(clone_exprs(e));
    }
    cloned
}

/// CloneScalarFunctions uses (*ScalarFunction).Clone to clone a slice of *ScalarFunction.
///
/// A `*ScalarFunction` is an `ExprRef` holding an `Expr::ScalarFunction`.
// go: CloneScalarFunctions L1 [M-EXPR]
pub fn clone_scalar_functions(scalar_funcs: &[ExprRef]) -> Vec<ExprRef> {
    let mut cloned = Vec::with_capacity(scalar_funcs.len());
    for f in scalar_funcs {
        cloned.push(Arc::clone(f));
    }
    cloned
}

/// CloneAssignments uses (*Assignment).Clone to clone a slice of *Assignment.
///
/// `expression.Assignment` (UPDATE / INSERT ... ON DUPLICATE KEY UPDATE) is not ported: the
/// write path is exempt from D08.1 (D08.1 tasks.md T6f) and comes with the DML builder.
// glue: stub of CloneAssignments, blocked-by:D09.15 (expression.Assignment, the write path)
pub fn clone_assignments() -> Result<(), Error> {
    Err(blocked(
        "D09.15",
        "CloneAssignments (expression.Assignment)",
    ))
}

/// CloneHandleCols uses HandleCols.Clone to clone a slice of HandleCols.
// go: CloneHandleCols L1
pub fn clone_handle_cols(handles: &[HandleCols]) -> Vec<HandleCols> {
    let mut cloned = Vec::with_capacity(handles.len());
    for h in handles {
        cloned.push(h.clone_cols());
    }
    cloned
}

/// A slot of a column slice that `CloneCols` copies: a column, or Go's possibly nil
/// `*Column` (`IndexInfo2FullCols` leaves nil entries).
pub trait ColSlot: Sized {
    /// `c.Clone()`, or nil for nil.
    // glue: M-EXPR (the element step of CloneCols for both slot types)
    fn clone_slot(&self) -> Self;
}

impl ColSlot for Arc<Column> {
    // glue: M-EXPR
    fn clone_slot(&self) -> Self {
        Arc::clone(self)
    }
}

impl ColSlot for Option<Arc<Column>> {
    // glue: M-EXPR
    fn clone_slot(&self) -> Self {
        self.as_ref().map(Arc::clone)
    }
}

/// CloneCols uses (*Column).Clone to clone a slice of *Column.
// go: CloneCols L1 [M-EXPR]
pub fn clone_cols<C: ColSlot>(cols: &[C]) -> Vec<C> {
    let mut cloned = Vec::with_capacity(cols.len());
    for c in cols {
        cloned.push(c.clone_slot());
    }
    cloned
}

/// CloneColumns uses CloneCols to clone a slice of expression.Column.
// go: CloneColumns L1
pub fn clone_columns<C: ColSlot>(cols: &[C]) -> Vec<C> {
    clone_cols(cols)
}

/// CloneConstants uses (*Constant).Clone to clone a slice of *Constant.
// go: CloneConstants L1 [M-EXPR]
pub fn clone_constants(constants: &[Arc<Constant>]) -> Vec<Arc<Constant>> {
    let mut cloned = Vec::with_capacity(constants.len());
    for c in constants {
        cloned.push(Arc::clone(c));
    }
    cloned
}

/// CloneConstant2D uses CloneConstants to clone a 2D slice of *Constant.
// go: CloneConstant2D L1
pub fn clone_constant2_d(constants: &[Vec<Arc<Constant>>]) -> Vec<Vec<Arc<Constant>>> {
    let mut cloned = Vec::with_capacity(constants.len());
    for c in constants {
        cloned.push(clone_constants(c));
    }
    cloned
}

/// CloneDatums uses Datum.Clone to clone a slice of Datum.
// go: CloneDatums L1
pub fn clone_datums(datums: &[Datum]) -> Vec<Datum> {
    let mut cloned = Vec::with_capacity(datums.len());
    for d in datums {
        cloned.push(d.clone());
    }
    cloned
}

/// CloneDatum2D uses CloneDatums to clone a 2D slice of Datum.
// go: CloneDatum2D L1
pub fn clone_datum2_d(datums: &[Vec<Datum>]) -> Vec<Vec<Datum>> {
    let mut cloned = Vec::with_capacity(datums.len());
    for d in datums {
        cloned.push(clone_datums(d));
    }
    cloned
}

/// CloneColInfos uses (*ColumnInfo).Clone to clone a slice of *ColumnInfo.
// go: CloneColInfos L1
pub fn clone_col_infos(cols: &[Arc<ColumnInfo>]) -> Vec<Arc<ColumnInfo>> {
    let mut cloned = Vec::with_capacity(cols.len());
    for c in cols {
        cloned.push(Arc::new((**c).clone()));
    }
    cloned
}

/// CloneRanges uses (*Range).Clone to clone a slice of *Range.
// go: CloneRanges L1
pub fn clone_ranges(ranges: &[Range]) -> Vec<Range> {
    let mut cloned = Vec::with_capacity(ranges.len());
    for r in ranges {
        cloned.push(r.clone());
    }
    cloned
}

/// CloneByItemss uses (*ByItems).Clone to clone a slice of *ByItems.
// go: CloneByItemss L1
pub fn clone_by_itemss(by_items: &[ByItems]) -> Vec<ByItems> {
    let mut cloned = Vec::with_capacity(by_items.len());
    for item in by_items {
        cloned.push(item.clone_item());
    }
    cloned
}

/// CloneSortItems uses SortItem.Clone to clone a slice of SortItem.
// go: CloneSortItems L1
pub fn clone_sort_items(items: &[SortItem]) -> Vec<SortItem> {
    let mut cloned = Vec::with_capacity(items.len());
    for item in items {
        cloned.push(item.clone_item());
    }
    cloned
}

/// CloneHandles uses Handle.Copy to clone a slice of Handle.
// go: CloneHandles L1
pub fn clone_handles(handles: &[Handle]) -> Vec<Handle> {
    let mut cloned = Vec::with_capacity(handles.len());
    for h in handles {
        cloned.push(h.copy());
    }
    cloned
}

/// QueryTimeRange represents a time range specified by TIME_RANGE hint
#[derive(Clone, Debug)]
pub struct QueryTimeRange {
    /// From.
    pub from: Time,
    /// To.
    pub to: Time,
}

/// MetricTableTimeFormat is the time format for metric table explain and format.
#[allow(non_upper_case_globals)]
pub const MetricTableTimeFormat: &str = "2006-01-02 15:04:05.999";

/// `t.Format(MetricTableTimeFormat)`: `2006-01-02 15:04:05` and, with `.999`, the
/// fraction of a second to at most 3 digits with trailing zeros (and an empty fraction's
/// dot) removed.
// go-std: time.Time.Format L2 (only the layout MetricTableTimeFormat)
fn format_metric_time(t: &Time) -> String {
    let (year, month, day) = t.date();
    let (hour, min, sec) = t.clock();
    let mut s = format!("{year:04}-{month:02}-{day:02} {hour:02}:{min:02}:{sec:02}");
    let mut frac = format!("{:03}", t.nanosecond() / 1_000_000);
    while frac.ends_with('0') {
        frac.pop();
    }
    if !frac.is_empty() {
        s.push('.');
        s.push_str(&frac);
    }
    s
}

impl QueryTimeRange {
    /// Condition returns a WHERE clause base on it's value
    // go: QueryTimeRange.Condition L1
    pub fn condition(&self) -> String {
        format!(
            "where time>='{}' and time<='{}'",
            format_metric_time(&self.from),
            format_metric_time(&self.to)
        )
    }

    /// MemoryUsage return the memory usage of QueryTimeRange
    // go: QueryTimeRange.MemoryUsage L2 (unsafe.Sizeof(QueryTimeRange{}) is Go's size on 64-bit targets)
    pub fn memory_usage(&self) -> i64 {
        EMPTY_QUERY_TIME_RANGE_SIZE
    }
}

/// `unsafe.Sizeof(QueryTimeRange{})` of Go v8.5.8 on amd64 / arm64 (two `time.Time`).
const EMPTY_QUERY_TIME_RANGE_SIZE: i64 = 48;

/// EncodeIntAsUint32 is used for LogicalPlan Interface
// go: EncodeIntAsUint32 L1 [M-INT]
pub fn encode_int_as_uint32(result: &mut Vec<u8>, value: isize) {
    result.extend_from_slice(&(value as u32).to_be_bytes());
}

/// GetMaxSortPrefix returns the prefix offset of sortCols in allCols.
// go: GetMaxSortPrefix L1
pub fn get_max_sort_prefix(sort_cols: &[Arc<Column>], all_cols: &[Arc<Column>]) -> Vec<isize> {
    let tmp_schema = new_schema(all_cols.to_vec());
    let mut sort_col_offsets = Vec::with_capacity(sort_cols.len());
    for sort_col in sort_cols {
        let offset = tmp_schema.column_index(sort_col);
        if offset == -1 {
            return sort_col_offsets;
        }
        sort_col_offsets.push(offset);
    }
    sort_col_offsets
}

/// DeriveLimitStats derives the stats of the top-n plan.
// go: DeriveLimitStats L1
pub fn derive_limit_stats(child_profile: &StatsInfo, limit_count: f64) -> StatsInfo {
    let mut stats = StatsInfo {
        row_count: go_min(limit_count, child_profile.row_count),
        col_ndvs: std::collections::HashMap::with_capacity(child_profile.col_ndvs.len()),
        // limit operation does not change the histogram (kind of sample).
        // HistColl: childProfile.HistColl (the field comes with D11.1, stats_info.rs)
        ..Default::default()
    };
    for (id, c) in &child_profile.col_ndvs {
        stats.col_ndvs.insert(*id, go_min(*c, stats.row_count));
    }
    stats
}

/// Go's `math.Min`: NaN if either is NaN, -0 is smaller than +0.
// go-std: math.Min L1
fn go_min(x: f64, y: f64) -> f64 {
    if x.is_infinite() && x < 0.0 || y.is_infinite() && y < 0.0 {
        return f64::NEG_INFINITY;
    }
    if x.is_nan() || y.is_nan() {
        return f64::NAN;
    }
    if x == 0.0 && x == y {
        if x.is_sign_negative() {
            return x;
        }
        return y;
    }
    if x < y { x } else { y }
}

/// ExtractTableAlias returns table alias of the base.LogicalPlan's columns.
/// It will return nil when there are multiple table alias, because the alias is only used to check if
/// the base.LogicalPlan Match some optimizer hints, and hints are not expected to take effect in this case.
///
/// The result type `*hint.HintedTable` and `SessionVars.PlannerSelectBlockAsName` are block
/// D13.3 (hint handling, VS1-24).
// glue: stub of ExtractTableAlias, blocked-by:D13.3 (hint.HintedTable, PlannerSelectBlockAsName)
pub fn extract_table_alias() -> Result<(), Error> {
    Err(blocked("D13.3", "ExtractTableAlias (hint.HintedTable)"))
}

/// GetPushDownCtx creates a PushDownContext from PlanContext
// go: GetPushDownCtx L1
pub fn get_push_down_ctx(pctx: &dyn PlanContext) -> PushDownContext<'static> {
    get_push_down_ctx_from_build_pb_context(&pctx.get_build_pb_ctx())
}

/// GetPushDownCtxFromBuildPBContext creates a PushDownContext from BuildPBContext
// go: GetPushDownCtxFromBuildPBContext L1
pub fn get_push_down_ctx_from_build_pb_context(bctx: &BuildPBContext) -> PushDownContext<'static> {
    new_push_down_context(
        bctx.get_expr_ctx().get_eval_ctx(),
        bctx.get_client(),
        bctx.in_explain_stmt,
        bctx.warn_handler.clone(),
        bctx.extra_warnghandler.clone(),
        bctx.group_concat_max_len,
    )
}
