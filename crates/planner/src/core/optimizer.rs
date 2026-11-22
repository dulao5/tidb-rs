//! go-file: pkg/planner/core/optimizer.go
//!
//! The optimizer driver (block D09.13): `DoOptimize` runs the logical rules
//! (`logicalOptimize`), the physical optimization (`physicalOptimize`) and the post passes
//! (`postOptimize`), plus the privilege, table lock and table mode checks the planner runs
//! after building a statement.
//!
//! What the driver reaches that is not ported (each is an explicit boundary, never a
//! silent skip; the spec `specs/D09.13-optimizer/spec.md` lists them):
//!
//! - logical rules of other blocks in `optRuleList` (join reorder, decorrelation,
//!   aggregation rules, ...): a rule whose flag is set and whose block is not ported returns
//!   `blocked-by:<block>` ([`UnportedRule`]);
//! - the two statistics loading points (`CollectPredicateColumnsPoint`,
//!   `SyncWaitStatsLoadPoint`): the new stack's statistics handle has only pseudo tables
//!   (VS1 plan §3.2, the statistics row), so there is nothing to load and they leave the plan
//!   unchanged ([`StatsLoadPoint`], registered as boundary stubs of D11.2);
//! - `physicalOptimize`: the statistics derivation (D10.6), `preparePossibleProperties` and
//!   `FindBestTask` (D10.2) and `ResolveIndices` (D10.9); the plan cost (D10.5) is called;
//! - the post passes over operators that are not ported (Apply, joins, aggregations,
//!   UnionAll, Lock, UnionScan, the index readers, ExchangeSender): their arms are not
//!   reachable, because no plan can contain those operators yet.
//!
//! The tracers (`EnableOptimizeTrace`, `EnableOptimizerCETrace`, the debug trace) are only
//! switched on by `TRACE` / `PLAN REPLAYER` statements, which the Rust AST does not have
//! (D02.2): when set they are a boundary.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};

use parking_lot::RwLock;
use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_base::config;
use tidb_base::kv;
use tidb_base::mysql;
use tidb_expression::Column;
use tidb_model::{TableModeNormal, TempTableLocal};
use tidb_parser::ast::Stmt;
use tidb_parser::model::CIStr;
use tidb_sessionctx::variable::session::SessionVars;

use crate::arena::{PlanId, with_detached};
use crate::base::{LogicalKind, LogicalOptRule, PhysicalKind, PhysicalPlan};
use crate::core::initialize::flatten_list_push_down_plan;
use crate::core::physical_plans::{PhysicalSelection, PhysicalTableReader};
use crate::core::plan_cache_utils::InfoSchemaLookup;
use crate::core::planbuilder::VisitInfo;
use crate::core::resolve::NodeW;
use crate::core::rule::logical_rules::{
    FlagCollectPredicateColumnsPoint, FlagJoinReOrder, FlagPartitionProcessor, FlagPruneColumns,
    FlagPruneColumnsAgain, FlagStabilizeResults, FlagSyncWaitStatsLoadPoint,
};
use crate::core::rule::rule_constant_propagation::ConstantPropagationSolver;
use crate::core::rule_column_pruning::ColumnPruner;
use crate::core::rule_derive_topn_from_window::DeriveTopNFromWindow;
use crate::core::rule_eliminate_projection::{ProjectionEliminator, eliminate_physical_projection};
use crate::core::rule_inject_extra_projection::inject_extra_projection;
use crate::core::rule_predicate_push_down::PPDSolver;
use crate::core::rule_predicate_simplification::PredicateSimplification;
use crate::core::rule_result_reorder::ResultReorder;
use crate::core::rule_topn_push_down::PushDownTopNOptimizer;
use crate::core::task::get_avg_row_size;
use crate::core::task_base::Task;
use crate::logicalop::DataSource;
use crate::logicalop::base_logical_plan::TaskMap;
use crate::physicalop::{PhysicalPlanRef, make_mut_plan, map_children};
use crate::planctx::PlanCtx;
use crate::property::{PhysicalProperty, StatsInfo};
use crate::util::optimizetrace::{
    LogicalOptimizeOp, default_logical_optimize_option, default_physical_optimize_option,
};
use tidb_sessionctx::planbase::PlanCounterTp;

/// AllowCartesianProduct means whether tidb allows cartesian join without equal conditions.
pub static ALLOW_CARTESIAN_PRODUCT: AtomicBool = AtomicBool::new(true);

/// A rule of `optRuleList` whose block is not ported: running it is a boundary
/// (`blocked-by:<block>`), so a statement whose flags ask for it falls back.
#[derive(Clone, Copy, Debug)]
pub struct UnportedRule {
    /// The rule's `Name()`.
    pub name: &'static str,
    /// The block that ports the rule.
    pub block: &'static str,
}

impl LogicalOptRule for UnportedRule {
    // glue: interface method of base.LogicalOptRule (a rule of a block that is not ported)
    fn optimize(
        &self,
        _ctx: &mut PlanCtx<'_>,
        _p: PlanId,
        _opt: &LogicalOptimizeOp,
    ) -> Result<(PlanId, bool), Error> {
        Err(blocked(self.block, self.name))
    }

    // glue: interface method of base.LogicalOptRule (a rule of a block that is not ported)
    fn name(&self) -> &'static str {
        self.name
    }
}

