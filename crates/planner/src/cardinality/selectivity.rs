//! go-file: pkg/planner/cardinality/selectivity.go
//!
//! `Selectivity` and its helpers (D10.6, VS1-29).
//!
//! Boundaries (specs/D10.6-cardinality §1.2):
//! - building ranges is [`crate::ranger`] (D10.4, VS1-25); its own boundary is the IN
//!   builtin (D08.9) when it rebuilds an IN condition;
//! - MV index paths (`CollectFilters4MVIndex`, `BuildPartialPaths4MVIndex`) are D10.7;
//! - TopN-assisted estimation (`GetSelectivityByFilter`), the index row counts from real
//!   statistics (`getEqualCondSelectivity`, `crossValidationSelectivity`) and the traces are
//!   D10.6 part 2.
//! - Go iterates the not-covered maps in random order; here the expressions' order
//!   (`M-MAPORDER`): the products can differ in the last bit when two DNF items are
//!   multiplied in another order, which Go itself does from run to run (spec §2).

// Go package-level names (pseudoEqualRate, TypeLonglong, ...), mapping rule M-GONAME.
#![allow(non_upper_case_globals)]

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_expression::scalar_function::ScalarFunction;
use tidb_expression::util::{
    extract_columns, extract_columns_from_expressions, get_expr_inside_is_truth,
};
use tidb_expression::{
    Column, Constant, Expr, ExprRef, ast_funcs as ast, flatten_cnf_conditions,
    flatten_dnf_conditions,
};
use tidb_model::IndexInfo;
use tidb_parser::types::UnspecifiedLength;
use tidb_sessionctx::ranger::types::Range;
use tidb_statistics::{HistColl, Histogram, column_stats_is_invalid, index_stats_is_invalid};
use tidb_types::datum::Datum;

use super::pseudo::{pseudo_selectivity, pseudoEqualRate, selectionFactor};
use super::row_count_column::{get_row_count_by_column_ranges, get_row_count_by_int_column_ranges};
use super::row_count_index::get_row_count_by_index_ranges;
use super::trace::{StatsItem, check_no_trace, record_used_item_stats_status};
use crate::PlanCtx;
use crate::ranger::{self, RangeType};
use crate::util::path::AccessPath;

/// `outOfRangeBetweenRate` (selectivity.go:42), a package variable in Go.
const outOfRangeBetweenRate: i64 = 100;

