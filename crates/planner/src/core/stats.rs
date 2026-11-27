//! go-file: pkg/planner/core/stats.go
//!
//! The statistics of a `DataSource` and of its access paths (block D10.2, the path
//! statistics of FindBestTask, specs/D10.2-find-best-task §4): `deriveStats4DataSource`
//! (the `DeriveStats` of the data source), `initStats`, the path statistics
//! (`deriveTablePathStats`, `deriveIndexPathStats`, `fillIndexPath`, ...) and the heuristic
//! path pruning (`derivePathStatsAndTryHeuristics`).
//!
//! Mapping:
//! - Go's access paths are `*util.AccessPath` pointers shared by `AllPossibleAccessPaths`
//!   and `PossibleAccessPaths`; here both are `Vec<AccessPath>` (values). The statistics are
//!   derived on `all_possible_access_paths` and `possible_access_paths` is rebuilt from it by
//!   position ([`PathAliases`], spec §4.2), which gives the same paths as Go's pointers.
//! - The statement's record of the statistics it used (`StmtCtx.GetUsedStatsInfo`, read by
//!   the slow log and `EXPLAIN ANALYZE`) is not carried (spec §5, known differences).
//! - The new stack has no statistics handle (VS1 plan §3.2): `getStatsTable` gives the
//!   pseudo table, as Go does before the handle is initialized.
//! - `LogicalTableScan` / `LogicalIndexScan` exist only in the old cascades planner
//!   (exception B3): their statistics functions are not ported.

use std::collections::HashMap;
use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_base::kv;
use tidb_base::mysql::has_unsigned_flag;
use tidb_expression::util::{eliminate_no_precision_loss_cast, push_down_not};
use tidb_expression::{Column, Expr, ExprRef};
use tidb_model::{ExtraHandleID, TableInfo, TempTableLocal};
use tidb_parser::types::UnspecifiedLength;
use tidb_sessionctx::ranger::types::{full_int_range, full_not_null_range, full_range};
use tidb_statistics::table::PseudoVersion;
use tidb_statistics::{HistColl, Table};

use crate::base::LogicalPlan;
use crate::cardinality;
use crate::core::cost::{SelectionFactor, ToleranceFactor};
use crate::core::find_best_task::split_index_filter_conditions;
use crate::logicalop::DataSource;
use crate::planctx::PlanCtx;
use crate::property::{GroupNDV, StatsInfo};
use crate::ranger::detacher::{detach_cond_and_build_range_for_index, detach_conds_for_column};
use crate::ranger::ranger::build_table_range;
use crate::util::path::{Col2Len, compare_col2_len};
use crate::util::{AccessPath, column::index_info2_cols};

/// `expression.PushDownNot`; Go stores its result, a nil expression when the internal
/// function cannot be built, and dereferences it later (a panic). The panic happens here.
// glue: expression.PushDownNot at the call sites of this file (Go's nil result panics later)
fn push_down_not_expr(ctx: &PlanCtx<'_>, expr: &ExprRef) -> ExprRef {
    push_down_not(&*ctx.expr_ctx, expr)
        .expect("runtime error: invalid memory address or nil pointer dereference")
}

/// The table's statistics; set by `initStats` before any path statistics are derived.
// glue: Go reads ds.StatisticTable directly (a nil pointer only before initStats)
fn statistic_table(ds: &DataSource) -> &Arc<Table> {
    ds.statistic_table
        .as_ref()
        .expect("initStats sets StatisticTable")
}

/// `ds.TableStats.HistColl`.
// glue: Go reads ds.TableStats.HistColl directly (set by initStats)
fn table_hist_coll(ds: &DataSource) -> Arc<HistColl> {
    Arc::clone(
        ds.table_stats
            .as_ref()
            .and_then(|s| s.hist_coll.as_ref())
            .expect("initStats sets TableStats.HistColl"),
    )
}

/// `ds.StatsInfo()`, set by `deriveStatsByFilter` before the path statistics.
// glue: Go reads ds.StatsInfo() directly
fn ds_stats(ds: &DataSource) -> &Arc<StatsInfo> {
    ds.base
        .plan
        .stats_info()
        .expect("deriveStatsByFilter sets the DataSource's stats")
}

/// Go's `PossibleAccessPaths` holds pointers into `AllPossibleAccessPaths`: the position
/// of each possible path in the list of all paths, so that the possible paths can be read
/// again after the paths of all were changed (module doc).
// glue: Go's *AccessPath sharing between AllPossibleAccessPaths and PossibleAccessPaths
pub(crate) struct PathAliases(Vec<usize>);