// glue: blocked-by:D09.13 (GcSubstituter.Optimize: the generated column substitution rule; only virtual generated columns set its flag, and the builder stops at them)
// glue: blocked-by:D09.13 (GcSubstituter.Name: the generated column substitution rule)
const GC_SUBSTITUTER: UnportedRule = UnportedRule {
    name: "generate_column_substitute",
    block: "D09.13",
};
// glue: blocked-by:D09.13 (BuildKeySolver.Optimize: the key-info rule: the BuildKeyInfo methods of DataSource / LogicalSelection; only aggregations, DISTINCT and subqueries set its flag)
// glue: blocked-by:D09.13 (BuildKeySolver.Name: the key-info rule)
const BUILD_KEY_SOLVER: UnportedRule = UnportedRule {
    name: "build_keys",
    block: "D09.13",
};
// glue: blocked-by:D09.6 (DecorrelateSolver.Optimize: subqueries)
// glue: blocked-by:D09.6 (DecorrelateSolver.Name: subqueries)
const DECORRELATE_SOLVER: UnportedRule = UnportedRule {
    name: "decorrelate",
    block: "D09.6",
};
// glue: blocked-by:D09.6 (SemiJoinRewriter.Optimize: subqueries)
// glue: blocked-by:D09.6 (SemiJoinRewriter.Name: subqueries)
const SEMI_JOIN_REWRITER: UnportedRule = UnportedRule {
    name: "semi_join_rewrite",
    block: "D09.6",
};
// glue: blocked-by:D09.4 (AggregationEliminator.Optimize: aggregation)
// glue: blocked-by:D09.4 (AggregationEliminator.Name: aggregation)
const AGGREGATION_ELIMINATOR: UnportedRule = UnportedRule {
    name: "aggregation_eliminate",
    block: "D09.4",
};
// glue: blocked-by:D09.4 (SkewDistinctAggRewriter.Optimize: aggregation)
// glue: blocked-by:D09.4 (SkewDistinctAggRewriter.Name: aggregation)
const SKEW_DISTINCT_AGG_REWRITER: UnportedRule = UnportedRule {
    name: "skew_distinct_agg_rewrite",
    block: "D09.4",
};
// glue: blocked-by:D09.4 (MaxMinEliminator.Optimize: aggregation)
// glue: blocked-by:D09.4 (MaxMinEliminator.Name: aggregation)
const MAX_MIN_ELIMINATOR: UnportedRule = UnportedRule {
    name: "max_min_eliminate",
    block: "D09.4",
};
// glue: blocked-by:D09.3 (ConvertOuterToInnerJoin.Optimize: joins)
// glue: blocked-by:D09.3 (ConvertOuterToInnerJoin.Name: joins)
const CONVERT_OUTER_TO_INNER_JOIN: UnportedRule = UnportedRule {
    name: "convert_outer_to_inner_joins",
    block: "D09.3",
};
// glue: blocked-by:D09.3 (OuterJoinEliminator.Optimize: joins)
// glue: blocked-by:D09.3 (OuterJoinEliminator.Name: joins)
const OUTER_JOIN_ELIMINATOR: UnportedRule = UnportedRule {
    name: "outer_join_eliminate",
    block: "D09.3",
};
// glue: blocked-by:D09.4 (AggregationPushDownSolver.Optimize: aggregation)
// glue: blocked-by:D09.4 (AggregationPushDownSolver.Name: aggregation)
const AGGREGATION_PUSH_DOWN_SOLVER: UnportedRule = UnportedRule {
    name: "aggregation_push_down",
    block: "D09.4",
};
// glue: blocked-by:D09.10 (JoinReOrderSolver.Optimize: join reorder)
// glue: blocked-by:D09.10 (JoinReOrderSolver.Name: join reorder)
const JOIN_RE_ORDER_SOLVER: UnportedRule = UnportedRule {
    name: "join_reorder",
    block: "D09.10",
};
// glue: blocked-by:D09.6 (CorrelateSolver.Optimize: subqueries)
// glue: blocked-by:D09.6 (CorrelateSolver.Name: subqueries)
const CORRELATE_SOLVER: UnportedRule = UnportedRule {
    name: "correlate",
    block: "D09.6",
};
// glue: blocked-by:D09.14 (PushDownSequenceSolver.Optimize: the sequence operator)
// glue: blocked-by:D09.14 (PushDownSequenceSolver.Name: the sequence operator)
const PUSH_DOWN_SEQUENCE_SOLVER: UnportedRule = UnportedRule {
    name: "push_down_sequence",
    block: "D09.14",
};
// glue: blocked-by:D09.4 (ResolveExpand.Optimize: GROUP BY ... WITH ROLLUP: the Expand operator)
// glue: blocked-by:D09.4 (ResolveExpand.Name: the Expand operator)
const RESOLVE_EXPAND: UnportedRule = UnportedRule {
    name: "resolve_expand",
    block: "D09.4",
};

/// The two statistics loading points of `optRuleList`. The new stack's statistics handle
/// has only pseudo tables (VS1 plan §3.2, the statistics row; the full handle is D11.2),
/// so there is no statistics item to collect usage for or to load: both leave the plan
/// unchanged. One difference in the plan: `CollectPredicateColumnsPoint` also prunes index
/// paths (`rule_prune_indexes.go`) — above `tidb_opt_index_prune_threshold` (20) by the
/// threshold, and below it too: when at least one index scores, it drops the "zero-score"
/// indexes (neither covering nor holding WHERE / ORDER BY columns, `rule_prune_indexes.go:86`).
/// So for a table with two or more secondary indexes Go removes candidate paths here and the
/// new stack does not; the pruning (`PruneIndexesByWhereAndOrder`) is ported with FindBestTask
/// (VS1-28). The new stack also records no predicate columns (docs/compatibility.md).
#[derive(Clone, Copy, Debug)]
pub struct StatsLoadPoint {
    name: &'static str,
}

impl LogicalOptRule for StatsLoadPoint {
    // glue: blocked-by:D11.2 (SyncWaitStatsLoadPoint.Optimize: waiting for the sync statistics load: nothing is loaded with pseudo statistics, VS1 plan §3.2)
    fn optimize(
        &self,
        _ctx: &mut PlanCtx<'_>,
        p: PlanId,
        _opt: &LogicalOptimizeOp,
    ) -> Result<(PlanId, bool), Error> {
        Ok((p, false))
    }

    // glue: blocked-by:D11.2 (SyncWaitStatsLoadPoint.Name: statistics loading point)
    fn name(&self) -> &'static str {
        self.name
    }
}

const COLLECT_PREDICATE_COLUMNS_POINT:
    crate::core::rule_collect_plan_stats::CollectPredicateColumnsPoint =
    crate::core::rule_collect_plan_stats::CollectPredicateColumnsPoint;
const SYNC_WAIT_STATS_LOAD_POINT: StatsLoadPoint = StatsLoadPoint {
    name: "sync_wait_stats_load_point",
};

/// The partition pruning rule (D09.12) for the plans the new stack builds: the plan
/// builder stops at partitioned tables (`blocked-by:D09.12`), and Go's rule leaves the
/// data source of a table without partitions unchanged (`rewriteDataSource` returns it as
/// is). A partitioned data source here would be a boundary.
#[derive(Clone, Copy, Debug)]
pub struct PartitionProcessorBoundary;

impl LogicalOptRule for PartitionProcessorBoundary {
    // glue: blocked-by:D09.12 (PartitionProcessor.Optimize: partition pruning: a table without partitions is left as is, a partitioned one is a boundary)
    fn optimize(
        &self,
        ctx: &mut PlanCtx<'_>,
        p: PlanId,
        _opt: &LogicalOptimizeOp,
    ) -> Result<(PlanId, bool), Error> {
        check_no_partitioned_data_source(ctx, p)?;
        Ok((p, false))
    }

    // glue: blocked-by:D09.12 (PartitionProcessor.Name: partition pruning)
    fn name(&self) -> &'static str {
        "partition_processor"
    }
}

/// `rewriteDataSource` of the partition pruning rule for a tree without partitioned
/// tables: the boundary for a partitioned data source.
// glue: the partitioned-table check of PartitionProcessor (D09.12)
fn check_no_partitioned_data_source(ctx: &PlanCtx<'_>, p: PlanId) -> Result<(), Error> {
    let node = ctx.arena.get(p);
    if node.kind() == LogicalKind::DataSource {
        let ds = node
            .as_any()
            .downcast_ref::<DataSource>()
            .expect("a DataSource");
        if ds.table_info.get_partition_info().is_some() {
            return Err(blocked("D09.12", "partition pruning (PartitionProcessor)"));
        }
    }
    for &child in node.children() {
        check_no_partitioned_data_source(ctx, child)?;
    }
    Ok(())
}

/// `optRuleList`: the logical rules in flag order (the i-th flag bit runs the i-th rule).
// go-part: optRuleList (optimizer.go var) L2 (rules of blocks not ported are UnportedRule boundaries; the statistics loading points and the partition rule: see their types)
pub fn opt_rule_list() -> [&'static dyn LogicalOptRule; 26] {
    [
        &GC_SUBSTITUTER,
        &ColumnPruner,
        &ResultReorder,
        &BUILD_KEY_SOLVER,
        &DECORRELATE_SOLVER,
        &SEMI_JOIN_REWRITER,
        &AGGREGATION_ELIMINATOR,
        &SKEW_DISTINCT_AGG_REWRITER,
        &ProjectionEliminator,
        &MAX_MIN_ELIMINATOR,
        &ConstantPropagationSolver,
        &CONVERT_OUTER_TO_INNER_JOIN,
        &PPDSolver,
        &OUTER_JOIN_ELIMINATOR,
        &PartitionProcessorBoundary,
        &COLLECT_PREDICATE_COLUMNS_POINT,
        &AGGREGATION_PUSH_DOWN_SOLVER,
        &DeriveTopNFromWindow,
        &PredicateSimplification,
        &PushDownTopNOptimizer,
        &SYNC_WAIT_STATS_LOAD_POINT,
        &JOIN_RE_ORDER_SOLVER,
        &CORRELATE_SOLVER,
        &ColumnPruner, // column pruning again at last, note it will mess up the results of buildKeySolver
        &PUSH_DOWN_SEQUENCE_SOLVER,
        &RESOLVE_EXPAND,
    ]
}