/// Selectivity is a function calculate the selectivity of the expressions on the specified HistColl.
/// The definition of selectivity is (row count after filter / row count before filter).
/// And exprs must be CNF now, in other words, `exprs[0] and exprs[1] and ... and exprs[len - 1]`
/// should be held when you call this.
/// Currently, the time complexity is o(n^2).
///
/// The returned nodes are Go's `retStatsNodes` (sorted by `GetUsableSetsByGreedy`, as Go
/// sorts the slice in place).
// go: Selectivity L1 [M-FMA, M-MAPORDER]
pub fn selectivity(
    ctx: &PlanCtx<'_>,
    coll: &HistColl,
    exprs: &[ExprRef],
    filled_paths: &[AccessPath],
) -> Result<(f64, Vec<StatsNode>), Error> {
    check_no_trace(ctx)?;
    // If the table's count is zero or conditions are empty, we should return 100% selectivity.
    if coll.realtime_count == 0 || exprs.is_empty() {
        return Ok((1.0, Vec::new()));
    }
    let mut ret = 1.0;
    let vars = &*ctx.vars;
    let table_id = coll.physical_id;
    // TODO: If len(exprs) is bigger than 63, we could use bitset structure to replace the int64.
    // This will simplify some code and speed up if we use this rather than a boolean slice.
    if exprs.len() > 63 || (coll.col_num() == 0 && coll.idx_num() == 0) {
        ret = pseudo_selectivity(ctx, coll, exprs);
        return Ok((ret, Vec::new()));
    }

    let mut nodes: Vec<StatsNode> = Vec::new();
    let mut remained_exprs: Vec<ExprRef> = Vec::with_capacity(exprs.len());

    // Deal with the correlated column.
    for expr in exprs {
        let Some(c) = is_col_eq_cor_col(expr) else {
            remained_exprs.push(Arc::clone(expr));
            continue;
        };

        let col_hist = coll.get_col(c.unique_id).map(Arc::as_ref);
        let sel = if column_stats_is_invalid(col_hist, Some(vars), coll, c.id) {
            1.0 / pseudoEqualRate
        } else if col_hist.expect("valid column stats").histogram.ndv > 0 {
            1.0 / col_hist.expect("valid column stats").histogram.ndv as f64
        } else {
            1.0 / pseudoEqualRate
        };
        ret *= sel;
    }

    let mut extracted_cols: Vec<Arc<Column>> = Vec::with_capacity(coll.col_num());
    extracted_cols = extract_columns_from_expressions(extracted_cols, &remained_exprs, None);
    extracted_cols.sort_by_key(|c| c.id);
    extracted_cols.dedup_by(|b, a| a.id == b.id);
    for col in &extracted_cols {
        if col.is_hidden && col.virtual_expr.is_some() {
            // For expression index, only the index stats should be used,
            // column stats can be ignored.
            continue;
        }
        let id = col.unique_id;
        if let Some(col_stats) = coll.get_col(id) {
            let (mask_covered, ranges, _, _) = get_mask_and_ranges(
                ctx,
                &remained_exprs,
                RangeType::ColumnRangeType,
                &[],
                None,
                std::slice::from_ref(col),
            )?;
            let mut node = StatsNode {
                tp: ColType,
                id,
                mask: mask_covered,
                ranges,
                num_cols: 1,
                ..StatsNode::default()
            };
            if col_stats.is_handle {
                node.tp = PkType;
                let cnt = get_row_count_by_int_column_ranges(ctx, coll, id, &node.ranges)?;
                node.selectivity = cnt / coll.realtime_count as f64;
                nodes.push(node);
                continue;
            }
            let cnt = get_row_count_by_column_ranges(ctx, coll, id, &node.ranges)?;
            node.selectivity = cnt / coll.realtime_count as f64;
            nodes.push(node);
        } else if !col.is_hidden {
            // TODO: We are able to remove this path if we remove the async stats load.
            column_stats_is_invalid(None, Some(vars), coll, col.id);
            record_used_item_stats_status(ctx, StatsItem::Column(None), table_id, col.id);
        }
    }
    let mut id2_paths: HashMap<i64, &AccessPath> = HashMap::new();
    for path in filled_paths {
        // Index merge path and table path don't have an index.
        let Some(index) = &path.index else {
            continue;
        };
        id2_paths.insert(index.id, path);
    }
    let mut idx_ids: Vec<i64> = Vec::with_capacity(coll.idx_num());
    coll.for_each_index_immutable(|id, _| {
        idx_ids.push(id);
        false
    });
    // Stabilize the result.
    idx_ids.sort();
    for id in idx_ids {
        let idx_stats = coll.get_idx(id).expect("index stats");
        let idx_info: &Arc<IndexInfo> = idx_stats.info.as_ref().expect("Index.Info");
        let idx_info_cols = idx_info.columns.as_deref().unwrap_or(&[]);
        if idx_info.mv_index {
            let Some((total_selectivity, mask)) =
                get_mask_and_selectivity_for_mv_index(ctx, coll, id, &remained_exprs)?
            else {
                continue;
            };
            nodes.push(StatsNode {
                tp: IndexType,
                id,
                mask,
                num_cols: idx_info_cols.len(),
                selectivity: total_selectivity,
                ..StatsNode::default()
            });
            continue;
        }
        let idx_cols = find_prefix_of_index_by_col(
            ctx,
            &extracted_cols,
            coll.idx2_col_unique_ids.get(&id).map_or(&[], Vec::as_slice),
            id2_paths.get(&idx_stats.histogram.id).copied(),
        );
        if !idx_cols.is_empty() {
            let mut lengths: Vec<isize> = Vec::with_capacity(idx_cols.len());
            let mut i = 0;
            while i < idx_cols.len() && i < idx_info_cols.len() {
                lengths.push(idx_info_cols[i].length);
                i += 1;
            }
            // If the found columns are more than the columns held by the index. We are appending the int pk to the tail of it.
            // When storing index data to key-value store, we use (idx_col1, ...., idx_coln, handle_col) as its key.
            if idx_cols.len() > idx_info_cols.len() {
                lengths.push(tidb_parser::types::UnspecifiedLength);
            }
            let (mask_covered, ranges, part_cover, min_access_conds_for_dnf_cond) =
                get_mask_and_ranges(
                    ctx,
                    &remained_exprs,
                    RangeType::IndexRangeType,
                    &lengths,
                    id2_paths.get(&idx_stats.histogram.id).copied(),
                    &idx_cols,
                )?;
            let cnt = get_row_count_by_index_ranges(ctx, coll, id, &ranges, &idx_cols)?;
            let selectivity = cnt / coll.realtime_count as f64;
            nodes.push(StatsNode {
                tp: IndexType,
                id,
                mask: mask_covered,
                ranges,
                num_cols: idx_info_cols.len(),
                selectivity,
                part_cover,
                min_access_conds_for_dnf_cond,
            });
        }
    }
    let used_sets = get_usable_sets_by_greedy(&mut nodes);
    // Initialize the mask with the full set.
    let mut mask: i64 = 1i64
        .wrapping_shl(remained_exprs.len() as u32)
        .wrapping_sub(1);

    for &set in &used_sets {
        let set = &nodes[set];
        mask &= !set.mask;
        ret *= set.selectivity;
        // If `partCover` is true, it means that the conditions are in DNF form, and only part
        // of the DNF expressions are extracted as access conditions, so besides from the selectivity
        // of the extracted access conditions, we multiply another selectionFactor for the residual
        // conditions.
        if set.part_cover {
            ret *= selectionFactor;
        }
    }

    // Go's maps keyed by the expression's position (iterated in position order here).
    let mut not_covered_constants: BTreeMap<usize, &Constant> = BTreeMap::new();
    let mut not_covered_dnf: BTreeMap<usize, &ScalarFunction> = BTreeMap::new();
    let mut not_covered_str_match: BTreeMap<usize, &ScalarFunction> = BTreeMap::new();
    let mut not_covered_negate_str_match: BTreeMap<usize, &ScalarFunction> = BTreeMap::new();
    let mut not_covered_other_expr: BTreeMap<usize, &ExprRef> = BTreeMap::new();
    if mask > 0 {
        for (i, expr) in remained_exprs.iter().enumerate() {
            if mask & (1i64 << i) == 0 {
                continue;
            }
            match &**expr {
                Expr::Constant(x) => {
                    not_covered_constants.insert(i, x);
                    continue;
                }
                Expr::ScalarFunction(x) => match x.func_name.l.as_str() {
                    ast::LogicOr => {
                        not_covered_dnf.insert(i, x);
                        continue;
                    }
                    ast::Like | ast::Ilike | ast::Regexp | ast::RegexpLike => {
                        not_covered_str_match.insert(i, x);
                        continue;
                    }
                    ast::UnaryNot => {
                        let inner = get_expr_inside_is_truth(&x.get_args()[0]);
                        if let Expr::ScalarFunction(inner_sf) = &*inner
                            && matches!(
                                inner_sf.func_name.l.as_str(),
                                ast::Like | ast::Ilike | ast::Regexp | ast::RegexpLike
                            )
                        {
                            not_covered_negate_str_match.insert(i, x);
                            continue;
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
            not_covered_other_expr.insert(i, expr);
        }
    }

    // Try to cover remaining Constants
    let build_ctx: &dyn tidb_sessionctx::exprctx::BuildContext = &*ctx.expr_ctx;
    let const_positions: Vec<usize> = not_covered_constants.keys().copied().collect();
    for i in const_positions {
        let c = not_covered_constants[&i];
        if tidb_expression::constant_fold::maybe_over_optimized4_plan_cache(
            build_ctx,
            &[Arc::new(Expr::Constant(c.clone()))],
        ) {
            continue;
        }
        if c.value.is_null() {
            // c is null
            ret *= 0.0;
            mask &= !(1i64 << i);
            not_covered_constants.remove(&i);
        } else {
            let (is_true, err) = c.value.to_bool(&vars.stmt_ctx().type_ctx());
            if err.is_ok() {
                if is_true == 0 {
                    // c is false
                    ret *= 0.0;
                }
                // c is true, no need to change ret
                mask &= !(1i64 << i);
                not_covered_constants.remove(&i);
            }
        }
        // Not expected to come here:
        // err != nil, no need to do anything.
    }

    // Try to cover remaining DNF conditions using independence assumption,
    // i.e., sel(condA or condB) = sel(condA) + sel(condB) - sel(condA) * sel(condB)
    let dnf_positions: Vec<usize> = not_covered_dnf.keys().copied().collect();
    'outer: for i in dnf_positions {
        let scalar_cond = not_covered_dnf[&i];
        // If there are columns not in stats, we won't handle them. This case might happen after DDL operations.
        let cols = extract_columns(&remained_exprs[i]);
        for c in &cols {
            if coll.get_col(c.unique_id).is_none() {
                continue 'outer;
            }
        }

        let dnf_items = flatten_dnf_conditions(scalar_cond);
        let dnf_items = ranger::merge_dnf_items4_col(&ctx.ranger_ctx(), &dnf_items);
        // If the conditions only contain a single column, we won't handle them.
        if dnf_items.len() <= 1 {
            continue;
        }

        let mut selectivity = 0.0;
        for cond in &dnf_items {
            // In selectivity calculation, we don't handle CorrelatedColumn, so we directly skip over it.
            // Other kinds of `Expression`, i.e., Constant, Column and ScalarFunction all can possibly be built into
            // ranges and used to calculation selectivity, so we accept them all.
            if let Expr::CorrelatedColumn(_) = &**cond {
                continue;
            }

            let cnf_items: Vec<ExprRef> = match &**cond {
                Expr::ScalarFunction(scalar) if scalar.func_name.l == ast::LogicAnd => {
                    flatten_cnf_conditions(scalar)
                }
                _ => vec![Arc::clone(cond)],
            };

            let cur_selectivity = match selectivity_fn(ctx, coll, &cnf_items, &[]) {
                Ok((s, _)) => s,
                // A boundary of the port is not "something wrong": it is returned as is.
                Err(e) if tidb_base::blocked::blocked_by(&e).is_some() => return Err(e),
                Err(e) => {
                    tidb_base::log_debug!(
                        "something wrong happened, use the default selectivity",
                        "error" => &e,
                    );
                    selectionFactor
                }
            };

            // [M-FMA] selectivity.go:370
            selectivity = msub(selectivity + cur_selectivity, selectivity, cur_selectivity);
        }

        if selectivity != 0.0 {
            ret *= selectivity;
            mask &= !(1i64 << i);
            not_covered_dnf.remove(&i);
        }
    }

    // Try to cover remaining string matching functions by evaluating the expressions with TopN to estimate.
    if vars.enable_eval_top_n_estimation_for_str_match()
        && (!not_covered_str_match.is_empty() || !not_covered_negate_str_match.is_empty())
    {
        // GetSelectivityByFilter is D10.6 part 2 (TopN-assisted estimation).
        return Err(blocked("D10.6", "GetSelectivityByFilter"));
    }

    // At last, if there are still conditions which cannot be estimated, we multiply the selectivity with
    // the minimal default selectivity of the remaining conditions.
    // Currently, only string matching functions (like and regexp) may have a different default selectivity,
    // other expressions' default selectivity is selectionFactor.
    if mask > 0 {
        let mut min_selectivity = 1.0;
        if !not_covered_constants.is_empty()
            || !not_covered_dnf.is_empty()
            || !not_covered_other_expr.is_empty()
        {
            min_selectivity = tidb_base::gostd::math::min(min_selectivity, selectionFactor);
        }
        if !not_covered_str_match.is_empty() {
            min_selectivity = tidb_base::gostd::math::min(
                min_selectivity,
                vars.get_str_match_default_selectivity(),
            );
        }
        if !not_covered_negate_str_match.is_empty() {
            min_selectivity = tidb_base::gostd::math::min(
                min_selectivity,
                vars.get_negate_str_match_default_selectivity(),
            );
        }
        ret *= min_selectivity;
    }

    Ok((ret, nodes))
}

/// The recursive call of `Selectivity` on a DNF item.
// glue: recursion helper (the public name `selectivity` is shadowed by the local variable)
fn selectivity_fn(
    ctx: &PlanCtx<'_>,
    coll: &HistColl,
    exprs: &[ExprRef],
    filled_paths: &[AccessPath],
) -> Result<(f64, Vec<StatsNode>), Error> {
    selectivity(ctx, coll, exprs, filled_paths)
}

/// `z - x * y` the way Go gc compiles selectivity.go:370 and :535: fused (`FMSUBD`) on
/// arm64, riscv64, loong64 only — unlike `madd` (`z + x * y`), `msub` is not fused on amd64
/// (GOAMD64=v1), ppc64le, or s390x (mapping rule `M-FMA`, per the tracker's `go_fma_sites.tsv`).
// glue: M-FMA helper
#[inline(always)]
fn msub(z: f64, x: f64, y: f64) -> f64 {
    #[cfg(any(
        target_arch = "aarch64",
        target_arch = "riscv64",
        target_arch = "loongarch64"
    ))]
    {
        (-x).mul_add(y, z)
    }
    #[cfg(not(any(
        target_arch = "aarch64",
        target_arch = "riscv64",
        target_arch = "loongarch64"
    )))]
    {
        z - x * y
    }
}

