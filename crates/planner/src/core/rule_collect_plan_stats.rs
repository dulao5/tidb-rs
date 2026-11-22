//! go-file: pkg/planner/core/rule_collect_plan_stats.go
//!
//! The statistics loading point `CollectPredicateColumnsPoint` (block D09.13): the column
//! statistics usage and the index pruning of every DataSource (`PruneIndexesByWhereAndOrder`,
//! ported with FindBestTask, D10.2). The statistics usage record and the synchronous /
//! asynchronous loading are the statistics cache (D11.2): the new stack has only pseudo
//! statistics (VS1 plan §3.2), so there is nothing to load; those parts stay boundaries of
//! D11.2 (registered in the optimizer's rule list).

use std::collections::{HashMap, HashSet};

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