/// CheckPrivilege checks the privilege for a user. It needs the privilege manager
/// (`privilege.Manager`, D04.1); its caller only calls it when a manager is registered, and
/// the new stack has none (VS1 plan §3.2, the privilege row).
// glue: stub of CheckPrivilege, blocked-by:D04.1 (the privilege manager)
pub fn check_privilege(vs: &[VisitInfo]) -> Result<(), Error> {
    let _ = vs;
    Err(blocked("D04.1", "CheckPrivilege (the privilege manager)"))
}

/// VisitInfo4PrivCheck generates privilege check infos because privilege check of local temporary tables is different
/// with normal tables. `CREATE` statement needs `CREATE TEMPORARY TABLE` privilege from the database, and subsequent
/// statements do not need any privileges.
///
/// `CREATE TABLE`, `DROP TABLE`, `GRANT`, `DROP SEQUENCE` and `DROP PLACEMENT POLICY` are
/// not in the Rust AST (D02.2): every statement takes the default arm.
// go: VisitInfo4PrivCheck L2 (the DDL / GRANT arms need statements the Rust AST does not have)
pub fn visit_info4_priv_check(
    is: &dyn InfoSchemaLookup,
    node: Option<&Stmt>,
    vs: &[VisitInfo],
) -> Vec<VisitInfo> {
    if node.is_none() {
        return vs.to_vec();
    }
    let mut priv_visit_info = Vec::with_capacity(vs.len());
    for v in vs {
        if need_check_tmp_table_priv(is, v) {
            priv_visit_info.push(v.clone());
        }
    }
    priv_visit_info
}

// go: needCheckTmpTablePriv L1
fn need_check_tmp_table_priv(is: &dyn InfoSchemaLookup, v: &VisitInfo) -> bool {
    if !v.db.is_empty() && !v.table.is_empty() {
        // Other statements on local temporary tables except `CREATE` do not check any privileges.
        let tb = is.table_by_name(&CIStr::new(&v.db), &CIStr::new(&v.table));
        // If the table doesn't exist, we do not report errors to avoid leaking the existence of the table.
        if let Ok(tb) = tb
            && tb.temp_table_type == TempTableLocal
        {
            return false;
        }
    }
    true
}

/// CheckTableMode checks if the table is accessible by table mode, only TableModeNormal can be accessed.
///
/// `SHOW` / `EXPLAIN` are not in the Rust AST (D02.2). The error of a protected table
/// (`dbutil.CheckTableModeIsNormal`, `ErrProtectedTableMode`) is block X (import / restore
/// table modes): a table that is not in the normal mode is a boundary.
// go: CheckTableMode L2 (the protected-mode error is blocked-by:X; SHOW / EXPLAIN are not in the Rust AST)
pub fn check_table_mode(node: &NodeW<'_>) -> Result<(), Error> {
    // First make exceptions for stmt that only visit table meta;
    // For example, `describe <table_name>` and `show create table <table_name>`;
    let resolve_ctx = node.get_resolve_context();
    let resolve_ctx = resolve_ctx.lock();
    for tbl_name_w in resolve_ctx.get_table_names().values() {
        if tbl_name_w.table_info.mode != TableModeNormal {
            // glue: blocked-by:X (dbutil.CheckTableModeIsNormal: ErrProtectedTableMode of the import / restore table modes)
            return Err(blocked(
                "X",
                "a table in import / restore mode (ErrProtectedTableMode)",
            ));
        }
    }
    Ok(())
}

/// CheckTableLock checks the table lock.
///
/// The lock checker (`pkg/lock`) is block X (LOCK TABLES): with `enable-table-lock` on
/// (off by default) the check is a boundary stub.
// go: CheckTableLock L2 (the lock checker of an enabled table lock is blocked-by:X)
pub fn check_table_lock(vs: &[VisitInfo]) -> Result<(), Error> {
    if !config::table_lock_enabled() {
        return Ok(());
    }
    // checker := lock.NewChecker(ctx, is); checker.CheckTableLock(...) for each visit info.
    let _ = vs;
    Err(blocked("X", "the table lock check (LOCK TABLES)"))
}

// go: checkStableResultMode L1
pub fn check_stable_result_mode(s: &SessionVars) -> bool {
    let st = s.stmt_ctx();
    s.enable_stable_result_mode
        && (!st.in_insert_stmt && !st.in_update_stmt && !st.in_delete_stmt && !st.in_load_data_stmt)
}

/// doOptimize optimizes a logical plan into a physical plan,
/// while also returning the optimized logical plan, the final physical plan, and the cost of the final plan.
/// The returned logical plan is necessary for generating plans for Common Table Expressions (CTEs).
///
/// The failpoint `ConsumeVolcanoOptimizePanic` is not ported (no failpoints).
// go: doOptimize L1 [M-CTX]
pub fn do_optimize(
    ctx: &mut PlanCtx<'_>,
    flag: u64,
    logic: PlanId,
) -> Result<(PlanId, PhysicalPlanRef, f64), Error> {
    let flag = adjust_optimization_flags(ctx.vars, flag);
    let logic = logical_optimize(ctx, flag, logic)?;

    if !ALLOW_CARTESIAN_PRODUCT.load(Ordering::SeqCst) && exists_cartesian_product(ctx, logic) {
        return Err(tidb_base::dbterror::plannererrors::ErrCartesianProductUnsupported.clone());
    }
    let mut plan_counter = ctx.vars.stmt_ctx().stmt_hints.force_nth_plan;
    if plan_counter == 0 {
        plan_counter = -1;
    }
    let (physical, cost) = physical_optimize(ctx, logic, &mut plan_counter)?;
    let final_plan = post_optimize(ctx, physical)?;

    if ctx.vars.stmt_ctx().enable_optimizer_ce_trace {
        refine_ce_trace()?;
    }
    if ctx.vars.stmt_ctx().enable_optimize_trace {
        // sessVars.StmtCtx.OptimizeTracer.RecordFinalPlan(finalPlan.BuildPlanTrace())
        // glue: blocked-by:D02.2 (the optimizer trace is only switched on by TRACE PLAN, not in the Rust AST)
        return Err(blocked("D02.2", "the optimizer trace (TRACE PLAN)"));
    }
    Ok((logic, final_plan, cost))
}