/// CalcTotalSelectivityForMVIdxPath calculates the total selectivity for the given partial paths of an MV index merge path.
/// It corresponds with the meaning of AccessPath.CountAfterAccess, as used in buildPartialPathUp4MVIndex.
/// It uses the independence assumption to estimate the selectivity.
// go: CalcTotalSelectivityForMVIdxPath L1 [M-FMA]
pub fn calc_total_selectivity_for_mv_idx_path(
    coll: &HistColl,
    partial_paths: &[AccessPath],
    is_intersection: bool,
) -> f64 {
    let mut selectivities: Vec<f64> = Vec::with_capacity(partial_paths.len());
    for path in partial_paths {
        // For a partial path, we distinguish between two cases if it's a mv index path.
        // 1. We will access a single value on the virtual column of the mv index.
        //   In this case, handles from a single partial path must be unique.
        //   The CountAfterAccess of a partial path will never be larger than the table total row count.
        //   For an index merge path with only one partial path, the CountAfterAccess will be exactly the same as the
        //   CountAfterAccess of the partial path (currently there's no index filter for partial path of mv index merge
        //   path).
        // 2. We use the mv index as if it's a non-MV index, which means the virtual column is not involved in the access
        //   conditions.
        //   In this case, we may read repeated handles from a single partial path.
        //   The CountAfterAccess of a partial path might be larger than the table total row count.
        //   For an index merge path with only one partial path, the CountAfterAccess might be less than the CountAfterAccess
        //   of the partial path
        // (Go's example and the reference to upstream issue `50125` are in the Go source.)
        let mut realtime_count = coll.realtime_count;
        if !path.is_table_path()
            && let Some(index) = &path.index
            && index.mv_index
        {
            let mut virtual_col: Option<&Arc<Column>> = None;
            if let Some(cols) = coll.mv_idx2_columns.get(&index.id) {
                for col in cols {
                    if col.virtual_expr.is_some() {
                        virtual_col = Some(col);
                        break;
                    }
                }
            }
            let filter = |column: &Column| -> bool {
                virtual_col.is_some_and(|v| column.unique_id == v.unique_id)
            };
            let cols =
                extract_columns_from_expressions(Vec::new(), &path.access_conds, Some(&filter));
            // If we can't find the virtual column from the access conditions, it's the case 2.
            if cols.is_empty() {
                (realtime_count, _) = coll
                    .get_scaled_realtime_and_modify_cnt(coll.get_idx(index.id).map(Arc::as_ref));
            }
        }
        let mut sel = path.count_after_access / realtime_count as f64;
        sel = clamp(sel, 0.0, 1.0);
        selectivities.push(sel);
    }
    let mut total_selectivity;
    if is_intersection {
        total_selectivity = 1.0;
        for sel in &selectivities {
            total_selectivity *= sel;
        }
    } else {
        total_selectivity = 0.0;
        for &sel in &selectivities {
            // [M-FMA] selectivity.go:535
            total_selectivity = msub(sel + total_selectivity, total_selectivity, sel);
        }
    }
    total_selectivity
}