impl PathAliases {
    /// The positions of `ds.possible_access_paths` in `ds.all_possible_access_paths`
    /// (same index, same store, same table/index kind). A possible path that is not in the
    /// list of all paths has no Go counterpart in the slice's statements (index merge and
    /// partition alternatives are boundaries): a boundary.
    // glue: Go's pointer identity of the access paths
    pub(crate) fn path_aliases_of(ds: &DataSource) -> Result<PathAliases, Error> {
        let mut pos = Vec::with_capacity(ds.possible_access_paths.len());
        let mut used = vec![false; ds.all_possible_access_paths.len()];
        for p in &ds.possible_access_paths {
            let found = ds
                .all_possible_access_paths
                .iter()
                .enumerate()
                .position(|(i, a)| {
                    !used[i]
                        && a.store_type == p.store_type
                        && a.is_int_handle_path == p.is_int_handle_path
                        && a.is_common_handle_path == p.is_common_handle_path
                        && a.index.as_ref().map(|x| x.id) == p.index.as_ref().map(|x| x.id)
                        && a.partial_index_paths.is_empty()
                        && p.partial_index_paths.is_empty()
                });
            let Some(i) = found else {
                // glue: blocked-by:D10.7 (an access path outside the list of all paths: index merge alternatives)
                return Err(blocked(
                    "D10.7",
                    "an access path outside AllPossibleAccessPaths (index merge)",
                ));
            };
            used[i] = true;
            pos.push(i);
        }
        Ok(PathAliases(pos))
    }

    /// Go's `PossibleAccessPaths` after the shared paths were changed.
    // glue: Go's *AccessPath sharing
    pub(crate) fn rebuild(&self, ds: &mut DataSource) {
        ds.possible_access_paths = self
            .0
            .iter()
            .map(|&i| ds.all_possible_access_paths[i].clone_path())
            .collect();
    }
}

/// `deriveStats4DataSource` (the `DeriveStats` of `DataSource`).
// go: deriveStats4DataSource L1 [M-PLAN]
pub fn derive_stats4_data_source(
    ds: &mut DataSource,
    ctx: &mut PlanCtx<'_>,
    col_groups: &[Vec<Arc<Column>>],
) -> Result<Arc<StatsInfo>, Error> {
    if let Some(s) = ds.base.plan.stats_info()
        && col_groups.is_empty()
    {
        return Ok(Arc::clone(s));
    }
    init_stats(ds, ctx, col_groups)?;
    if let Some(s) = ds.base.plan.stats_info() {
        // Just reload the GroupNDVs.
        let table_stats = ds.table_stats.as_ref().expect("initStats");
        let selectivity = s.row_count / table_stats.row_count;
        let stats = Arc::new(table_stats.scale(selectivity));
        ds.base.plan.set_stats(Some(Arc::clone(&stats)));
        return Ok(stats);
    }
    // EnableOptimizerDebugTrace: physicalOptimize returns the boundary of the debug trace
    // (D02.2, only PLAN REPLAYER switches it on) before the statistics derivation.
    for i in 0..ds.pushed_down_conds.len() {
        let e = push_down_not_expr(ctx, &ds.pushed_down_conds[i]);
        ds.pushed_down_conds[i] = eliminate_no_precision_loss_cast(&*ctx.expr_ctx, &e)
            .expect("runtime error: invalid memory address or nil pointer dereference");
    }
    check_partial_indexes(ds)?;
    let aliases = PathAliases::path_aliases_of(ds)?;
    // Index pruning is now done earlier in CollectPredicateColumnsPoint to avoid loading stats for pruned indexes.
    // Fill index paths for all paths.
    for i in 0..ds.all_possible_access_paths.len() {
        if ds.all_possible_access_paths[i].is_table_path() {
            continue;
        }
        let mut path = std::mem::take(&mut ds.all_possible_access_paths[i]);
        let conds = ds.pushed_down_conds.clone();
        let r = fill_index_path(ds, ctx, &mut path, &conds);
        ds.all_possible_access_paths[i] = path;
        r?;
    }
    aliases.rebuild(ds);
    // TODO: Can we move ds.deriveStatsByFilter after pruning by heuristics? In this way some computation can be avoided
    // when ds.PossibleAccessPaths are pruned.
    let conds = ds.pushed_down_conds.clone();
    let stats = derive_stats_by_filter(ds, ctx, &conds, &ds.all_possible_access_paths)?;
    ds.base.plan.set_stats(Some(Arc::new(stats)));
    // after heuristic pruning, the new path are stored into ds.PossibleAccessPaths.
    derive_path_stats_and_try_heuristics(ds, ctx, &aliases)?;

    // index merge path is generated from all conditions from ds based on ds.PossibleAccessPath.
    // we should renew ds.PossibleAccessPath to AllPossibleAccessPath once a new DS is generated.
    generate_index_merge_path(ds, ctx)?;

    // ds.SCtx().GetSessionVars().StmtCtx.EnableOptimizerDebugTrace: returned above.
    let total = ds.tbl_col_hists.as_ref().map_or(0, |h| h.realtime_count) as f64;
    let (min_selectivity, index_force) =
        get_general_attributes_from_paths(&ds.possible_access_paths, total);
    ds.access_path_min_selectivity = min_selectivity;
    if index_force {
        ctx.vars.stmt_ctx_mut().set_index_force();
    }

    Ok(Arc::clone(ds_stats(ds)))
}

/// `DataSource.CheckPartialIndexes` (`logical_datasource.go`): partial indexes (indexes
/// with a condition) are checked against the pushed down conditions. Without a partial
/// index Go does nothing; partial indexes need `CheckPartialIndexByFilters` (the partial
/// index condition parser), a boundary.
// go: pkg/planner/core/operator/logicalop/logical_datasource.go DataSource.CheckPartialIndexes L2 (a partial index is a boundary: D10.2 part 2)
fn check_partial_indexes(ds: &DataSource) -> Result<(), Error> {
    for path in &ds.possible_access_paths {
        // If there is no condition expression, it is not a partial index.
        // So we skip it directly.
        let Some(index) = &path.index else { continue };
        if !index.has_condition() {
            continue;
        }
        // glue: blocked-by:D10.2 (partial indexes: CheckPartialIndexByFilters, part 2 of this block)
        return Err(blocked(
            "D10.2",
            "a partial index (CheckPartialIndexByFilters)",
        ));
    }
    // 1. No partial index: we don't need to do anything.
    Ok(())
}

