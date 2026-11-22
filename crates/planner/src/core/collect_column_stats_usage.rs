//! go-file: pkg/planner/core/collect_column_stats_usage.go
//!
//! The column statistics usage of a logical plan (block D10.2): the predicate columns of
//! every table and the interesting columns of every DataSource, which the index pruning of
//! the statistics loading point reads (`DataSource.InterestingColumns`, then
//! `PruneIndexesByWhereAndOrder`).
//!
//! The operators of the walk are the slice's (DataSource, Selection, Projection, Sort, TopN,
//! Limit, TableDual); joins, applies, windows, aggregations, unions and CTEs are not ported
//! (their arms come with the operators). The predicate columns are collected as Go does, but
//! nothing reads them: the statistics usage record (`UpdateColStatsUsage`) and the loading
//! are the statistics cache (D11.2), which the new stack does not have.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tidb_expression::util::{extract_columns, extract_columns_and_cor_columns_from_expressions};
use tidb_expression::{Column, ExprRef};

use crate::arena::PlanId;
use crate::base::{LogicalKind, LogicalPlan};
use crate::logicalop::{DataSource, LogicalProjection, LogicalSelection, LogicalSort, LogicalTopN};
use crate::planctx::PlanCtx;

/// `model.TableItemID` without its load-failure flag (the map key of Go's predicate
/// columns: table id, column or index id, whether it is an index).
pub type TableItemKey = (i64, i64, bool);

/// columnStatsUsageCollector collects predicate columns and/or histogram-needed columns from logical plan.
/// Predicate columns are the columns whose statistics are utilized when making query plans, which usually occur in where conditions, join conditions and so on.
/// Histogram-needed columns are the columns whose histograms are utilized when making query plans, which usually occur in the conditions pushed down to DataSource.
/// The set of histogram-needed columns is the subset of that of predicate columns.
struct ColumnStatsUsageCollector {
    /// predicateCols records predicate columns (the value: whether full statistics are needed).
    predicate_cols: HashMap<TableItemKey, bool>,
    /// colMap maps expression.Column.UniqueID to the table columns whose statistics may be utilized to calculate statistics of the column.
    /// It is used for collecting predicate columns.
    /// For example, in `select count(distinct a, b) as e from t`, the count of column `e` is calculated as `max(ndv(t.a), ndv(t.b))` if
    /// we don't know `ndv(t.a, t.b)`(see (*LogicalAggregation).DeriveStats and getColsNDV for details). So when calculating the statistics
    /// of column `e`, we may use the statistics of column `t.a` and `t.b`.
    col_map: HashMap<i64, HashSet<TableItemKey>>,
    /// visitedPhysTblIDs all ds.PhysicalTableID that have been visited.
    /// It's always collected, even if histogram-needed columns are not collected.
    visited_phys_tbl_ids: HashSet<i64>,
    /// tblID2PartitionIDs is used for tables with static pruning mode.
    tbl_id2_partition_ids: HashMap<i64, Vec<i64>>,
    /// interestingColsByDS tracks all columns of interest for index pruning (WHERE + JOIN + ORDERING)
    /// (Go keys it by the DataSource pointer; here the arena id.)
    interesting_cols_by_ds: Option<HashMap<PlanId, Vec<Arc<Column>>>>,
    /// Temporary storage for deduplication
    col_set: HashSet<i64>,
}

/// `newColumnStatsUsageCollector`. The plan replayer capture (`collectVisitedTable`) is a
/// boundary of the caller.
// go: newColumnStatsUsageCollector L2 (the plan replayer capture's visited tables: the caller's boundary, D02.2)
fn new_column_stats_usage_collector(collect_index_pruning_cols: bool) -> ColumnStatsUsageCollector {
    ColumnStatsUsageCollector {
        predicate_cols: HashMap::new(),
        col_map: HashMap::new(),
        visited_phys_tbl_ids: HashSet::new(),
        tbl_id2_partition_ids: HashMap::new(),
        interesting_cols_by_ds: collect_index_pruning_cols.then(HashMap::new),
        col_set: HashSet::new(),
    }
}