/// Go's `mathutil.Clamp` for `float64`.
// glue: Go util/mathutil.Clamp (D00.3 leaf, generic min / max)
pub(crate) fn clamp(n: f64, min: f64, max: f64) -> f64 {
    if n >= max {
        return max;
    }
    if n <= min {
        return min;
    }
    n
}

/// StatsNode is used for calculating selectivity.
#[derive(Clone, Debug, Default)]
pub struct StatsNode {
    /// Ranges contains all the Ranges we got.
    pub ranges: Vec<Range>,
    /// Tp.
    pub tp: isize,
    /// ID.
    pub id: i64,
    /// mask is a bit pattern whose ith bit will indicate whether the ith expression is covered by this index/column.
    pub(crate) mask: i64,
    /// Selectivity indicates the Selectivity of this column/index.
    pub selectivity: f64,
    /// numCols is the number of columns contained in the index or column(which is always 1).
    pub(crate) num_cols: usize,
    /// partCover indicates whether the bit in the mask is for a full cover or partial cover. It is only true
    /// when the condition is a DNF expression on index, and the expression is not totally extracted as access condition.
    pub(crate) part_cover: bool,
    /// Please see comments of planner/util.AccessPath.MinAccessCondsForDNFCond for more details.
    pub(crate) min_access_conds_for_dnf_cond: isize,
}

/// The type of the StatsNode.
pub const IndexType: isize = 0;
/// PkType.
pub const PkType: isize = 1;
/// ColType.
pub const ColType: isize = 2;