/// `generateIndexMergePath` (`indexmerge_path.go`, block D10.7): the index merge paths. Go
/// adds no path, and changes nothing visible, when index merge is not allowed, on a local
/// temporary table, or when the conditions have no `OR`, no path is a multi-valued index and
/// the statement has no index merge hint (the AND paths need hinted indexes, the OR paths
/// an `OR` condition, the MV index paths an MV index). Every other case is a boundary.
// glue: blocked-by:D10.7 (generateIndexMergePath: the index merge paths, outside the first slice)
fn generate_index_merge_path(ds: &DataSource, ctx: &PlanCtx<'_>) -> Result<(), Error> {
    let stmt_ctx = ctx.vars.stmt_ctx();
    let session_and_stmt_permission =
        ctx.vars.get_enable_index_merge() && !stmt_ctx.stmt_hints.no_index_merge_hint;
    if !session_and_stmt_permission {
        return Ok(());
    }
    if ds.table_info.temp_table_type == TempTableLocal {
        return Ok(());
    }
    let has_or = ds.all_conds.iter().any(|c| {
        matches!(&**c, Expr::ScalarFunction(sf) if sf.func_name.l == tidb_expression::scalar_function::ast_func::LogicOr)
    });
    let has_mv_index = ds
        .possible_access_paths
        .iter()
        .any(|p| p.index.as_ref().is_some_and(|i| i.mv_index));
    if (has_or && ds.possible_access_paths.len() > 1) || has_mv_index {
        return Err(blocked(
            "D10.7",
            "index merge paths (generateIndexMergePath)",
        ));
    }
    Ok(())
}

/// `fillIndexPath`.
// go: fillIndexPath L1 [M-PLAN]
pub fn fill_index_path(
    ds: &mut DataSource,
    ctx: &PlanCtx<'_>,
    path: &mut AccessPath,
    conds: &[ExprRef],
) -> Result<(), Error> {
    // EnableOptimizerDebugTrace is checked by deriveStats4DataSource.
    path.ranges = full_range();
    path.count_after_access = statistic_table(ds).realtime_count as f64;
    let index = Arc::clone(path.index.as_ref().expect("an index path"));
    (
        path.idx_cols,
        path.idx_col_lens,
        path.full_idx_cols,
        path.full_idx_col_lens,
    ) = index_info2_cols(&ds.columns, &ds.schema(&ctx.arena).columns, &index);
    let index_cols = index.columns.as_deref().unwrap_or_default();
    if !index.unique && !index.primary && index_cols.len() == path.idx_cols.len() {
        let handle_col = ds.get_pk_is_handle_col(ctx);
        if let Some(handle_col) = handle_col
            && !has_unsigned_flag(handle_col.ret_type.get_flag())
        {
            let mut already_handle = false;
            for col in &path.idx_cols {
                if col.id == ExtraHandleID as i64
                    || col.equal_column(&Expr::Column(Column::clone(&handle_col)))
                {
                    already_handle = true;
                }
            }
            // Don't add one column twice to the index. May cause unexpected errors.
            if !already_handle {
                path.full_idx_cols.push(Some(Arc::clone(&handle_col)));
                path.full_idx_col_lens.push(UnspecifiedLength);
                path.idx_cols.push(Arc::clone(&handle_col));
                path.idx_col_lens.push(UnspecifiedLength);
                // Also updates the map that maps the index id to its prefix column ids.
                // (Go writes the map through the shared HistColl; the table stats are the
                // DataSource's own at this point: copy on write.)
                let table_stats = Arc::make_mut(ds.table_stats.as_mut().expect("initStats"));
                let coll = Arc::make_mut(table_stats.hist_coll.as_mut().expect("initStats"));
                if coll.idx2_col_unique_ids.get(&index.id).map_or(0, Vec::len) == index_cols.len() {
                    coll.idx2_col_unique_ids
                        .entry(index.id)
                        .or_default()
                        .push(handle_col.unique_id);
                }
            }
        }
    }
    let coll = table_hist_coll(ds);
    detach_cond_and_build_range_for_path(ctx, path, conds, &coll)
}

