//! go-file: pkg/planner/core/rule_collect_plan_stats.go
//!
//! The statistics loading point `CollectPredicateColumnsPoint` (block D09.13): the column
//! statistics usage and the index pruning of every DataSource (`PruneIndexesByWhereAndOrder`,
//! ported with FindBestTask, D10.2). The statistics usage record and the synchronous /
//! asynchronous loading are the statistics cache (D11.2): the new stack has only pseudo
//! statistics (VS1 plan §3.2), so there is nothing to load; those parts stay boundaries of
//! D11.2 (registered in the optimizer's rule list).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tidb_base::Error;

use crate::arena::PlanId;
use crate::base::{LogicalKind, LogicalOptRule};
use crate::logicalop::DataSource;
use crate::planctx::PlanCtx;
use crate::util::optimizetrace::LogicalOptimizeOp;

/// CollectPredicateColumnsPoint collects the columns that are used in the predicates.
#[derive(Clone, Copy, Debug)]
pub struct CollectPredicateColumnsPoint;

impl LogicalOptRule for CollectPredicateColumnsPoint {
    /// Optimize implements LogicalOptRule.<0th> interface: the column statistics usage and
    /// the index pruning; the statistics usage record (`UpdateColStatsUsage`) and the loading
    /// of the needed statistics are not carried (module doc).
    // go: CollectPredicateColumnsPoint.Optimize L2 (the statistics usage record and loading are the statistics cache, D11.2)
    fn optimize(
        &self,
        ctx: &mut PlanCtx<'_>,
        plan: PlanId,
        _opt: &LogicalOptimizeOp,
    ) -> Result<(PlanId, bool), Error> {
        let plan_changed = false;
        let (_predicate_columns, _visited_phys_tbl_ids, _tid2pids) =
            crate::core::collect_column_stats_usage::collect_column_stats_usage(ctx, plan)?;
        // plan.SCtx().UpdateColStatsUsage(...), the table metadata of the visited tables and
        // markAtLeastOneFullStatsLoadForEachTable: the statistics cache (D11.2).

        // Prune indexes before collecting sync indices to avoid loading stats for indexes that will be pruned.
        // This must happen after InterestingColumns is set (which happens in CollectColumnStatsUsage).
        let _kept_index_ids = prune_indexes_for_all_data_sources(ctx, plan);

        // CollectDependingVirtualCols, collectSyncIndices, collectHistNeededItems and the
        // loading (RequestLoadStats / AsyncLoadHistogramNeededItems): nothing to load with
        // pseudo statistics (module doc).
        Ok((plan, plan_changed))
    }

    /// Name implements the base.LogicalOptRule.<1st> interface.
    // go: CollectPredicateColumnsPoint.Name L1
    fn name(&self) -> &'static str {
        "collect_predicate_columns_point"
    }
}

/// pruneIndexesForAllDataSources prunes indexes for all DataSource nodes in the plan tree.
/// It returns a map of tableID -> set of kept index IDs (indexes that remain after pruning).
// go: CollectPredicateColumnsPoint.pruneIndexesForAllDataSources L1 [M-PLAN]
fn prune_indexes_for_all_data_sources(
    ctx: &mut PlanCtx<'_>,
    plan: PlanId,
) -> HashMap<i64, HashSet<i64>> {
    let mut kept_index_ids = HashMap::new();
    collect_and_prune_data_sources(ctx, plan, &mut kept_index_ids);
    kept_index_ids
}

/// collectAndPruneDataSources recursively finds all DataSource nodes and prunes their
/// indexes. `LogicalIndexScan` / `LogicalTableScan` are built only by the old cascades
/// planner (exception B3).
// go: CollectPredicateColumnsPoint.collectAndPruneDataSources L1 [M-PLAN]
fn collect_and_prune_data_sources(
    ctx: &mut PlanCtx<'_>,
    plan: PlanId,
    kept_index_ids: &mut HashMap<i64, HashSet<i64>>,
) {
    if ctx.arena.get(plan).kind() == LogicalKind::DataSource {
        crate::arena::with_detached(ctx, plan, |node, ctx| {
            let ds = node
                .as_any_mut()
                .downcast_mut::<DataSource>()
                .expect("DataSource");
            prune_indexes_for_data_source(ds, ctx, kept_index_ids);
        });
    }

    let children = ctx.arena.get(plan).children().to_vec();
    for child in children {
        collect_and_prune_data_sources(ctx, child, kept_index_ids);
    }
}