/// compareType.
// go: compareType L1
fn compare_type(l: isize, r: isize) -> isize {
    if l == r {
        return 0;
    }
    if l == ColType {
        return -1;
    }
    if l == PkType {
        return 1;
    }
    if r == ColType {
        return 1;
    }
    -1
}

/// `unknownColumnID` (selectivity.go:583).
pub(crate) const unknownColumnID: i64 = i64::MIN;

/// staleLastBucketThreshold is the threshold for detecting stale last bucket estimates.
/// If the last bucket's count is less than 30% of the average bucket count, we consider
/// it potentially stale.
const staleLastBucketThreshold: f64 = 0.3;

/// valueAwareRowAddedThreshold is the minimum ratio of newly added rows relative to
/// the average value count required to trigger the stale bucket heuristic.
/// If new rows >= (avgValueCount * threshold), we consider applying the heuristic.
const valueAwareRowAddedThreshold: f64 = 0.5;

/// IsLastBucketEndValueUnderrepresented detects when the last value (upper bound) of the last bucket
/// has a suspiciously low count that may be stale due to concentrated writes after ANALYZE.
///
/// Go's `sctx` is only for the debug trace of `LocateBucket`.
// go: IsLastBucketEndValueUnderrepresented L1
pub fn is_last_bucket_end_value_underrepresented(
    hg: &Histogram,
    val: &Datum,
    hist_cnt: f64,
    hist_ndv: f64,
    realtime_row_count: i64,
    modify_count: i64,
) -> bool {
    if modify_count <= 0 || hg.buckets.is_empty() || hist_ndv <= 0.0 {
        return false;
    }

    // This represents data changes since ANALYZE - we use absolute difference as a proxy for
    // activity level since we cannot distinguish between inserts, deletes, and updates
    let new_rows_added = hg.abs_row_count_difference(realtime_row_count);

    // Calculate average count per distinct value
    let avg_value_count = hg.not_null_count() / hist_ndv;

    // Only apply heuristic when new rows are significant relative to average value count
    if new_rows_added < avg_value_count * valueAwareRowAddedThreshold {
        return false;
    }

    // Use LocateBucket to check if this value is at the last bucket's upper bound
    let (_, bucket_idx, in_bucket, match_last_value) = hg.locate_bucket(val);

    // Check if this is the last bucket's upper bound value (end value)
    let is_last_bucket_end_value =
        (bucket_idx == hg.buckets.len() - 1) && in_bucket && match_last_value;
    if !is_last_bucket_end_value {
        return false;
    }

    // If count is much less than average value count, it's likely underrepresented
    hist_cnt < avg_value_count * staleLastBucketThreshold
}

/// getConstantColumnID receives two expressions and if one of them is column and another is constant, it returns the
/// ID of the column.
// go: getConstantColumnID L1
pub(crate) fn get_constant_column_id(e: &[ExprRef]) -> i64 {
    if e.len() != 2 {
        return unknownColumnID;
    }
    if let (Expr::Column(col), Expr::Constant(_)) = (&*e[0], &*e[1]) {
        return col.id;
    }
    if let (Expr::Column(col), Expr::Constant(_)) = (&*e[1], &*e[0]) {
        return col.id;
    }
    unknownColumnID
}

/// GetUsableSetsByGreedy will select the indices and pk used for calculate selectivity by greedy algorithm.
///
/// Go sorts `nodes` in place and returns pointers into it; here the positions in the sorted
/// `nodes`.
// go: GetUsableSetsByGreedy L2 (positions instead of pointers)
pub fn get_usable_sets_by_greedy(nodes: &mut [StatsNode]) -> Vec<usize> {
    // Go's slices.SortFunc is not stable; two nodes compare equal only with the same type and
    // id, which Selectivity never produces.
    nodes.sort_by(|i, j| {
        let r = compare_type(i.tp, j.tp);
        if r != 0 {
            return r.cmp(&0);
        }
        i.id.cmp(&j.id)
    });
    let mut new_blocks = Vec::new();
    let mut marked = vec![false; nodes.len()];
    let mut mask: i64 = i64::MAX;
    loop {
        // Choose the index that covers most.
        let mut best_mask: i64 = 0;
        let best_default = StatsNode {
            tp: ColType,
            selectivity: 0.0,
            num_cols: 0,
            part_cover: true,
            min_access_conds_for_dnf_cond: -1,
            ..StatsNode::default()
        };
        let mut best = StatsNodeForGreedyChoice {
            stats_node: &best_default,
            idx: -1,
            cover_count: 0,
        };
        for (i, set) in nodes.iter().enumerate() {
            if marked[i] {
                continue;
            }
            let cur_mask = set.mask & mask;
            if cur_mask != set.mask {
                marked[i] = true;
                continue;
            }
            let bits = (cur_mask as u64).count_ones() as usize;
            // This set cannot cover any thing, just skip it.
            if bits == 0 {
                marked[i] = true;
                continue;
            }
            let current = StatsNodeForGreedyChoice {
                stats_node: set,
                idx: i as isize,
                cover_count: bits,
            };
            if current.is_better_than(&best) {
                best = current;
                best_mask = cur_mask;
            }
        }
        if best.cover_count == 0 {
            break;
        }

        // Update the mask, remove the bit that nodes[best.idx].mask has.
        mask &= !best_mask;

        new_blocks.push(best.idx as usize);
        marked[best.idx as usize] = true;
    }
    new_blocks
}

/// statsNodeForGreedyChoice.
struct StatsNodeForGreedyChoice<'a> {
    stats_node: &'a StatsNode,
    idx: isize,
    cover_count: usize,
}