/// deriveIndexPathStats will fulfill the information that the AccessPath need.
/// conds is the conditions used to generate the DetachRangeResult for path.
/// isIm indicates whether this function is called to generate the partial path for IndexMerge.
// go: deriveIndexPathStats L1 [M-PLAN]
pub fn derive_index_path_stats(
    ds: &DataSource,
    ctx: &PlanCtx<'_>,
    path: &mut AccessPath,
    is_im: bool,
) -> Result<(), Error> {
    let stats_tbl = statistic_table(ds);
    if path.eq_or_in_cond_count == path.access_conds.len() as isize {
        let (accesses, remained) =
            path.split_cor_col_access_cond_from_filters(&*ctx.expr_ctx, path.eq_or_in_cond_count);
        path.access_conds.extend(accesses.iter().map(Arc::clone));
        path.table_filters = remained;
        if !accesses.is_empty() && stats_tbl.pseudo {
            path.count_after_access = cardinality::pseudo::pseudo_avg_count_per_value(stats_tbl);
        } else {
            let selectivity = path.count_after_access / stats_tbl.realtime_count as f64;
            for i in 0..accesses.len() {
                let col = &path.idx_cols[path.eq_or_in_cond_count as usize + i];
                let mut ndv = cardinality::ndv::estimate_column_ndv(stats_tbl, col.id);
                ndv *= selectivity;
                if ndv < 1.0 {
                    ndv = 1.0;
                }
                path.count_after_access /= ndv;
            }
        }
    }
    let (index_filters, table_filters) = split_index_filter_conditions(
        ds,
        ctx,
        &path.table_filters,
        &path.full_idx_cols,
        &path.full_idx_col_lens,
    );
    path.table_filters = table_filters;
    path.index_filters.extend(index_filters);
    // If the `CountAfterAccess` is less than `stats.RowCount`, there must be some inconsistent stats info.
    // We prefer the `stats.RowCount` because it could use more stats info to calculate the selectivity.
    // Add an arbitrary tolerance factor to account for comparison with floating point
    let row_count = ds_stats(ds).row_count;
    if (path.count_after_access + ToleranceFactor) < row_count && !is_im {
        path.count_after_access =
            (row_count / SelectionFactor).min(stats_tbl.realtime_count as f64);
    }
    if !path.index_filters.is_empty() {
        let coll = table_hist_coll(ds);
        let selectivity =
            match cardinality::selectivity::selectivity(ctx, &coll, &path.index_filters, &[]) {
                Ok((s, _)) => s,
                Err(e) if tidb_base::blocked::blocked_by(&e).is_some() => return Err(e),
                Err(e) => {
                    tidb_base::log_debug!(
                        "calculate selectivity failed, use selection factor",
                        "error" => &e,
                    );
                    SelectionFactor
                }
            };
        if is_im {
            path.count_after_index = path.count_after_access * selectivity;
        } else {
            path.count_after_index = (path.count_after_access * selectivity).max(row_count);
        }
    } else {
        path.count_after_index = path.count_after_access;
    }
    Ok(())
}

/// deriveTablePathStats will fulfill the information that the AccessPath need.
/// isIm indicates whether this function is called to generate the partial path for IndexMerge.
// go: deriveTablePathStats L1 [M-PLAN]
pub fn derive_table_path_stats(
    ds: &DataSource,
    ctx: &PlanCtx<'_>,
    path: &mut AccessPath,
    conds: &[ExprRef],
    is_im: bool,
) -> Result<(), Error> {
    if path.is_common_handle_path {
        return derive_common_handle_table_path_stats(ds, ctx, path, conds, is_im);
    }
    let stats_tbl = statistic_table(ds);
    path.count_after_access = stats_tbl.realtime_count as f64;
    path.table_filters = conds.to_vec();
    let mut pk_col: Option<Arc<Column>> = None;
    let mut is_unsigned = false;
    let schema = ds.schema(&ctx.arena);
    if ds.table_info.pk_is_handle {
        if let Some(pk_col_info) = ds.table_info.get_pk_col_info() {
            is_unsigned = has_unsigned_flag(pk_col_info.get_flag());
            pk_col = tidb_expression::column::col_info_to_col(&schema.columns, pk_col_info)
                .map(Arc::clone);
        }
    } else {
        pk_col = schema.get_extra_handle_column().map(Arc::clone);
    }
    let Some(pk_col) = pk_col else {
        path.ranges = full_int_range(is_unsigned);
        return Ok(());
    };

    path.ranges = full_int_range(is_unsigned);
    if conds.is_empty() {
        return Ok(());
    }
    // for cnf condition combination, c=1 and c=2 and (1 member of (a)),
    // c=1 and c=2 will derive invalid range represented by an access condition as constant of 0 (false).
    // later this constant of 0 will be built as empty range.
    let rctx = ctx.ranger_ctx();
    (path.access_conds, path.table_filters) = detach_conds_for_column(&rctx, conds, &pk_col);
    // If there's no access cond, we try to find that whether there's expression containing correlated column that
    // can be used to access data.
    let mut cor_col_in_access_conds = false;
    if path.access_conds.is_empty() {
        let pk_expr = Expr::Column(Column::clone(&pk_col));
        for i in 0..path.table_filters.len() {
            let filter = &path.table_filters[i];
            let Expr::ScalarFunction(eq_func) = &**filter else {
                continue;
            };
            if eq_func.func_name.l != tidb_expression::scalar_function::ast_func::EQ {
                continue;
            }
            let args = eq_func.get_args();
            if let Expr::Column(l_col) = &*args[0]
                && l_col.equal(&pk_expr)
                && matches!(&*args[1], Expr::CorrelatedColumn(_))
            {
                let f = path.table_filters.remove(i);
                path.access_conds.push(f);
                cor_col_in_access_conds = true;
                break;
            }
            if let Expr::Column(r_col) = &*args[1]
                && r_col.equal(&pk_expr)
                && matches!(&*args[0], Expr::CorrelatedColumn(_))
            {
                let f = path.table_filters.remove(i);
                path.access_conds.push(f);
                cor_col_in_access_conds = true;
                break;
            }
        }
    }
    if cor_col_in_access_conds {
        path.count_after_access = 1.0;
        return Ok(());
    }
    let (ranges, access_conds, remained_conds) = build_table_range(
        std::mem::take(&mut path.access_conds),
        &rctx,
        &pk_col.ret_type,
        ctx.vars.range_max_size,
    )?;
    path.ranges = ranges;
    path.access_conds = access_conds;
    path.table_filters.extend(remained_conds);
    path.count_after_access = cardinality::row_count_column::get_row_count_by_int_column_ranges(
        ctx,
        &stats_tbl.hist_coll,
        pk_col.id,
        &path.ranges,
    )?;
    // If the `CountAfterAccess` is less than `stats.RowCount`, there must be some inconsistent stats info.
    // We prefer the `stats.RowCount` because it could use more stats info to calculate the selectivity.
    // Add an arbitrary tolerance factor to account for comparison with floating point
    let row_count = ds_stats(ds).row_count;
    if (path.count_after_access + ToleranceFactor) < row_count && !is_im {
        path.count_after_access =
            (row_count / SelectionFactor).min(stats_tbl.realtime_count as f64);
    }
    Ok(())
}