/// `adjustOptimizationFlags`. Go reads the session through `logic.SCtx()`; here the session
/// variables are passed (the logical plan is only used for its context).
// go: adjustOptimizationFlags L1 [M-CTX]
pub fn adjust_optimization_flags(vars: &SessionVars, mut flag: u64) -> u64 {
    if check_stable_result_mode(vars) {
        flag |= FlagStabilizeResults;
    }
    if vars.stmt_ctx().stmt_hints.straight_join_order {
        // When we use the straight Join Order hint, we should disable the join reorder optimization.
        flag &= !FlagJoinReOrder;
    }
    // InternalSQLScanUserTable is for ttl scan.
    if !vars.in_restricted_sql || vars.internal_sql_scan_user_table {
        flag |= FlagCollectPredicateColumnsPoint;
        flag |= FlagSyncWaitStatsLoadPoint;
    }
    if !vars.stmt_ctx().use_dynamic_prune_mode {
        flag |= FlagPartitionProcessor; // apply partition pruning under static mode
    }
    // FlagCorrelate is added by the correlate alternative round's flag adjuster,
    // not here. EnableCorrelateSubquery is an internal flag toggled by the round.
    // A second column-prune pass is worthwhile when any rule above column
    // pruning is enabled.
    if flag & FlagPruneColumns != 0 {
        let above_prune_columns: u64 = !(FlagPruneColumns | (FlagPruneColumns - 1));
        if flag & above_prune_columns != 0 {
            flag |= FlagPruneColumnsAgain;
        }
    }
    flag
}

/// DoOptimize optimizes a logical plan to a physical plan.
///
/// The optimizer debug trace (`EnableOptimizerDebugTrace`, set by `PLAN REPLAYER`, not in
/// the Rust AST) is a boundary.
// go: DoOptimize L1 [M-CTX]
pub fn do_optimize_plan(
    ctx: &mut PlanCtx<'_>,
    flag: u64,
    logic: PlanId,
) -> Result<(PhysicalPlanRef, f64), Error> {
    if ctx.vars.stmt_ctx().enable_optimizer_debug_trace {
        // debugtrace.EnterContextCommon(sctx); defer debugtrace.LeaveContextCommon(sctx)
        // glue: blocked-by:D02.2 (the optimizer debug trace is only switched on by PLAN REPLAYER, not in the Rust AST)
        return Err(blocked(
            "D02.2",
            "the optimizer debug trace (PLAN REPLAYER)",
        ));
    }
    let (_, final_plan, cost) = do_optimize(ctx, flag, logic)?;
    Ok((final_plan, cost))
}

/// refineCETrace will adjust the content of CETrace. The cardinality estimation trace is
/// only switched on by `TRACE PLAN TARGET = 'estimation'`, which the Rust AST does not have.
// glue: stub of refineCETrace, blocked-by:D02.2 (the cardinality estimation trace is only switched on by TRACE PLAN, not in the Rust AST)
fn refine_ce_trace() -> Result<(), Error> {
    Err(blocked(
        "D02.2",
        "the cardinality estimation trace (TRACE PLAN)",
    ))
}

/// mergeContinuousSelections merge continuous selections which may occur after changing plans.
// go: mergeContinuousSelections L1 [M-PLAN]
pub fn merge_continuous_selections(p: &mut PhysicalPlanRef) {
    if p.kind() == PhysicalKind::Selection {
        loop {
            let child_sel = Arc::clone(&p.children()[0]);
            let Some(tmp) = child_sel.as_any().downcast_ref::<PhysicalSelection>() else {
                break;
            };
            let tmp_conditions = tmp.conditions.clone();
            let tmp_child = Arc::clone(&tmp.children()[0]);
            drop(child_sel);
            let sel = make_mut_plan(p)
                .as_any_mut()
                .downcast_mut::<PhysicalSelection>()
                .expect("a PhysicalSelection");
            sel.conditions.extend(tmp_conditions);
            sel.set_child(0, tmp_child);
        }
    }
    map_children(p, |mut child| {
        merge_continuous_selections(&mut child);
        child
    });
    // merge continuous selections in a coprocessor task of tiflash
    if let Some(table_reader) = p.as_any().downcast_ref::<PhysicalTableReader>()
        && table_reader.store_type == kv::TiFlash
    {
        rewrite_table_plan(p, |mut table_plan| {
            merge_continuous_selections(&mut table_plan);
            Ok(table_plan)
        })
        .expect("merging selections does not fail");
    }
}

/// Rewrites the table plan of a `PhysicalTableReader` (Go: `tr.tablePlan = f(tr.tablePlan);
/// tr.TablePlans = flattenListPushDownPlan(tr.tablePlan)`). `TablePlans` holds the same
/// nodes as `tablePlan`, so every node under a reader has two owners: writing through it
/// would copy the nodes and leave `TablePlans` on the old ones (the taskMap spike's finding;
/// `ToPB` reads `TablePlans`). The list is cleared first, so the statement owns the nodes
/// alone and `f` writes them in place, and rebuilt after. When `f` fails the reader is left
/// half-written (the chain went to `f`, the list is empty); the statement then falls back
/// and the plan is dropped, so no caller sees it (spec §4.4).
// glue: N1 §3.4 (the two owners of the nodes under a table reader)
pub(crate) fn rewrite_table_plan(
    plan: &mut PhysicalPlanRef,
    f: impl FnOnce(PhysicalPlanRef) -> Result<PhysicalPlanRef, Error>,
) -> Result<(), Error> {
    let tr = make_mut_plan(plan)
        .as_any_mut()
        .downcast_mut::<PhysicalTableReader>()
        .expect("a PhysicalTableReader");
    tr.table_plans.clear();
    let table_plan = tr.table_plan.take().expect("tablePlan");
    let res = f(table_plan);
    let tr = make_mut_plan(plan)
        .as_any_mut()
        .downcast_mut::<PhysicalTableReader>()
        .expect("a PhysicalTableReader");
    let table_plan = res?;
    tr.table_plans = flatten_list_push_down_plan(&table_plan);
    tr.table_plan = Some(table_plan);
    Ok(())
}

/// `postOptimize`: the passes over the physical plan, in Go's order.
// go: postOptimize L1 [M-CTX]
pub fn post_optimize(
    ctx: &mut PlanCtx<'_>,
    plan: PhysicalPlanRef,
) -> Result<PhysicalPlanRef, Error> {
    // some cases from update optimize will require avoiding projection elimination.
    // see comments ahead of call of DoOptimize in function of buildUpdate().
    let plan = eliminate_physical_projection(plan);
    let mut plan = inject_extra_projection(ctx, plan);
    merge_continuous_selections(&mut plan);
    let plan = eliminate_union_scan_and_lock(ctx, plan)?;
    let plan = avoid_column_evaluator_for_proj_below_union(plan);
    let mut plan = enable_parallel_apply(ctx, plan);
    handle_fine_grained_shuffle(ctx, &plan);
    propagate_probe_parents(&mut plan, &[]);
    count_star_rewrite(&plan);
    disable_reuse_chunk_if_needed(ctx, &plan);
    try_enable_late_materialization(ctx, &mut plan)?;
    generate_runtime_filter(ctx, &plan)?;
    Ok(plan)
}

/// `generateRuntimeFilter`. The generator (`RuntimeFilterGenerator`, DEF.MPP) creates
/// filters only at a `PhysicalHashJoin` of TiFlash (not ported) and assigns them to the
/// table scans below it; over a plan without hash joins its walk assigns nothing (and
/// re-flattens a TiFlash reader's unchanged table plan), so with the filter switched on
/// (a fresh `SessionVars` has mode 0, not `RFOff`, as Go's) the pass changes nothing here.
// go: generateRuntimeFilter L2 (the generator only acts at PhysicalHashJoin, not ported: no filter is generated)
fn generate_runtime_filter(ctx: &PlanCtx<'_>, plan: &PhysicalPlanRef) -> Result<(), Error> {
    if !ctx.vars.is_runtime_filter_enabled() || ctx.vars.in_restricted_sql {
        return Ok(());
    }
    // rfGenerator.GenerateRuntimeFilter(plan): its PhysicalHashJoin arm is the only source
    // of filters (see the doc).
    let _ = plan;
    Ok(())
}