impl StatsNodeForGreedyChoice<'_> {
    /// isBetterThan.
    // go: statsNodeForGreedyChoice.isBetterThan L1
    fn is_better_than(&self, other: &StatsNodeForGreedyChoice<'_>) -> bool {
        let (s, o) = (self.stats_node, other.stats_node);
        // 1. The stats type, always prefer the primary key or index.
        if s.tp != ColType && o.tp == ColType {
            return true;
        }
        // 2. The number of expression that it covers, the more, the better.
        if self.cover_count > other.cover_count {
            return true;
        }
        // Worse or equal. We return false for both cases. The same for the following rules.
        if self.cover_count != other.cover_count {
            return false;
        }
        // 3. It's only for DNF. Full cover is better than partial cover
        if !s.part_cover && o.part_cover {
            return true;
        }
        if s.part_cover != o.part_cover {
            return false;
        }
        // 4. It's only for DNF. The minimum number of access conditions among all DNF items, the more, the better.
        // s.coverCount is not enough for DNF, so we use this field to make the judgment more accurate.
        if s.min_access_conds_for_dnf_cond > o.min_access_conds_for_dnf_cond {
            return true;
        }
        if s.min_access_conds_for_dnf_cond != o.min_access_conds_for_dnf_cond {
            return false;
        }

        // 5. The number of columns that it contains, the less, the better.
        if s.num_cols < o.num_cols {
            return true;
        }
        if s.num_cols != o.num_cols {
            return false;
        }
        // 6. The selectivity of the covered conditions, the less, the better.
        // The rationale behind is that lower selectivity tends to reflect more functional dependencies
        // between columns. It's hard to decide the priority of this rule against rules above, in order
        // to avoid massive plan changes between tidb-server versions, I adopt this conservative strategy
        // to impose this rule after rules above.
        if s.selectivity < o.selectivity {
            return true;
        }
        false
    }
}

/// isColEqCorCol checks if the expression is an eq function that one side is correlated column and another is column.
/// If so, it will return the column's reference. Otherwise, return nil instead.
// go: isColEqCorCol L1
fn is_col_eq_cor_col(filter: &ExprRef) -> Option<&Column> {
    let Expr::ScalarFunction(f) = &**filter else {
        return None;
    };
    if f.func_name.l != ast::EQ {
        return None;
    }
    let args = f.get_args();
    if let Expr::Column(c) = &*args[0]
        && let Expr::CorrelatedColumn(_) = &*args[1]
    {
        return Some(c);
    }
    if let Expr::Column(c) = &*args[1]
        && let Expr::CorrelatedColumn(_) = &*args[0]
    {
        return Some(c);
    }
    None
}

/// findPrefixOfIndex will find columns in index by checking the unique id.
/// So it will return at once no matching column is found.
// go: findPrefixOfIndex L1
fn find_prefix_of_index(cols: &[Arc<Column>], idx_col_ids: &[i64]) -> Vec<Arc<Column>> {
    let mut ret_cols = Vec::with_capacity(idx_col_ids.len());
    'id_loop: for &id in idx_col_ids {
        for col in cols {
            if col.unique_id == id {
                ret_cols.push(Arc::clone(col));
                continue 'id_loop;
            }
        }
        // If no matching column is found, just return.
        return ret_cols;
    }
    ret_cols
}

/// findPrefixOfIndexByCol will find columns in index by checking the unique id or the virtual expression.
/// So it will return at once no matching column is found.
// go: findPrefixOfIndexByCol L1
fn find_prefix_of_index_by_col(
    ctx: &PlanCtx<'_>,
    cols: &[Arc<Column>],
    idx_col_ids: &[i64],
    cached_path: Option<&AccessPath>,
) -> Vec<Arc<Column>> {
    if let Some(cached_path) = cached_path {
        let eval_ctx = ctx.eval_ctx();
        let idx_cols = &cached_path.idx_cols;
        let mut ret_cols = Vec::with_capacity(idx_cols.len());
        'id_loop: for id_col in idx_cols {
            for col in cols {
                if col.equal_by_expr_and_id(&*eval_ctx, id_col) {
                    ret_cols.push(Arc::clone(col));
                    continue 'id_loop;
                }
            }
            // If no matching column is found, just return.
            return ret_cols;
        }
        return ret_cols;
    }
    find_prefix_of_index(cols, idx_col_ids)
}