// go: deriveCommonHandleTablePathStats L1 [M-PLAN]
fn derive_common_handle_table_path_stats(
    ds: &DataSource,
    ctx: &PlanCtx<'_>,
    path: &mut AccessPath,
    conds: &[ExprRef],
    is_im: bool,
) -> Result<(), Error> {
    let stats_tbl = statistic_table(ds);
    path.count_after_access = stats_tbl.realtime_count as f64;
    path.ranges = full_not_null_range();
    let index = Arc::clone(path.index.as_ref().expect("the clustered index"));
    (
        path.idx_cols,
        path.idx_col_lens,
        path.full_idx_cols,
        path.full_idx_col_lens,
    ) = index_info2_cols(&ds.columns, &ds.schema(&ctx.arena).columns, &index);
    if conds.is_empty() {
        return Ok(());
    }
    let coll = table_hist_coll(ds);
    detach_cond_and_build_range_for_path(ctx, path, conds, &coll)?;
    if path.eq_or_in_cond_count == path.access_conds.len() as isize {
        let (accesses, remained) =
            path.split_cor_col_access_cond_from_filters(&*ctx.expr_ctx, path.eq_or_in_cond_count);
        path.access_conds.extend(accesses.iter().map(Arc::clone));
        path.table_filters = remained;
        if !accesses.is_empty() && stats_tbl.pseudo {
            path.count_after_access = cardinality::pseudo::pseudo_avg_count_per_value(stats_tbl);
        } else {
            let selectivity = path.count_after_access / stats_tbl.realtime_count as f64;
            for i in 0..accesses.len() {
                let col = &path.idx_cols[path.eq_or_in_cond_count as usize + i];
                let mut ndv = cardinality::ndv::estimate_column_ndv(stats_tbl, col.id);
                ndv *= selectivity;
                if ndv < 1.0 {
                    ndv = 1.0;
                }
                path.count_after_access /= ndv;
            }
        }
    }
    // If the `CountAfterAccess` is less than `stats.RowCount`, there must be some inconsistent stats info.
    // We prefer the `stats.RowCount` because it could use more stats info to calculate the selectivity.
    // Add an arbitrary tolerance factor to account for comparison with floating point
    let row_count = ds_stats(ds).row_count;
    if (path.count_after_access + ToleranceFactor) < row_count && !is_im {
        path.count_after_access =
            (row_count / SelectionFactor).min(stats_tbl.realtime_count as f64);
    }
    Ok(())
}

// go: detachCondAndBuildRangeForPath L1 [M-CTX]
fn detach_cond_and_build_range_for_path(
    sctx: &PlanCtx<'_>,
    path: &mut AccessPath,
    conds: &[ExprRef],
    hist_coll: &HistColl,
) -> Result<(), Error> {
    if path.idx_cols.is_empty() {
        path.table_filters = conds.to_vec();
        return Ok(());
    }
    let res = detach_cond_and_build_range_for_index(
        &sctx.ranger_ctx(),
        conds,
        &path.idx_cols,
        &path.idx_col_lens,
        sctx.vars.range_max_size,
    )?;
    path.ranges = res.ranges;
    path.access_conds = res.access_conds;
    path.table_filters = res.remained_conds;
    path.eq_cond_count = res.eq_cond_count;
    path.eq_or_in_cond_count = res.eq_or_in_count;
    path.is_dnf_cond = res.is_dnf_cond;
    path.min_access_conds_for_dnf_cond = res.min_access_conds_for_dnf_cond;
    path.const_cols = vec![false; path.idx_cols.len()];
    if let Some(values) = &res.column_values {
        for (i, c) in path.const_cols.iter_mut().enumerate() {
            *c = values[i].is_some();
        }
    }
    let index_id = path.index.as_ref().expect("an index path").id;
    path.count_after_access = cardinality::row_count_index::get_row_count_by_index_ranges(
        sctx,
        hist_coll,
        index_id,
        &path.ranges,
        &path.idx_cols,
    )?;
    Ok(())
}