/// tryEnableLateMaterialization tries to push down some filter conditions to the table scan operator
/// @brief: push down some filter conditions to the table scan operator
/// @param: sctx: session context
/// @param: plan: the physical plan to be pruned
/// @note: this optimization is only applied when the TiFlash is used.
/// @note: the following conditions should be satisfied:
///   - Only the filter conditions with high selectivity should be pushed down.
///   - The filter conditions which contain heavy cost functions should not be pushed down.
///   - Filter conditions that apply to the same column are either pushed down or not pushed down at all.
// go: tryEnableLateMaterialization L1 [M-CTX]
fn try_enable_late_materialization(
    ctx: &mut PlanCtx<'_>,
    plan: &mut PhysicalPlanRef,
) -> Result<(), Error> {
    // check if EnableLateMaterialization is set
    let tiflash_fast_scan = ctx.vars.tiflash_fast_scan;
    if ctx.vars.enable_late_materialization && !tiflash_fast_scan {
        // Go ignores the returned plan (only the children it sets are kept).
        let _ = predicate_push_down_to_table_scan(ctx, plan)?;
    }
    if ctx.vars.enable_late_materialization && tiflash_fast_scan {
        ctx.vars.stmt_ctx().append_warning(Error::plain(
            "FastScan is not compatible with late materialization, late materialization is disabled",
        ));
    }
    Ok(())
}

/// `predicatePushDownToTableScan` (`tiflash_selection_late_materialization.go`): only a
/// Selection over a TiFlash table scan is rewritten (`predicatePushDownToTableScanImpl`,
/// TiFlash late materialization, DEF.MPP), which is a boundary; for every other plan it
/// rebuilds the children as they are. Go sets the children of the node it is given and
/// returns the plan to use in its place.
// go: pkg/planner/core/tiflash_selection_late_materialization.go predicatePushDownToTableScan L2 (the TiFlash rewrite, predicatePushDownToTableScanImpl, is blocked-by:DEF.MPP)
fn predicate_push_down_to_table_scan(
    ctx: &PlanCtx<'_>,
    plan: &mut PhysicalPlanRef,
) -> Result<PhysicalPlanRef, Error> {
    match plan.kind() {
        PhysicalKind::Selection => {
            let child = Arc::clone(&plan.children()[0]);
            if child.kind() == PhysicalKind::TableScan {
                let ts = child
                    .as_any()
                    .downcast_ref::<crate::core::physical_plans::PhysicalTableScan>()
                    .expect("a PhysicalTableScan");
                if ts.store_type == kv::TiFlash {
                    // Only when the the store type is TiFlash, we will try to push down predicates.
                    // glue: blocked-by:DEF.MPP (predicatePushDownToTableScanImpl: TiFlash late materialization)
                    return Err(blocked(
                        "DEF.MPP",
                        "TiFlash late materialization (predicatePushDownToTableScanImpl)",
                    ));
                }
            } else {
                predicate_push_down_children(ctx, plan)?;
            }
        }
        PhysicalKind::TableReader => {
            // p.tablePlan = predicatePushDownToTableScan(sctx, p.tablePlan); TablePlans is
            // rebuilt from it (rewrite_table_plan: the nodes stay single-owned).
            rewrite_table_plan(plan, |mut table_plan| {
                predicate_push_down_to_table_scan(ctx, &mut table_plan)
            })?;
        }
        _ => {
            if !plan.children().is_empty() {
                predicate_push_down_children(ctx, plan)?;
            }
        }
    }
    Ok(Arc::clone(plan))
}

/// `plan.SetChildren(newChildren...)` with each child pushed down.
// go-part: pkg/planner/core/tiflash_selection_late_materialization.go predicatePushDownToTableScan (the children loop) L1
fn predicate_push_down_children(
    ctx: &PlanCtx<'_>,
    plan: &mut PhysicalPlanRef,
) -> Result<(), Error> {
    let mut err = None;
    map_children(plan, |mut child| {
        if err.is_some() {
            return child;
        }
        match predicate_push_down_to_table_scan(ctx, &mut child) {
            Ok(c) => c,
            Err(e) => {
                err = Some(e);
                child
            }
        }
    });
    err.map_or(Ok(()), Err)
}

/// countStarRewrite: `count(*) -> count(not null column)`, **only for TiFlash** (see Go's
/// comment); the pattern needs a physical aggregation, which is not ported, so no plan
/// matches yet.
// go: countStarRewrite L1
fn count_star_rewrite(plan: &PhysicalPlanRef) {
    count_star_rewrite_internal(plan);
    if let Some(table_reader) = plan.as_any().downcast_ref::<PhysicalTableReader>() {
        if let Some(table_plan) = &table_reader.table_plan {
            count_star_rewrite(table_plan);
        }
    } else {
        for child in plan.children() {
            count_star_rewrite(child);
        }
    }
}

/// `countStarRewriteInternal`: matches `PhysicalHashAgg` / `PhysicalStreamAgg` only, which
/// are not ported (D09.4 / D10.1): every ported operator takes Go's `default: return`.
// go: countStarRewriteInternal L2 (the aggregation arms come with the physical aggregations)
fn count_star_rewrite_internal(plan: &PhysicalPlanRef) {
    // match pattern any agg(count(constant)) -> tablefullscan(tiflash)
    let _ = plan;
}

/// Only for MPP(Window<-[Sort]<-ExchangeReceiver<-ExchangeSender).
///
/// ```text
/// TiFlashFineGrainedShuffleStreamCount:
/// < 0: fine grained shuffle is disabled.
/// > 0: use TiFlashFineGrainedShuffleStreamCount as stream count.
/// == 0: use TiFlashMaxThreads as stream count when it's greater than 0. Otherwise set status as uninitialized.
/// ```
// go: handleFineGrainedShuffle L1 [M-CTX]
fn handle_fine_grained_shuffle(ctx: &PlanCtx<'_>, plan: &PhysicalPlanRef) {
    let mut stream_count = ctx.vars.tiflash_fine_grained_shuffle_stream_count;
    if stream_count < 0 {
        return;
    }
    if stream_count == 0 && ctx.vars.tiflash_max_threads > 0 {
        stream_count = ctx.vars.tiflash_max_threads;
    }
    // use two separate cluster info to avoid grpc calls cost (the stream count info is
    // only read by the MPP arm of setupFineGrainedShuffle)
    let _ = stream_count;
    setup_fine_grained_shuffle(plan);
}

/// `setupFineGrainedShuffle`: only a table reader whose table plan is a
/// `PhysicalExchangeSender` (MPP, DEF.MPP, not ported) is set up; no plan has one yet, so
/// the walk changes nothing.
// go: setupFineGrainedShuffle L2 (the ExchangeSender arm comes with MPP, DEF.MPP)
fn setup_fine_grained_shuffle(plan: &PhysicalPlanRef) {
    if plan.kind() == PhysicalKind::TableReader {
        // if _, isExchangeSender := tableReader.tablePlan.(*PhysicalExchangeSender): not a
        // ported operator.
        return;
    }
    for child in plan.children() {
        setup_fine_grained_shuffle(child);
    }
}