/// getMaskAndRanges.
///
/// Returns Go's `(mask, ranges, partCover, minAccessCondsForDNFCond, err)`. Go panics on
/// another range type (`IntRangeType`).
// go: getMaskAndRanges L1
fn get_mask_and_ranges(
    ctx: &PlanCtx<'_>,
    exprs: &[ExprRef],
    range_type: RangeType,
    lengths: &[isize],
    cached_path: Option<&AccessPath>,
    cols: &[Arc<Column>],
) -> Result<(i64, Vec<Range>, bool, isize), Error> {
    let mut is_dnf = false;
    let mut min_access_conds_for_dnf_cond: isize = 0;
    let ranges: Vec<Range>;
    let access_conds: Vec<ExprRef>;
    let mut remained_conds: Vec<ExprRef> = Vec::new();
    let rctx = ctx.ranger_ctx();
    match range_type {
        RangeType::ColumnRangeType => {
            let conds = ranger::extract_access_conditions_for_column(&rctx, exprs, &cols[0]);
            let (r, a, _) = ranger::build_column_range(
                conds,
                &rctx,
                &cols[0].ret_type,
                UnspecifiedLength,
                ctx.vars.range_max_size,
            )?;
            (ranges, access_conds) = (r, a);
        }
        RangeType::IndexRangeType => {
            if let Some(cached_path) = cached_path {
                ranges = cached_path.ranges.clone();
                access_conds = cached_path.access_conds.clone();
                remained_conds = cached_path.table_filters.clone();
                is_dnf = cached_path.is_dnf_cond;
                min_access_conds_for_dnf_cond = cached_path.min_access_conds_for_dnf_cond;
            } else {
                let res = ranger::detach_cond_and_build_range_for_index(
                    &rctx,
                    exprs,
                    cols,
                    lengths,
                    ctx.vars.range_max_size,
                )?;
                ranges = res.ranges;
                access_conds = res.access_conds;
                remained_conds = res.remained_conds;
                is_dnf = res.is_dnf_cond;
                min_access_conds_for_dnf_cond = res.min_access_conds_for_dnf_cond;
            }
        }
        RangeType::IntRangeType => panic!("should never be here"),
    }
    let mut mask: i64 = 0;
    if is_dnf && !access_conds.is_empty() {
        mask |= 1;
        return Ok((
            mask,
            ranges,
            !remained_conds.is_empty(),
            min_access_conds_for_dnf_cond,
        ));
    }
    let eval_ctx = ctx.eval_ctx();
    for (i, expr) in exprs.iter().enumerate() {
        for access_cond in &access_conds {
            if expr.equal(&*eval_ctx, access_cond) {
                mask |= 1i64 << i;
                break;
            }
        }
    }
    Ok((mask, ranges, false, 0))
}

/// getMaskAndSelectivityForMVIndex.
///
/// Go returns `(1.0, 0, false)` for "not usable"; here `None`. The `Result` carries the
/// D10.7 boundary of the MV index path builders.
// go: getMaskAndSelectivityForMVIndex L2 (Result for the D10.7 boundary)
fn get_mask_and_selectivity_for_mv_index(
    ctx: &PlanCtx<'_>,
    coll: &HistColl,
    id: i64,
    exprs: &[ExprRef],
) -> Result<Option<(f64, i64)>, Error> {
    let Some(cols) = coll.mv_idx2_columns.get(&id).filter(|c| !c.is_empty()) else {
        return Ok(None);
    };
    // You can find more examples and explanations in comments for collectFilters4MVIndex() and
    // buildPartialPaths4MVIndex() in planner/core.
    let access_conds = collect_filters4_mv_index(ctx, exprs, cols)?;
    let idx_info = coll
        .get_idx(id)
        .expect("index stats")
        .info
        .as_ref()
        .expect("Index.Info");
    let Ok((paths, is_intersection, true)) =
        build_partial_paths4_mv_index(ctx, &access_conds, cols, idx_info, coll)?
    else {
        return Ok(None);
    };
    let total_selectivity = calc_total_selectivity_for_mv_idx_path(coll, &paths, is_intersection);
    let mut mask: i64 = 0;
    let eval_ctx = ctx.eval_ctx();
    for (i, expr) in exprs.iter().enumerate() {
        for access_cond in &access_conds {
            if expr.equal(&*eval_ctx, access_cond) {
                mask |= 1i64 << i;
                break;
            }
        }
    }
    Ok(Some((total_selectivity, mask)))
}

/// `CollectFilters4MVIndex` (planner/core, set as a function variable in Go): the access
/// filters.
// glue: boundary of collectFilters4MVIndex (another block), blocked-by:D10.7 (MV index paths)
fn collect_filters4_mv_index(
    _ctx: &PlanCtx<'_>,
    _filters: &[ExprRef],
    _idx_cols: &[Arc<Column>],
) -> Result<Vec<ExprRef>, Error> {
    Err(blocked("D10.7", "CollectFilters4MVIndex"))
}

/// `BuildPartialPaths4MVIndex` (planner/core): the outer `Result` is the boundary, the inner
/// one Go's `err` (`(paths, isIntersection, ok)` on success).
// glue: boundary of buildPartialPaths4MVIndex (another block), blocked-by:D10.7 (MV index paths)
#[allow(clippy::type_complexity)]
fn build_partial_paths4_mv_index(
    _ctx: &PlanCtx<'_>,
    _access_filters: &[ExprRef],
    _idx_cols: &[Arc<Column>],
    _mv_index: &IndexInfo,
    _hist_coll: &HistColl,
) -> Result<Result<(Vec<AccessPath>, bool, bool), Error>, Error> {
    Err(blocked("D10.7", "BuildPartialPaths4MVIndex"))
}

// glue: stub of GetSelectivityByFilter, blocked-by:D10.6 (part 2: TopN-assisted estimation; Selectivity returns the boundary error when tidb_default_string_match_selectivity is 0)

/// findAvailableStatsForCol.
///
/// `Idx2ColUniqueIDs` is iterated in index id order (Go: random, `M-MAPORDER`).
// go: findAvailableStatsForCol L1 [M-MAPORDER]
pub fn find_available_stats_for_col(
    sctx: &PlanCtx<'_>,
    coll: &HistColl,
    unique_id: i64,
) -> (bool, i64) {
    let vars = &*sctx.vars;
    // try to find available stats in column stats
    let col_stats = coll.get_col(unique_id).map(Arc::as_ref);
    if !column_stats_is_invalid(col_stats, Some(vars), coll, unique_id)
        && col_stats.expect("valid column stats").is_full_load()
    {
        return (false, unique_id);
    }
    // try to find available stats in single column index stats (except for prefix index)
    for (&idx_stats_idx, cols) in &coll.idx2_col_unique_ids {
        if cols.len() == 1 && cols[0] == unique_id {
            let idx_stats = coll.get_idx(idx_stats_idx).map(Arc::as_ref);
            if !index_stats_is_invalid(vars, idx_stats, coll, idx_stats_idx) {
                let idx_stats = idx_stats.expect("valid index stats");
                if idx_stats
                    .info
                    .as_ref()
                    .expect("Index.Info")
                    .columns
                    .as_deref()
                    .unwrap_or(&[])[0]
                    .length
                    == tidb_parser::types::UnspecifiedLength
                    && idx_stats.is_full_load()
                {
                    return (true, idx_stats_idx);
                }
            }
        }
    }
    (false, -1)
}