// go: getGeneralAttributesFromPaths L1
fn get_general_attributes_from_paths(paths: &[AccessPath], total_row_count: f64) -> (f64, bool) {
    let mut min_selectivity: f64 = 1.0;
    let mut index_force = false;
    for path in paths {
        // For table path and index merge path, AccessPath.CountAfterIndex is not set and meaningless,
        // but we still consider their AccessPath.CountAfterAccess.
        if total_row_count > 0.0 {
            if path.is_table_path() || !path.partial_index_paths.is_empty() {
                min_selectivity = min_selectivity.min(path.count_after_access / total_row_count);
            } else {
                min_selectivity = min_selectivity.min(path.count_after_index / total_row_count);
            }
        }
        if !index_force && path.forced {
            index_force = true;
        }
    }
    (min_selectivity, index_force)
}

/// `getGroupNDVs`; Go's nil `colGroups` returns nil.
// go: getGroupNDVs L1 [M-MAPORDER]
fn get_group_ndvs(ds: &DataSource, col_groups: &[Vec<Arc<Column>>]) -> Vec<GroupNDV> {
    if col_groups.is_empty() {
        return Vec::new();
    }
    let tbl = table_hist_coll(ds);
    let mut ndvs = Vec::with_capacity(col_groups.len());
    tbl.for_each_index_immutable(|idx_id, idx| {
        let ids = tbl
            .idx2_col_unique_ids
            .get(&idx_id)
            .cloned()
            .unwrap_or_default();
        let mut cols_len = ids.len();
        let info_cols = idx
            .info
            .as_ref()
            .and_then(|i| i.columns.as_deref())
            .map_or(0, <[_]>::len);
        // tbl.Idx2ColUniqueIDs may only contain the prefix of index columns.
        // But it may exceeds the total index since the index would contain the handle column if it's not a unique index.
        // We append the handle at fillIndexPath.
        if cols_len < info_cols {
            return false;
        } else if cols_len > info_cols {
            cols_len -= 1;
        }
        let mut idx_cols = ids[..cols_len].to_vec();
        idx_cols.sort();
        for g in col_groups {
            // We only want those exact matches.
            if g.len() != cols_len {
                return false;
            }
            let mut matched = true;
            for (i, col) in g.iter().enumerate() {
                // Both slices are sorted according to UniqueID.
                if col.unique_id != idx_cols[i] {
                    matched = false;
                    break;
                }
            }
            if matched && idx.stats_loaded_status.is_essential_stats_loaded() {
                ndvs.push(GroupNDV {
                    cols: idx_cols,
                    ndv: idx.histogram.ndv as f64,
                });
                return true;
            }
        }
        false
    });
    ndvs
}

// glue: stub of getTblInfoForUsedStatsByPhysicalID, blocked-by:D14.2 (the statement's record of the used statistics, read by the slow log and EXPLAIN ANALYZE: not carried, see the module doc)
/// `initStats`. The record of the used statistics (`statsRecord.RecordUsedInfo`) is not
/// carried (module doc).
// go: initStats L2 (the used statistics record is not carried, spec §5)
pub fn init_stats(
    ds: &mut DataSource,
    ctx: &PlanCtx<'_>,
    col_groups: &[Vec<Arc<Column>>],
) -> Result<(), Error> {
    if ds.table_stats.is_some() {
        // Reload GroupNDVs since colGroups may have changed.
        let ndvs = get_group_ndvs(ds, col_groups);
        if let Some(table_stats) = &mut ds.table_stats {
            Arc::make_mut(table_stats).group_ndvs = ndvs;
        }
        return Ok(());
    }
    if ds.statistic_table.is_none() {
        ds.statistic_table = Some(Arc::new(
            crate::core::logical_plan_builder::get_stats_table(
                ctx,
                &ds.table_info,
                ds.physical_table_id,
            )?,
        ));
    }
    let stats_tbl = Arc::clone(statistic_table(ds));
    let schema = ds.schema(&ctx.arena);
    let mut table_stats = StatsInfo {
        row_count: stats_tbl.realtime_count as f64,
        col_ndvs: HashMap::with_capacity(schema.len()),
        hist_coll: Some(Arc::new(
            stats_tbl.generate_hist_coll_from_column_info(&ds.table_info, &ds.tbl_cols)?,
        )),
        stats_version: stats_tbl.version,
        ..Default::default()
    };
    if stats_tbl.pseudo {
        table_stats.stats_version = PseudoVersion;
    }

    // statsRecord.RecordUsedInfo(...): not carried (module doc).

    for col in &schema.columns {
        table_stats.col_ndvs.insert(
            col.unique_id,
            cardinality::ndv::estimate_column_ndv(&stats_tbl, col.id),
        );
    }
    ds.table_stats = Some(Arc::new(table_stats));
    let ndvs = get_group_ndvs(ds, col_groups);
    Arc::make_mut(ds.table_stats.as_mut().expect("set above")).group_ndvs = ndvs;
    ds.tbl_col_hists = Some(Arc::new(stats_tbl.id2_unique_id(&ds.tbl_cols)));
    // The loop over the public columns of the table does nothing.
    Ok(())
}