/// pruneIndexesForDataSource prunes indexes for a single DataSource if needed.
// go: pruneIndexesForDataSource L1 [M-PLAN]
fn prune_indexes_for_data_source(
    ds: &mut DataSource,
    ctx: &PlanCtx<'_>,
    kept_index_ids: &mut HashMap<i64, HashSet<i64>>,
) {
    let mut threshold = ctx.vars.opt_index_prune_threshold as isize;
    // Early return if pruning is disabled (threshold < 0)
    if threshold < 0 {
        return;
    }
    // if ds.AllPossibleAccessPaths == nil { ds.AllPossibleAccessPaths = ds.PossibleAccessPaths }:
    // the builder always sets both.
    // If we have only one path, we don't need to prune indexes.
    if ds.all_possible_access_paths.len() <= 1 {
        return;
    }
    // If threshold = 0, we should only prune indexes with score == 0.
    // Set threshold to the original number of paths as the logic
    // will keep all indexes (up to the threshold) with score > 0.
    if threshold == 0 {
        threshold = ds.all_possible_access_paths.len() as isize;
    }

    // Prune indexes
    let mut paths = std::mem::take(&mut ds.all_possible_access_paths);
    let interesting = ds.interesting_columns.clone();
    let kept = crate::core::rule::rule_prune_indexes::prune_indexes_by_where_and_order(
        ds,
        ctx,
        &mut paths,
        &interesting,
        threshold,
    );

    // Extract kept index IDs directly from the pruned paths
    let mut table_kept_indexes = HashSet::new();
    for &i in &kept {
        let path = &paths[i];
        if !path.is_table_path()
            && let Some(index) = &path.index
        {
            table_kept_indexes.insert(index.id);
        }
        // Also keep common-handle primary index from table paths
        // Table paths for common-handle tables store the primary index in path.Index
        // TODO: Consider later pruning the common-handle primary index when
        // fillIndexPath is called - when more information is available.
        if path.is_table_path()
            && path.is_common_handle_path
            && let Some(index) = &path.index
        {
            table_kept_indexes.insert(index.id);
        }
    }

    // Only update if pruning actually occurred (paths changed)
    if kept.len() < paths.len() {
        // Union kept indexes instead of overwriting, to handle cases where the same physical table
        // appears multiple times (e.g., self-joins, repeated subqueries). Each alias may keep different
        // indexes, and we need to load stats for all indexes kept by any alias.
        kept_index_ids
            .entry(ds.physical_table_id)
            .or_default()
            .extend(table_kept_indexes);
        let pruned: Vec<_> = kept.iter().map(|&i| paths[i].clone_path()).collect();
        // Make a copy for PossibleAccessPaths to avoid sharing the same slice
        ds.possible_access_paths = pruned.iter().map(|p| p.clone_path()).collect();
        ds.all_possible_access_paths = pruned;
    } else {
        // The function may have written IsSingleScan (Go's shared paths): the possible
        // paths share them.
        for (pp, p) in ds.possible_access_paths.iter_mut().zip(&paths) {
            pp.is_single_scan = p.is_single_scan;
        }
        ds.all_possible_access_paths = paths;
    }
}

/// recordTableRuntimeStats records the statistics of the visited tables into
/// `StmtCtx.TableStats` for the plan replayer capture. A value is the table's
/// `Option<Arc<tidb_statistics::Table>>` (Go's `*statistics.Table` in an `any`, which may be
/// nil). A boundary of the lookup (see [`record_single_table_runtime_stats`]) is returned;
/// other errors are logged as Go does.
// go: recordTableRuntimeStats L1
pub fn record_table_runtime_stats(ctx: &mut PlanCtx<'_>, tbls: &HashSet<i64>) -> Result<(), Error> {
    // tblStats == nil: the Rust map always exists.
    let mut tbl_stats = std::mem::take(&mut ctx.vars.stmt_ctx_mut().table_stats);
    for &tbl_id in tbls {
        let (tbl_json_stats, skip) = match record_single_table_runtime_stats(ctx, tbl_id) {
            Ok(r) => r,
            // A boundary of the lookup is not Go's error: the statement stops here.
            Err(err) if tidb_base::blocked::blocked_by(&err).is_some() => {
                ctx.vars.stmt_ctx_mut().table_stats = tbl_stats;
                return Err(err);
            }
            Err(err) => {
                log::warn!("record table json stats failed, tblID: {tbl_id}, error: {err}");
                (None, false)
            }
        };
        if tbl_json_stats.is_none() && !skip {
            log::warn!("record table json stats failed due to empty, tblID: {tbl_id}");
        }
        let value: Arc<dyn std::any::Any + Send + Sync> = Arc::new(tbl_json_stats);
        tbl_stats.insert(tbl_id, value);
    }
    ctx.vars.stmt_ctx_mut().table_stats = tbl_stats;
    Ok(())
}