/// propagateProbeParents doesn't affect the execution plan, it only sets the probeParents field of a PhysicalPlan.
/// It's for handling the inconsistency between row count in the statsInfo and the recorded actual row count. Please
/// see comments in PhysicalPlan for details.
///
/// Probe parents are only added below `PhysicalApply` and the index joins, which are not
/// ported: every operator here gets Go's empty list. Their representation without `Arc`
/// cycles (the parents' outer-child id and statistics, not the parent nodes) comes with
/// those operators (spec, the probe-parents section).
// go: propagateProbeParents L2 (the Apply / index join arms come with those operators; the readers' index plans with the index readers)
fn propagate_probe_parents(plan: &mut PhysicalPlanRef, probe_parents: &[PhysicalPlanRef]) {
    if plan.base_physical().probe_parents().is_empty() && probe_parents.is_empty() {
        // Go's SetProbeParents(nil) on a node that has none: nothing to write (and no copy
        // of a shared node).
    } else {
        make_mut_plan(plan).set_probe_parents(probe_parents.to_vec());
    }
    if plan.kind() == PhysicalKind::TableReader {
        if probe_parents.is_empty() && !has_probe_parents_below(plan) {
            // SetProbeParents(nil) everywhere below: nothing to write.
            return;
        }
        rewrite_table_plan(plan, |mut table_plan| {
            propagate_probe_parents(&mut table_plan, probe_parents);
            Ok(table_plan)
        })
        .expect("propagating probe parents does not fail");
        return;
    }
    map_children(plan, |mut child| {
        propagate_probe_parents(&mut child, probe_parents);
        child
    });
}

/// Whether a node of the reader's table plan has probe parents (to clear).
// glue: the no-write shortcut of propagateProbeParents (N1 §3.4)
fn has_probe_parents_below(plan: &PhysicalPlanRef) -> bool {
    let Some(tr) = plan.as_any().downcast_ref::<PhysicalTableReader>() else {
        return false;
    };
    let mut found = false;
    if let Some(tp) = &tr.table_plan {
        iterate_physical_plan(tp, &mut |p| {
            found |= !p.base_physical().probe_parents().is_empty();
            !found
        });
    }
    found
}

/// `enableParallelApply`: only `PhysicalApply` (subqueries, D09.6, not ported) is made
/// parallel; for every ported operator the pass recurses into the children.
// go: enableParallelApply L2 (the PhysicalApply arm comes with the operator)
fn enable_parallel_apply(ctx: &PlanCtx<'_>, mut plan: PhysicalPlanRef) -> PhysicalPlanRef {
    if !ctx.vars.enable_parallel_apply {
        return plan;
    }
    // the parallel apply has three limitation:
    // 1. the parallel implementation now cannot keep order;
    // 2. the inner child has to support clone;
    // 3. if one Apply is in the inner side of another Apply, it cannot be parallel, for example:
    //		The topology of 3 Apply operators are A1(A2, A3), which means A2 is the outer child of A1
    //		while A3 is the inner child. Then A1 and A2 can be parallel and A3 cannot.
    // (if apply, ok := plan.(*PhysicalApply): not a ported operator.)
    map_children(&mut plan, |child| enable_parallel_apply(ctx, child));
    plan
}

/// LogicalOptimizeTest is just exported for test.
// go: LogicalOptimizeTest L1 [M-CTX]
pub fn logical_optimize_test(
    ctx: &mut PlanCtx<'_>,
    flag: u64,
    logic: PlanId,
) -> Result<PlanId, Error> {
    logical_optimize(ctx, flag, logic)
}

/// `logicalOptimize`. Go's `optInteractionRuleList` is an empty map, so no rule runs a
/// second time (the "again" list stays empty).
// go: logicalOptimize L1 [M-CTX]
pub fn logical_optimize(ctx: &mut PlanCtx<'_>, flag: u64, logic: PlanId) -> Result<PlanId, Error> {
    if ctx.vars.stmt_ctx().enable_optimizer_debug_trace {
        // glue: blocked-by:D02.2 (the optimizer debug trace is only switched on by PLAN REPLAYER, not in the Rust AST)
        return Err(blocked(
            "D02.2",
            "the optimizer debug trace (PLAN REPLAYER)",
        ));
    }
    let opt = default_logical_optimize_option();
    if ctx.vars.stmt_ctx().enable_optimize_trace {
        // vars.StmtCtx.OptimizeTracer = &tracing.OptimizeTracer{} ...
        // glue: blocked-by:D02.2 (the optimizer trace is only switched on by TRACE PLAN, not in the Rust AST)
        return Err(blocked("D02.2", "the optimizer trace (TRACE PLAN)"));
    }
    let mut logic = logic;
    for (i, rule) in opt_rule_list().into_iter().enumerate() {
        // The order of flags is same as the order of optRule in the list.
        // We use a bitmask to record which opt rules should be used. If the i-th bit is 1, it means we should
        // apply i-th optimizing rule.
        if flag & (1 << i) == 0 || is_logical_rule_disabled(rule) {
            continue;
        }
        // logic.BuildPlanTrace: only called with a tracer, which needs TRACE PLAN (above).
        opt.append_before_rule_optimize(i as isize, rule.name(), Default::default);
        let (new_logic, _plan_changed) = rule.optimize(ctx, logic, &opt)?;
        logic = new_logic;
        // Compute interaction rules that should be optimized again: optInteractionRuleList
        // is empty.
    }

    // Trigger the interaction rule: the again list is empty.

    opt.record_final_logical_plan(Default::default);
    Ok(logic)
}

/// DefaultDisabledLogicalRulesList indicates the logical rules which should be banned.
///
/// Go fills it from `mysql.opt_rule_blacklist` when the domain loads (D06.5); the new
/// stack does not load that table yet, so the list is empty (recorded in the spec).
pub static DEFAULT_DISABLED_LOGICAL_RULES_LIST: LazyLock<RwLock<HashSet<String>>> =
    LazyLock::new(|| RwLock::new(HashSet::new()));

// go: isLogicalRuleDisabled L1
fn is_logical_rule_disabled(r: &dyn LogicalOptRule) -> bool {
    DEFAULT_DISABLED_LOGICAL_RULES_LIST
        .read()
        .contains(r.name())
}

/// `physicalOptimize`: the statistics derivation (`RecursiveDeriveStats`),
/// `preparePossibleProperties`, FindBestTask (D10.2), `ResolveIndices` (D10.9) and the plan
/// cost.
///
/// The memo of FindBestTask ([`TaskMap`], Go's `taskMap` on every logical plan) is created
/// here and dropped before the best plan is written (N1 §3 item 9.3): the plan is moved out
/// of the best task and the memo, so `ResolveIndices` and `postOptimize` write the nodes in
/// place (no `clone_node`).
// go: physicalOptimize L1 [M-CTX]
pub fn physical_optimize(
    ctx: &mut PlanCtx<'_>,
    logic: PlanId,
    plan_counter: &mut i64,
) -> Result<(PhysicalPlanRef, f64), Error> {
    if ctx.vars.stmt_ctx().enable_optimizer_debug_trace {
        // glue: blocked-by:D02.2 (the optimizer debug trace is only switched on by PLAN REPLAYER, not in the Rust AST)
        return Err(blocked(
            "D02.2",
            "the optimizer debug trace (PLAN REPLAYER)",
        ));
    }
    crate::logicalop::base_logical_plan::recursive_derive_stats(ctx, logic, &[])?;

    crate::core::property_cols_prune::prepare_possible_properties(ctx, logic);

    let prop = PhysicalProperty {
        task_tp: crate::property::RootTaskType,
        expected_cnt: f64::MAX,
        ..Default::default()
    };

    let opt = default_physical_optimize_option();
    if ctx.vars.stmt_ctx().enable_optimize_trace {
        // glue: blocked-by:D02.2 (the optimizer trace is only switched on by TRACE PLAN, not in the Rust AST)
        return Err(blocked("D02.2", "the optimizer trace (TRACE PLAN)"));
    }

    ctx.vars.stmt_ctx_mut().task_map_bak_ts = 0;
    let mut memo = TaskMap::new();
    let mut counter = PlanCounterTp(*plan_counter);
    let r = crate::base::find_best_task(ctx, logic, &mut memo, &prop, &mut counter, &opt);
    *plan_counter = counter.0;
    let (t, _) = r?;
    // The memo holds the best task of every node: drop it before writing the plan.
    drop(memo);
    if *plan_counter > 0 {
        ctx.vars
            .stmt_ctx()
            .append_warning(Error::plain("The parameter of nth_plan() is out of range"));
    }
    if t.invalid() {
        let mut err_msg = "Can't find a proper physical plan for this query".to_owned();
        if config::get_global_config().disaggregated_tiflash && !ctx.vars.is_mpp_allowed() {
            err_msg += ": cop and batchCop are not allowed in disaggregated tiflash mode, you should turn on tidb_allow_mpp switch";
        }
        return Err(tidb_base::dbterror::plannererrors::ErrInternal
            .gen_with_stack_by_args(vec![tidb_base::gofmt::Arg::from(err_msg)]));
    }

    // Move the plan out of the task (and drop the task) before writing it.
    let mut plan = take_task_plan(t);
    make_mut_plan(&mut plan).resolve_indices()?;
    let cost = crate::core::plan_cost_ver2::get_plan_cost_impl(
        ctx,
        &*plan,
        crate::property::RootTaskType,
        &crate::util::optimizetrace::new_default_plan_cost_option(),
    )?;
    Ok((plan, cost))
}