/// `deriveStatsByFilter`. A boundary of the selectivity estimation is returned (Go's
/// estimation has no such boundaries; it falls back to the selection factor on errors).
// go: deriveStatsByFilter L1 [M-PLAN]
pub fn derive_stats_by_filter(
    ds: &DataSource,
    ctx: &PlanCtx<'_>,
    conds: &[ExprRef],
    filled_paths: &[AccessPath],
) -> Result<StatsInfo, Error> {
    let coll = table_hist_coll(ds);
    let selectivity = match cardinality::selectivity::selectivity(ctx, &coll, conds, filled_paths) {
        Ok((s, _)) => s,
        Err(e) if tidb_base::blocked::blocked_by(&e).is_some() => return Err(e),
        Err(e) => {
            tidb_base::log_debug!(
                "something wrong happened, use the default selectivity",
                "error" => &e,
            );
            SelectionFactor
        }
    };
    // TODO: remove NewHistCollBySelectivity later on.
    Ok(ds
        .table_stats
        .as_ref()
        .expect("initStats")
        .scale(selectivity))
}

/// We bind logic of derivePathStats and tryHeuristics together. When some path matches the
/// heuristic rule, we don't need to derive stats of subsequent paths. In this way we can save
/// unnecessary computation of derivePathStats.
// go: derivePathStatsAndTryHeuristics L1 [M-PLAN]
fn derive_path_stats_and_try_heuristics(
    ds: &mut DataSource,
    ctx: &PlanCtx<'_>,
    aliases: &PathAliases,
) -> Result<(), Error> {
    let mut unique_idxs_with_double_scan: Vec<usize> =
        Vec::with_capacity(ds.all_possible_access_paths.len());
    let mut single_scan_idxs: Vec<usize> = Vec::with_capacity(ds.all_possible_access_paths.len());
    let mut selected: Option<usize> = None;
    let mut unique_best: Option<usize> = None;
    let mut refined_best: Option<usize> = None;
    let mut is_refined_path = false;
    // step1: if user prefer tiFlash store type, tiFlash path should always be built anyway ahead.
    let mut tiflash_path: Option<usize> = None;
    if ds.prefer_store_type & crate::hint::hint::PreferTiFlash != 0 {
        for i in 0..ds.all_possible_access_paths.len() {
            if ds.all_possible_access_paths[i].store_type == kv::TiFlash {
                let mut path = std::mem::take(&mut ds.all_possible_access_paths[i]);
                let conds = ds.pushed_down_conds.clone();
                let r = derive_table_path_stats(ds, ctx, &mut path, &conds, false);
                path.is_single_scan = true;
                ds.all_possible_access_paths[i] = path;
                r?;
                tiflash_path = Some(i);
                break;
            }
        }
    }
    // step2: kv path should follow the heuristic rules.
    let tc = ctx.vars.stmt_ctx().type_ctx();
    for i in 0..ds.all_possible_access_paths.len() {
        let mut path = std::mem::take(&mut ds.all_possible_access_paths[i]);
        let r = if path.is_table_path() {
            let conds = ds.pushed_down_conds.clone();
            let r = derive_table_path_stats(ds, ctx, &mut path, &conds, false);
            path.is_single_scan = true;
            r
        } else {
            let r = derive_index_path_stats(ds, ctx, &mut path, false);
            // Reevaluate path.IsSingleScan because it may have been set incorrectly
            // in the pruning logic.
            path.is_single_scan =
                ds.is_single_scan(ctx, &path.full_idx_cols, &path.full_idx_col_lens);
            r
        };
        ds.all_possible_access_paths[i] = path;
        r?;
        let path = &ds.all_possible_access_paths[i];
        // step: 3
        // Try some heuristic rules to select access path.
        // tiFlash path also have table-range-scan (range point like here) to be heuristic treated.
        if path.ranges.is_empty() {
            selected = Some(i);
            break;
        }
        if path.only_point_range(&tc) {
            if path.is_table_path() || path.index.as_ref().is_some_and(|x| x.unique) {
                if path.is_single_scan {
                    selected = Some(i);
                    break;
                }
                unique_idxs_with_double_scan.push(i);
            }
        } else if path.is_single_scan {
            single_scan_idxs.push(i);
        }
    }
    let paths = &ds.all_possible_access_paths;
    if selected.is_none() && !unique_idxs_with_double_scan.is_empty() {
        let mut unique_idx_access_cols: Vec<Col2Len> =
            Vec::with_capacity(unique_idxs_with_double_scan.len());
        for &u in &unique_idxs_with_double_scan {
            let unique_idx = &paths[u];
            unique_idx_access_cols.push(unique_idx.get_col2_len_from_access_conds(&*ctx.expr_ctx));
            // Find the unique index with the minimal number of ranges as `uniqueBest`.
            /*
                If the number of scan ranges are equal, choose the one with the least table predicates - meaning the unique index with the most index predicates.
                Because the most index predicates means that it is more likely to fetch 0 index rows.
                Example in the test "TestPointgetIndexChoosen".
            */
            let better = match unique_best {
                None => true,
                Some(b) => {
                    unique_idx.ranges.len() < paths[b].ranges.len()
                        || (unique_idx.ranges.len() == paths[b].ranges.len()
                            && unique_idx.table_filters.len() < paths[b].table_filters.len())
                }
            };
            if better {
                unique_best = Some(u);
            }
        }
        // `uniqueBest` may not always be the best.
        // ```text
        // create table t(a int, b int, c int, unique index idx_b(b), index idx_b_c(b, c));
        // select b, c from t where b = 5 and c > 10;
        // ```
        // In the case, `uniqueBest` is `idx_b`. However, `idx_b_c` is better than `idx_b`.
        // Hence, for each index in `singleScanIdxs`, we check whether it is better than some index in `uniqueIdxsWithDoubleScan`.
        // If yes, the index is a refined one. We find the refined index with the minimal number of ranges as `refineBest`.
        for &s in &single_scan_idxs {
            let single_scan_idx = &paths[s];
            let col2_len = single_scan_idx.get_col2_len_from_access_conds(&*ctx.expr_ctx);
            for unique_idx_col2_len in &unique_idx_access_cols {
                let (access_result, comparable1) = compare_col2_len(&col2_len, unique_idx_col2_len);
                if comparable1
                    && access_result == 1
                    && refined_best
                        .is_none_or(|r| single_scan_idx.ranges.len() < paths[r].ranges.len())
                {
                    refined_best = Some(s);
                }
            }
        }
        // `refineBest` may not always be better than `uniqueBest`.
        // ```text
        // create table t(a int, b int, c int, d int, unique index idx_a(a), unique index idx_b_c(b, c), unique index idx_b_c_a_d(b, c, a, d));
        // select a, b, c from t where a = 1 and b = 2 and c in (1, 2, 3, 4, 5);
        // ```
        // In the case, `refinedBest` is `idx_b_c_a_d` and `uniqueBest` is `a`. `idx_b_c_a_d` needs to access five points while `idx_a`
        // only needs one point access and one table access.
        // Hence we should compare `len(refinedBest.Ranges)` and `2*len(uniqueBest.Ranges)` to select the better one.
        if let Some(r) = refined_best
            && unique_best.is_none_or(|u| paths[r].ranges.len() < 2 * paths[u].ranges.len())
        {
            selected = Some(r);
            is_refined_path = true;
        } else {
            selected = unique_best;
        }
    }
    // The possible paths share the derived paths (Go's pointers).
    aliases.rebuild(ds);
    // heuristic rule pruning other path should consider hint prefer.
    // If no hints and some path matches a heuristic rule, just remove other possible paths.
    if let Some(sel) = selected {
        // heuristic rule pruning only affect current DS's PossibleAccessPaths, where physical plan will be generated.
        let selected_path = ds.all_possible_access_paths[sel].clone_path();
        if ds.possible_access_paths.is_empty() {
            // Go writes PossibleAccessPaths[0] of an empty slice: an index out of range.
            panic!("runtime error: index out of range [0] with length 0");
        }
        ds.possible_access_paths.truncate(1);
        ds.possible_access_paths[0] = selected_path;
        // if user wanna tiFlash read, while current heuristic choose a TiKV path. so we shouldn't prune tiFlash path.
        let keep = ds.prefer_store_type & crate::hint::hint::PreferTiFlash != 0
            && ds.all_possible_access_paths[sel].store_type != kv::TiFlash;
        if keep {
            // also keep tiflash path as well.
            let Some(t) = tiflash_path else {
                // Go appends a nil path; it is dereferenced later.
                panic!("runtime error: invalid memory address or nil pointer dereference");
            };
            let p = ds.all_possible_access_paths[t].clone_path();
            ds.possible_access_paths.push(p);
            return Ok(());
        }
        let table_name = match &ds.table_as_name {
            Some(n) if !n.o.is_empty() => n.o.clone(),
            _ => ds.table_info.name.o.clone(),
        };
        let selected = &ds.all_possible_access_paths[sel];
        let mut sb = String::new();
        if selected.is_table_path() {
            // TODO: primary key / handle / real name?
            sb.push_str(&format!(
                "handle of {table_name} is selected since the path only has point ranges"
            ));
        } else {
            let index = selected.index.as_ref().expect("an index path");
            if index.unique {
                sb.push_str("unique ");
            }
            sb.push_str(&format!(
                "index {} of {table_name} is selected since the path",
                index.name.o
            ));
            if is_refined_path {
                sb.push_str(" only fetches limited number of rows");
            } else {
                sb.push_str(" only has point ranges");
            }
            if selected.is_single_scan {
                sb.push_str(" with single scan");
            } else {
                sb.push_str(" with double scan");
            }
        }
        let sc = ctx.vars.stmt_ctx();
        if sc.in_verbose_explain {
            sc.append_note(Error::plain(sb.clone()));
        } else {
            sc.append_extra_note(Error::plain(sb.clone()));
        }
    }
    Ok(())
}

/// loadTableStats loads the stats of the table and store it in the statement
/// `UsedStatsInfo` if it didn't exist. The record of the used statistics is not carried
/// (module doc), so only the statistics table is read (`getStatsTable`, whose errors are
/// the boundaries of the debug trace).
// go: loadTableStats L2 (the used statistics record is not carried, spec §5)
pub fn load_table_stats(ctx: &PlanCtx<'_>, tbl_info: &TableInfo, pid: i64) -> Result<(), Error> {
    // statsRecord.GetUsedInfo(pid) != nil: never (not carried).
    let _table_stats = crate::core::logical_plan_builder::get_stats_table(ctx, tbl_info, pid)?;
    // statsRecord.RecordUsedInfo(pid, usedStats): not carried.
    Ok(())
}