/// `filter.IsSystemSchema` (`pkg/util/filter/schema.go`): the system databases.
// go: pkg/util/filter/schema.go IsSystemSchema L1
pub fn is_system_schema(schema: &str) -> bool {
    matches!(
        schema.to_uppercase().as_str(),
        "DM_HEARTBEAT"
            | "SYS"
            | "MYSQL"
            | "INFORMATION_SCHEMA"
            | "INSPECTION_SCHEMA"
            | "PERFORMANCE_SCHEMA"
            | "METRICS_SCHEMA"
    )
}

impl ColumnStatsUsageCollector {
    // go: columnStatsUsageCollector.addPredicateColumn L1
    fn add_predicate_column(&mut self, col: &Column, need_full_stats: bool) {
        let Some(tbl_col_ids) = self.col_map.get(&col.unique_id) else {
            // It may happen if some leaf of logical plan is LogicalMemTable/LogicalShow/LogicalShowDDLJobs.
            return;
        };
        for tbl_col_id in tbl_col_ids {
            let found = self.predicate_cols.get(tbl_col_id).copied();
            // It's already marked as full stats. Skip it.
            if found == Some(true) {
                continue;
            }
            // If it's found and marked as meta stats, and the passed mark this time is also meta stats. Skip it.
            if found == Some(false) && !need_full_stats {
                continue;
            }
            self.predicate_cols.insert(*tbl_col_id, need_full_stats);
        }
    }

    // go: columnStatsUsageCollector.addPredicateColumnsFromExpressions L1
    fn add_predicate_columns_from_expressions(&mut self, list: &[ExprRef], need_full_stats: bool) {
        let cols = extract_columns_and_cor_columns_from_expressions(Vec::new(), list);
        for col in &cols {
            self.add_predicate_column(col, need_full_stats);
        }
    }

    // go: columnStatsUsageCollector.updateColMap L1
    fn update_col_map(&mut self, col: &Column, related_cols: &[Arc<Column>]) {
        let mut ids = self.col_map.remove(&col.unique_id).unwrap_or_default();
        for related_col in related_cols {
            // (A related column that is the column itself adds its own ids: nothing.)
            let Some(tbl_col_ids) = self.col_map.get(&related_col.unique_id) else {
                // It may happen if some leaf of logical plan is LogicalMemTable/LogicalShow/LogicalShowDDLJobs.
                continue;
            };
            ids.extend(tbl_col_ids.iter().copied());
        }
        self.col_map.insert(col.unique_id, ids);
    }

    // go: columnStatsUsageCollector.updateColMapFromExpressions L1
    fn update_col_map_from_expressions(&mut self, col: &Column, list: &[ExprRef]) {
        let related = extract_columns_and_cor_columns_from_expressions(Vec::new(), list);
        self.update_col_map(col, &related);
    }

    // go: columnStatsUsageCollector.collectPredicateColumnsForDataSource L1 [M-PLAN]
    fn collect_predicate_columns_for_data_source(&mut self, ctx: &PlanCtx<'_>, ds: &DataSource) {
        // Skip all system tables.
        if is_system_schema(&ds.db_name.l) {
            return;
        }
        // For partition tables, no matter whether it is static or dynamic pruning mode, we use table ID rather than partition ID to
        // set TableColumnID.TableID. In this way, we keep the set of predicate columns consistent between different partitions and global table.
        let tbl_id = ds.table_info.id;
        // c.collectVisitedTable: the plan replayer capture (the caller's boundary).
        self.visited_phys_tbl_ids.insert(tbl_id);
        if tbl_id != ds.physical_table_id {
            self.tbl_id2_partition_ids
                .entry(tbl_id)
                .or_default()
                .push(ds.physical_table_id);
        }
        for col in &ds.schema(&ctx.arena).columns {
            // Internal pseudo columns (for example, _tidb_rowid with ID -1) don't have persisted stats.
            // Skip them when collecting predicate/histogram-needed columns.
            if col.id <= 0 {
                continue;
            }
            let tbl_col_id: TableItemKey = (tbl_id, col.id, false);
            self.col_map
                .insert(col.unique_id, HashSet::from([tbl_col_id]));
        }
        // We should use `PushedDownConds` here. `AllConds` is used for partition pruning, which doesn't need stats.
        self.add_predicate_columns_from_expressions(&ds.pushed_down_conds, true);
        // Note: Interesting columns (WHERE + JOIN + ORDERING) for index pruning are collected during the same traversal
        // in collectFromPlan via collectInterestingColumnsForDataSource method.
    }