/// recordSingleTableRuntimeStats: the statistics of one visited table, and whether a missing
/// one is expected (a temporary table has none).
///
/// The new stack has no statistics handle (the statistics cache, D11.2): Go's
/// `GetPhysicalTableStats` on a nil handle (`getStatsByPhysicalID`) gives the pseudo table
/// with the physical id, which is what this returns. Go's error is always nil; here the
/// lookup's error is a boundary (a table not loaded into the InfoSchema cache, which the
/// planner reads before planning).
// go: recordSingleTableRuntimeStats L2 (no statistics handle: the nil handle's pseudo table, D11.2; lookup boundaries are returned)
fn record_single_table_runtime_stats(
    ctx: &PlanCtx<'_>,
    tbl_id: i64,
) -> Result<(Option<Arc<tidb_statistics::Table>>, bool), Error> {
    // A planning context without a domain (the test contexts; the session always gives its
    // domain's InfoSchema) has no table to find.
    let Some(is) = &ctx.domain_info_schema else {
        return Ok((None, false));
    };
    let Some(table_info) = is.table_by_id(tbl_id)? else {
        return Ok((None, false));
    };
    // statsHandle.GetPhysicalTableStats(tableInfo.ID, tableInfo) with the nil handle.
    let mut stats = tidb_statistics::table::pseudo_table(&table_info, false, false);
    stats.hist_coll.physical_id = table_info.id;
    // Skip the warning if the table is a temporary table because the temporary table doesn't have stats.
    let skip = table_info.temp_table_type != tidb_model::TempTableNone;
    Ok((Some(Arc::new(stats)), skip))
}

#[cfg(test)]
mod tests {
    use tidb_model::TableInfo;
    use tidb_parser::model::CIStr;
    use tidb_sessionctx::variable::session::{GlobalVarAccessor, SessionVars};

    use super::*;
    use crate::core::plan_cache_utils::InfoSchemaLookup;

    struct Acc;

    impl GlobalVarAccessor for Acc {
        fn get_global_sys_var(&self, _: &str) -> Result<String, Error> {
            Ok(String::new())
        }
        fn set_global_sys_var_only(&mut self, _: &str, _: &str, _: bool) -> Result<(), Error> {
            Ok(())
        }
        fn get_tidb_table_value(&self, _: &str) -> Result<String, Error> {
            Ok(String::new())
        }
        fn set_tidb_table_value(&mut self, _: &str, _: &str, _: &str) -> Result<(), Error> {
            Ok(())
        }
    }

    /// A domain InfoSchema with the given tables.
    struct Tables(Vec<Arc<TableInfo>>);

    impl InfoSchemaLookup for Tables {
        fn table_by_name(&self, _: &CIStr, _: &CIStr) -> Result<Arc<TableInfo>, Error> {
            Err(Error::plain("not used"))
        }
        fn table_by_id(&self, id: i64) -> Result<Option<Arc<TableInfo>>, Error> {
            Ok(self.0.iter().find(|t| t.id == id).map(Arc::clone))
        }
        fn schema_name_by_id(&self, _: i64) -> Option<CIStr> {
            None
        }
        fn schema_meta_version(&self) -> i64 {
            1
        }
        fn snapshot(&self) -> Arc<dyn InfoSchemaLookup> {
            Arc::new(Tables(self.0.clone()))
        }
    }

    fn stats_of(vars: &SessionVars, id: i64) -> Option<Arc<tidb_statistics::Table>> {
        vars.stmt_ctx().table_stats[&id]
            .downcast_ref::<Option<Arc<tidb_statistics::Table>>>()
            .expect("Option<Arc<Table>>")
            .clone()
    }

    /// recordTableRuntimeStats: each visited table gets the nil statistics handle's pseudo
    /// table with its physical id (a temporary table too, whose missing statistics are
    /// expected); a table the domain does not have gets nil; entries already in
    /// `StmtCtx.TableStats` stay.
    #[test]
    fn record_table_runtime_stats_records_pseudo_tables() {
        let t = |id: i64, temp| {
            Arc::new(TableInfo {
                id,
                name: CIStr::new("t"),
                temp_table_type: temp,
                ..Default::default()
            })
        };
        let mut vars = SessionVars::new(Box::new(Acc));
        let old: Arc<dyn std::any::Any + Send + Sync> = Arc::new(0);
        vars.stmt_ctx_mut().table_stats.insert(1, old);
        let mut ctx = PlanCtx::new(
            &mut vars,
            tidb_sessionctx::exprstatic::new_expr_context(Vec::new()),
        );
        ctx.domain_info_schema = Some(Arc::new(Tables(vec![
            t(10, tidb_model::TempTableNone),
            t(11, tidb_model::TempTableGlobal),
        ])));
        let tbls: HashSet<i64> = [10, 11, 12].into_iter().collect();
        record_table_runtime_stats(&mut ctx, &tbls).unwrap();
        assert!(
            record_single_table_runtime_stats(&ctx, 11).unwrap().1,
            "a temporary table skips the warning"
        );
        assert!(!record_single_table_runtime_stats(&ctx, 12).unwrap().1);

        // Without a domain there is no table to find.
        ctx.domain_info_schema = None;
        assert!(
            record_single_table_runtime_stats(&ctx, 10)
                .unwrap()
                .0
                .is_none()
        );

        assert_eq!(vars.stmt_ctx().table_stats.len(), 4);
        assert!(vars.stmt_ctx().table_stats.contains_key(&1));
        for id in [10, 11] {
            let s = stats_of(&vars, id).expect("the pseudo table");
            assert!(s.hist_coll.pseudo);
            assert_eq!(s.hist_coll.physical_id, id);
        }
        assert!(stats_of(&vars, 12).is_none());
    }
}