/// `t.Plan()` of the best task, moved out of it (the task is dropped).
// glue: N1 §3 item 9.3 (the best plan leaves the task before it is written)
fn take_task_plan(t: Task) -> PhysicalPlanRef {
    let plan = match t {
        Task::Root(mut rt) => rt.take_plan(),
        Task::Cop(c) => c.into_plan(),
        Task::Mpp(m) => m.p,
    };
    plan.expect("the best task's plan")
}

/// avoidColumnEvaluatorForProjBelowUnion sets AvoidColumnEvaluator to false for the projection operator which is a child of Union operator.
///
/// `PhysicalUnionAll` (set operations, D09.5) is not ported: no plan has one yet, so the
/// pass changes nothing.
// go: avoidColumnEvaluatorForProjBelowUnion L2 (the PhysicalUnionAll arm comes with the operator)
fn avoid_column_evaluator_for_proj_below_union(p: PhysicalPlanRef) -> PhysicalPlanRef {
    iterate_physical_plan(&p, &mut |_p| {
        // x, ok := p.(*PhysicalUnionAll): not a ported operator.
        true
    });
    p
}

/// eliminateUnionScanAndLock set lock property for PointGet and BatchPointGet and eliminates UnionScan and Lock.
///
/// `PhysicalLock`, `PhysicalUnionScan` and `BatchPointGetPlan` are not ported (the lock
/// operator D09.14, the union scan of dirty transactions, the batch point get D10.8): with
/// neither a lock nor a union scan in the plan, Go returns the plan unchanged.
// go: eliminateUnionScanAndLock L2 (the Lock / UnionScan / BatchPointGet arms come with those operators)
fn eliminate_union_scan_and_lock(
    ctx: &PlanCtx<'_>,
    p: PhysicalPlanRef,
) -> Result<PhysicalPlanRef, Error> {
    let _ = ctx;
    let mut point_get = false;
    iterate_physical_plan(&p, &mut |p| {
        if p.children().len() > 1 {
            return false;
        }
        if p.kind() == PhysicalKind::PointGet {
            point_get = true;
        }
        true
    });
    if !point_get {
        return Ok(p);
    }
    // physLock == nil && unionScan == nil: neither operator is ported.
    Ok(p)
}

// go: iteratePhysicalPlan L1
fn iterate_physical_plan(p: &PhysicalPlanRef, f: &mut dyn FnMut(&dyn PhysicalPlan) -> bool) {
    if !f(&**p) {
        return;
    }
    for child in p.children() {
        iterate_physical_plan(child, f);
    }
}

/// `transformPhysicalPlan`: Go's only caller (`eliminateUnionScanAndLock`) removes Lock /
/// UnionScan nodes, which are not ported.
// go: transformPhysicalPlan L1 [M-PLAN]
#[allow(dead_code)]
fn transform_physical_plan(
    mut p: PhysicalPlanRef,
    f: &mut dyn FnMut(PhysicalPlanRef) -> PhysicalPlanRef,
) -> PhysicalPlanRef {
    map_children(&mut p, |child| transform_physical_plan(child, f));
    f(p)
}

/// `existsCartesianProduct`: `LogicalJoin` (D09.3) is not ported, so a plan has no join.
// go: existsCartesianProduct L2 (the LogicalJoin arm comes with joins, D09.3)
fn exists_cartesian_product(ctx: &PlanCtx<'_>, p: PlanId) -> bool {
    // if join, ok := p.(*logicalop.LogicalJoin): not a ported operator.
    for &child in ctx.arena.get(p).children() {
        if exists_cartesian_product(ctx, child) {
            return true;
        }
    }
    false
}

// go: disableReuseChunkIfNeeded L1 [M-CTX]
fn disable_reuse_chunk_if_needed(ctx: &mut PlanCtx<'_>, plan: &PhysicalPlanRef) {
    if !ctx.vars.is_alloc_valid() {
        return;
    }
    let (disable_reuse_chunk, continue_iterating) =
        check_skip_reuse_chunk_for_overlong_type(ctx, plan);
    if disable_reuse_chunk || !continue_iterating {
        return;
    }
    for child in plan.children() {
        disable_reuse_chunk_if_needed(ctx, child);
    }
}

/// checkSkipReuseChunkForOverlongType walks the root-side physical plan tree top-down and stops at
/// the first reader / point get boundary. Only those operators materialize rows into the reusable
/// chunk owned by the current statement, so higher-level physical operators only decide whether the
/// traversal should continue.
///
/// The index readers and the batch point get are not ported operators.
// go: checkSkipReuseChunkForOverlongType L2 (the index reader / index lookup / index merge / batch point get arms come with those operators)
fn check_skip_reuse_chunk_for_overlong_type(
    ctx: &mut PlanCtx<'_>,
    plan: &PhysicalPlanRef,
) -> (bool, bool) {
    match plan.kind() {
        PhysicalKind::TableReader | PhysicalKind::PointGet => {
            if should_skip_reuse_chunk_for_physical_plan(ctx, &**plan) {
                ctx.vars.clear_alloc(None, false);
                return (true, false);
            }
        }
        // Other physical operators do not read data, so we can continue to iterate.
        _ => return (false, true),
    }
    // Once we reach a reader / point get, its children are nil or stay on the coprocessor side, so
    // no other root-side operator can affect the reusable-chunk decision.
    (false, false)
}

/// MaxMemoryLimitForOverlongType is the memory limit for overlong type column check.
/// Why is it not 128 ?
/// Because many customers allocate a portion of memory to their management programs,
/// the actual amount of usable memory does not align to 128GB.
/// TODO: We are also lacking test data for instances with less than 128GB of memory, so we need to plan the rules here.
/// TODO: internal sql can force to use chunk reuse if we ensure the memory usage is safe.
/// TODO: We can consider the limit/Topn in the future.
pub const MAX_MEMORY_LIMIT_FOR_OVERLONG_TYPE: u64 = 120 * (1 << 30);
/// `maxFlenForOverlongType`.
const MAX_FLEN_FOR_OVERLONG_TYPE: i64 = mysql::MaxBlobWidth as i64 * 2;