    /// collectInterestingColumnsForDataSource collects all interesting columns (WHERE + JOIN + ORDERING + GROUP BY) for a DataSource.
    // go: columnStatsUsageCollector.collectInterestingColumnsForDataSource L1 [M-PLAN]
    fn collect_interesting_columns_for_data_source(
        &mut self,
        ctx: &PlanCtx<'_>,
        p: PlanId,
        ds: &DataSource,
        accumulated_join_cols: &[Arc<Column>],
        accumulated_ordering_cols: &[Arc<Column>],
    ) {
        // Clear the map for reuse instead of allocating a new one
        self.col_set.clear();
        let mut all_cols: Vec<Arc<Column>> = Vec::new();
        let schema = ds.schema(&ctx.arena);

        // Collect WHERE columns from local filter predicates
        let mut all_conds: Vec<ExprRef> =
            Vec::with_capacity(ds.pushed_down_conds.len() + ds.all_conds.len());
        all_conds.extend(ds.pushed_down_conds.iter().map(Arc::clone));
        all_conds.extend(ds.all_conds.iter().map(Arc::clone));

        for cond in &all_conds {
            let cond_cols = extract_columns(cond);
            // Only keep columns from this DataSource (local WHERE conditions)
            let all_local = cond_cols.iter().all(|col| schema.contains(col));
            if all_local {
                for col in cond_cols {
                    if self.col_set.insert(col.unique_id) {
                        all_cols.push(col);
                    }
                }
            }
        }

        // Add join columns, then ordering columns
        for col in accumulated_join_cols
            .iter()
            .chain(accumulated_ordering_cols)
        {
            if schema.contains(col) && self.col_set.insert(col.unique_id) {
                all_cols.push(Arc::clone(col));
            }
        }

        if let Some(m) = &mut self.interesting_cols_by_ds {
            m.insert(p, all_cols);
        }
    }

    /// collectFromPlan will dive into the tree to collect base column stats usage, in this
    /// process we also make the use of the dive process down to passing the parent
    /// operator's column groups requirement to notify the underlying datasource to maintain
    /// the possible group ndv. accumulatedJoinCols and accumulatedOrderingCols (which
    /// includes GROUP BY columns) are propagated down the tree for index pruning.
    ///
    /// The operators outside the slice (joins, applies, windows, aggregations, unions,
    /// CTEs, the old cascades scans) are not ported; their arms come with them.
    // go: columnStatsUsageCollector.collectFromPlan L2 (the arms of the operators outside the slice come with them)
    fn collect_from_plan(
        &mut self,
        ctx: &PlanCtx<'_>,
        asked_col_groups: &[Vec<Arc<Column>>],
        lp: PlanId,
        accumulated_join_cols: &[Arc<Column>],
        accumulated_ordering_cols: &[Arc<Column>],
    ) {
        let node = ctx.arena.get(lp);
        // derive the new current op's new asked column groups accordingly.
        let cur_col_groups = node.extract_col_groups(&ctx.arena, asked_col_groups);

        // Track columns for index pruning as we traverse
        let current_join_cols = accumulated_join_cols.to_vec();
        let mut current_ordering_cols = accumulated_ordering_cols.to_vec();

        // Extract join and ordering columns from this node before visiting children
        if self.interesting_cols_by_ds.is_some() {
            match node.kind() {
                LogicalKind::Sort => {
                    let x = node
                        .as_any()
                        .downcast_ref::<LogicalSort>()
                        .expect("LogicalSort");
                    for item in &x.by_items {
                        current_ordering_cols.extend(extract_columns(&item.expr));
                    }
                }
                LogicalKind::TopN => {
                    let x = node
                        .as_any()
                        .downcast_ref::<LogicalTopN>()
                        .expect("LogicalTopN");
                    for item in &x.by_items {
                        current_ordering_cols.extend(extract_columns(&item.expr));
                    }
                }
                // LogicalJoin / LogicalApply / LogicalWindow / LogicalAggregation: not ported.
                _ => {}
            }
        }

        for &child in node.children() {
            // passing the new asked column groups down, along with accumulated join/ordering columns
            self.collect_from_plan(
                ctx,
                &cur_col_groups,
                child,
                &current_join_cols,
                &current_ordering_cols,
            );
        }

        match node.kind() {
            LogicalKind::DataSource => {
                let x = node
                    .as_any()
                    .downcast_ref::<DataSource>()
                    .expect("DataSource");
                self.collect_predicate_columns_for_data_source(ctx, x);
                // Collect all interesting columns (WHERE + JOIN + ORDERING) for index pruning
                if self.interesting_cols_by_ds.is_some() {
                    self.collect_interesting_columns_for_data_source(
                        ctx,
                        lp,
                        x,
                        &current_join_cols,
                        &current_ordering_cols,
                    );
                }
            }
            LogicalKind::Projection => {
                let x = node
                    .as_any()
                    .downcast_ref::<LogicalProjection>()
                    .expect("LogicalProjection");
                // Schema change from children to self.
                let schema = x.schema(&ctx.arena);
                for (i, expr) in x.exprs.iter().enumerate() {
                    self.update_col_map_from_expressions(
                        &schema.columns[i],
                        std::slice::from_ref(expr),
                    );
                }
            }
            LogicalKind::Selection => {
                let x = node
                    .as_any()
                    .downcast_ref::<LogicalSelection>()
                    .expect("LogicalSelection");
                // Though the conditions in LogicalSelection are complex conditions which cannot be pushed down to DataSource, we still
                // regard statistics of the columns in the conditions as needed.
                self.add_predicate_columns_from_expressions(&x.conditions, false);
            }
            LogicalKind::Sort => {
                let x = node
                    .as_any()
                    .downcast_ref::<LogicalSort>()
                    .expect("LogicalSort");
                // Assume statistics of all the columns in ByItems are needed.
                for item in &x.by_items {
                    self.add_predicate_columns_from_expressions(
                        std::slice::from_ref(&item.expr),
                        false,
                    );
                }
            }
            LogicalKind::TopN => {
                let x = node
                    .as_any()
                    .downcast_ref::<LogicalTopN>()
                    .expect("LogicalTopN");
                // Assume statistics of all the columns in ByItems are needed.
                for item in &x.by_items {
                    self.add_predicate_columns_from_expressions(
                        std::slice::from_ref(&item.expr),
                        false,
                    );
                }
            }
            // Limit and TableDual have no arm; the others are not ported.
            _ => {}
        }
    }
}