// glue: stub of getEqualCondSelectivity, blocked-by:D10.6 (part 2: index row counts from statistics; only called by them)

/// outOfRangeEQSelectivity estimates selectivities for out-of-range values.
/// It assumes all modifications are insertions and all new-inserted rows are uniformly distributed
/// and has the same distribution with analyzed rows, which means each unique value should have the
/// same number of rows(Tot/NDV) of it.
// go: outOfRangeEQSelectivity L1
pub fn out_of_range_eq_selectivity(
    mut ndv: i64,
    realtime_row_count: i64,
    column_row_count: i64,
) -> f64 {
    let increase_row_count = realtime_row_count.wrapping_sub(column_row_count);
    if increase_row_count <= 0 {
        return 0.0; // it must be 0 since the histogram contains the whole data
    }
    if ndv < outOfRangeBetweenRate {
        ndv = outOfRangeBetweenRate; // avoid inaccurate selectivity caused by small NDV
    }
    let mut selectivity = 1.0 / ndv as f64;
    if selectivity * column_row_count as f64 > increase_row_count as f64 {
        selectivity = increase_row_count as f64 / column_row_count as f64;
    }
    selectivity
}

/// outOfRangeFullNDV estimates the number of qualified rows when the topN represents all NDV values
/// and the searched value does not appear in the topN
// go: outOfRangeFullNDV L1
pub fn out_of_range_full_ndv(
    mut ndv: f64,
    orig_row_count: f64,
    mut not_null_count: f64,
    realtime_row_count: f64,
    increase_factor: f64,
    modify_count: i64,
) -> f64 {
    let fmin = tidb_base::gostd::math::min;
    let fmax = tidb_base::gostd::math::max;
    // If the table hasn't been modified, it's safe to return 0.
    if modify_count == 0 {
        return 0.0;
    }
    // Calculate "newly added rows" using original row count. We do NOT use notNullCount here
    // because that can always be less than realtimeRowCount if NULLs exist
    let mut new_rows = realtime_row_count - orig_row_count;
    // If the original row count is zero - take the min of original row count and realtimeRowCount
    if not_null_count <= 0.0 {
        not_null_count = fmin(orig_row_count, realtime_row_count);
    }
    // If realtimeRowCount has reduced below the original, we can't determine if there has been a
    // combination of inserts/updates/deletes or only deletes - any out of range estimate is unreliable
    if new_rows < 0.0 {
        new_rows = fmin(not_null_count, realtime_row_count);
    }
    // if no NDV - derive an NDV using sqrt
    if ndv <= 0.0 {
        ndv = fmax(not_null_count, realtime_row_count).sqrt();
    } else {
        // We need to increase the ndv by increaseFactor because the estimate will be increased by
        // the caller of the function
        ndv *= increase_factor;
    }
    fmax(1.0, new_rows / ndv)
}

// glue: stub of crossValidationSelectivity, blocked-by:D10.6 (part 2: index row counts from statistics; needs GetColumnRowCount)

#[cfg(test)]
mod tests {
    use super::*;

    fn node(tp: isize, id: i64, mask: i64, num_cols: usize, sel: f64) -> StatsNode {
        StatsNode {
            tp,
            id,
            mask,
            num_cols,
            selectivity: sel,
            ..StatsNode::default()
        }
    }

    /// Go `TestSelectivityGreedyAlgo` (selectivity_test.go:988; `MockStatsNode(id, mask,
    /// numCols)` leaves the type `IndexType`).
    #[test]
    fn selectivity_greedy_algo() {
        let mut nodes = vec![
            node(IndexType, 1, 3, 2, 0.0),
            node(IndexType, 2, 5, 2, 0.0),
            node(IndexType, 3, 9, 2, 0.0),
        ];
        // Sets should not overlap on mask, so only nodes[0] is chosen.
        let used = get_usable_sets_by_greedy(&mut nodes);
        assert_eq!(used.len(), 1);
        assert_eq!(nodes[used[0]].id, 1);

        nodes.swap(0, 1);
        // Sets chosen should be stable, so the returned node is still the one with ID 1.
        let used = get_usable_sets_by_greedy(&mut nodes);
        assert_eq!(used.len(), 1);
        assert_eq!(nodes[used[0]].id, 1);
    }

    #[test]
    fn out_of_range_helpers() {
        assert_eq!(out_of_range_eq_selectivity(10, 100, 100), 0.0);
        assert_eq!(out_of_range_eq_selectivity(10, 150, 100), 0.01);
        assert_eq!(out_of_range_eq_selectivity(1000, 101, 100), 0.001);
        assert_eq!(out_of_range_full_ndv(0.0, 0.0, 0.0, 0.0, 1.0, 0), 0.0);
        assert_eq!(out_of_range_full_ndv(10.0, 100.0, 90.0, 200.0, 2.0, 5), 5.0);
        assert_eq!(compare_type(ColType, PkType), -1);
        assert_eq!(compare_type(PkType, IndexType), 1);
        assert_eq!(compare_type(IndexType, ColType), 1);
        assert_eq!(compare_type(IndexType, PkType), -1);
    }
}