/// shouldSkipReuseChunkForPhysicalPlan checks whether one reader / point get should skip chunk reuse
/// because of overlong output columns. The decision is based on retained reusable-chunk memory rather
/// than full query output size.
///
/// For readers with trusted row-count stats, we estimate:
///
/// ```text
/// average size of overlong columns * min(estimated row count, MaxChunkSize)
/// ```
///
/// All relaxed paths first require server memory >= MaxMemoryLimitForOverlongType; otherwise chunk
/// reuse is unconditionally disabled. Point gets and batch point gets have an exact operator-level
/// row bound, while non-point readers only take the relaxed path when row-count stats are trusted.
/// Only readers with trusted row-count stats use observed average sizes.
// go: shouldSkipReuseChunkForPhysicalPlan L1 [M-CTX]
fn should_skip_reuse_chunk_for_physical_plan(ctx: &PlanCtx<'_>, plan: &dyn PhysicalPlan) -> bool {
    let schema = plan.schema();
    let mut total_flen: i64 = 0;
    let mut overlong_columns: Vec<Arc<Column>> = Vec::with_capacity(schema.columns.len());
    for column in &schema.columns {
        match column.ret_type.get_type() {
            mysql::TypeLongBlob
            | mysql::TypeBlob
            | mysql::TypeJSON
            | mysql::TypeTiDBVectorFloat32 => {
                // These types are still treated as unbounded here, so keep the old conservative behavior.
                return true;
            }
            mysql::TypeVarString
            | mysql::TypeVarchar
            | mysql::TypeTinyBlob
            | mysql::TypeMediumBlob => {
                // if the column is varchar and the length of
                // the column is defined to be more than 1000,
                // the column is considered a large type and
                // disable chunk_reuse.
                if column.ret_type.get_flen() <= 1000 {
                    continue;
                }
                total_flen += column.ret_type.get_flen() as i64;
                overlong_columns.push(Arc::clone(column));
            }
            _ => {}
        }
    }
    if total_flen == 0 {
        return false;
    }
    !allow_reuse_chunk_for_overlong_type(ctx, plan, &overlong_columns, total_flen)
}

/// allowReuseChunkForOverlongType applies the relaxed rule for bounded overlong columns. It first
/// checks the host-memory gate, then estimates the retained memory of one reusable chunk instead of
/// the whole result set.
// go: allowReuseChunkForOverlongType L1 [M-CTX]
fn allow_reuse_chunk_for_overlong_type(
    ctx: &PlanCtx<'_>,
    plan: &dyn PhysicalPlan,
    overlong_columns: &[Arc<Column>],
    total_flen: i64,
) -> bool {
    let total_memory = match tidb_memory::meminfo::mem_total() {
        Ok(m) => m,
        Err(_) => return false,
    };
    if total_memory == 0 || total_memory < MAX_MEMORY_LIMIT_FOR_OVERLONG_TYPE {
        return false;
    }
    let (estimated_rows, has_trusted_stats) = estimate_reusable_chunk_rows_for_overlong_type(plan);
    if estimated_rows <= 0.0 {
        // Without a trusted row bound, keep the old conservative behavior for non-point readers.
        return false;
    }

    // Estimate the retained reusable chunk size as rows per chunk * bytes per row.
    let mut rows_per_reusable_chunk = estimated_rows;
    if plan.kind() != PhysicalKind::PointGet {
        rows_per_reusable_chunk = rows_per_reusable_chunk.min(ctx.vars.max_chunk_size as f64);
    }

    let stats_info = plan.stats_info();
    let mut estimated_bytes_per_row = total_flen as f64;
    if has_trusted_stats && has_usable_overlong_type_size_stats(stats_info, overlong_columns) {
        // Real stats let us use the observed average size instead of the schema worst case.
        estimated_bytes_per_row =
            get_avg_row_size(stats_info.expect("checked above"), overlong_columns);
    }
    let estimated_reusable_chunk_bytes = rows_per_reusable_chunk * estimated_bytes_per_row;
    estimated_reusable_chunk_bytes <= MAX_FLEN_FOR_OVERLONG_TYPE as f64
}

// go: hasUsableOverlongTypeSizeStats L1
fn has_usable_overlong_type_size_stats(
    stats_info: Option<&Arc<StatsInfo>>,
    overlong_columns: &[Arc<Column>],
) -> bool {
    let Some(stats_info) = stats_info else {
        return false;
    };
    let Some(hist_coll) = &stats_info.hist_coll else {
        return false;
    };
    if hist_coll.pseudo || hist_coll.realtime_count == 0 || hist_coll.col_num() == 0 {
        return false;
    }
    for column in overlong_columns {
        let Some(col_stats) = hist_coll.get_col(column.unique_id) else {
            return false;
        };
        // Keep the schema-level fallback when this column would still fall back to EstimateTypeWidth.
        if !col_stats.is_handle
            && col_stats.histogram.tot_col_size == 0
            && col_stats.histogram.null_count != hist_coll.realtime_count
        {
            return false;
        }
    }
    true
}

/// estimateReusableChunkRowsForOverlongType returns the row bound used by the reusable-chunk memory
/// estimate together with a flag indicating whether the caller may use observed average row sizes.
/// Point gets and batch point gets have an exact operator-level row bound. Non-point readers only
/// enter the relaxed path when row-count stats are trusted.
// go: estimateReusableChunkRowsForOverlongType L2 (the batch point get and index reader arms come with those operators)
fn estimate_reusable_chunk_rows_for_overlong_type(plan: &dyn PhysicalPlan) -> (f64, bool) {
    let Some(stats_info) = plan.stats_info() else {
        return (0.0, false);
    };
    if stats_info.row_count <= 0.0 {
        return (0.0, false);
    }

    match plan.kind() {
        PhysicalKind::PointGet => (stats_info.row_count.ceil(), true),
        PhysicalKind::TableReader => {
            // Non-point readers only take the relaxed path when row-count stats are trusted.
            match &stats_info.hist_coll {
                Some(h) if !h.pseudo && h.realtime_count != 0 && h.col_num() != 0 => {
                    (stats_info.row_count.ceil(), true)
                }
                _ => (0.0, false),
            }
        }
        _ => (0.0, false),
    }
}

/// `BuildLogicalPlanForTest`: the plan builder with plan / column ids from 0 (the replays
/// do the same in their harness).
// go: BuildLogicalPlanForTest L2 (RecheckCTE is a no-op without CTEs, D09.5)
pub fn build_logical_plan_for_test<'ast>(
    ctx: &mut PlanCtx<'_>,
    node: &'ast Stmt,
    builder: &mut crate::core::planbuilder::PlanBuilder<'ast>,
) -> Result<PlanId, Error> {
    ctx.vars.state.plan_id.0.store(0, Ordering::SeqCst);
    ctx.vars.state.plan_column_id.0.store(0, Ordering::SeqCst);
    builder.build(ctx, node)
}

/// The arena node of `constant_propagation`, for the solver (`currentPlan.ConstantPropagation`).
// glue: N1 (interface dispatch of ConstantPropagation, whose first argument is the parent, not the node)
pub fn dispatch_constant_propagation(
    ctx: &mut PlanCtx<'_>,
    p: PlanId,
    parent: Option<PlanId>,
    current_child_idx: usize,
    opt: &LogicalOptimizeOp,
) -> Option<PlanId> {
    with_detached(ctx, p, |node, ctx| {
        node.constant_propagation(ctx, parent, current_child_idx, opt)
    })
}