// `collectPredicateColumnsForJoin` / `collectPredicateColumnsForUnionAll` come with joins (D09.3)
// and set operations (D09.5).

/// The result of [`collect_column_stats_usage`]: the predicate columns (whether full
/// statistics are needed), the visited table ids and the partition ids of the tables.
pub type ColumnStatsUsage = (
    HashMap<TableItemKey, bool>,
    HashSet<i64>,
    HashMap<i64, Vec<i64>>,
);

/// CollectColumnStatsUsage collects column stats usage from logical plan, and sets the
/// interesting columns of every DataSource when the index pruning is enabled. The plan
/// replayer capture (`recordTableRuntimeStats`) is a boundary.
// go: CollectColumnStatsUsage L2 (the plan replayer capture is a boundary, D02.2)
pub fn collect_column_stats_usage(
    ctx: &mut PlanCtx<'_>,
    lp: PlanId,
) -> Result<ColumnStatsUsage, tidb_base::Error> {
    // Check if we need to collect index pruning columns
    let threshold = ctx.vars.opt_index_prune_threshold;
    let collect_index_pruning_cols = threshold >= 0;

    if ctx.vars.is_plan_replayer_capture_enabled() {
        // glue: blocked-by:D02.2 (the plan replayer capture: recordTableRuntimeStats)
        return Err(tidb_base::blocked::blocked(
            "D02.2",
            "the plan replayer capture (recordTableRuntimeStats)",
        ));
    }
    let mut collector = new_column_stats_usage_collector(collect_index_pruning_cols);
    collector.collect_from_plan(ctx, &[], lp, &[], &[]);

    // Populate DataSource field with the collected interesting columns (if index pruning is enabled)
    if let Some(m) = collector.interesting_cols_by_ds.take() {
        for (ds, cols) in m {
            if let Some(x) = ctx.arena.downcast_mut::<DataSource>(ds) {
                x.interesting_columns = cols;
            }
        }
    }

    Ok((
        collector.predicate_cols,
        collector.visited_phys_tbl_ids,
        collector.tbl_id2_partition_ids,
    ))
}
