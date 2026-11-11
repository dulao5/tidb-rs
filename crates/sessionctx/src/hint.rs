//! go-file: pkg/util/hint/hint.go
//!
//! The statement-level hint data (`StmtHints`) that `stmtctx.StatementContext` embeds.
//!
//! Block D13.0 (D05.2 PR E): the first step of the hint block D13.3. Go's `pkg/util/hint`
//! belongs to D13.3 in `tidb-planner` (N0 rank 13), but `StatementContext` (rank 7) embeds
//! `StmtHints`, so the data type and its two data-only methods live here, below the
//! statement context; the planner re-exports them when D13.3 is ported. Hint parsing
//! (`ParseStmtHints`, `addHypoIndex`) stays in D13.3.

use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;

/// StmtHints are SessionVars related sql hints.
// go-part: StmtHints (hint.go struct) L2 (HintedHypoIndexes and OriginalTableHints are opaque
// until D13.3: the index info map and the AST hints are only read by the planner)
// Not `Clone`: Go's `Clone` drops HintedHypoIndexes; use `clone_hints`.
#[derive(Debug, Default)]
pub struct StmtHints {
    // Hint Information
    pub mem_quota_query: i64,
    pub max_execution_time: u64,
    pub replica_read: u8,
    pub allow_in_subq_to_join_and_agg: bool,
    pub no_index_merge_hint: bool,
    pub straight_join_order: bool,
    /// EnableCascadesPlanner is use cascades planner for a single query only.
    pub enable_cascades_planner: bool,
    /// ForceNthPlan indicates the PlanCounterTp number for finding physical plan.
    /// -1 for disable.
    pub force_nth_plan: i64,
    pub resource_group: String,
    pub write_slow_log: bool,

    // Hint flags
    pub has_allow_in_subq_to_join_and_agg_hint: bool,
    pub has_mem_quota_hint: bool,
    pub has_replica_read_hint: bool,
    pub has_max_execution_time: bool,
    pub has_enable_cascades_planner_hint: bool,
    pub has_resource_group: bool,
    /// SetVars is Go's `map[string]string` (`nil` and empty are not distinguished by any
    /// reader: `len(SetVars) > 0` and ranging).
    pub set_vars: HashMap<String, String>,

    /// Hypo Indexes from Hints: dbName -> tblName -> idxName -> idxInfo. Opaque until D13.3
    /// (`addHypoIndex`); `None` is Go's nil map.
    pub hinted_hypo_indexes: Option<Arc<dyn Any + Send + Sync>>,

    /// the original table hints (`[]*ast.TableOptimizerHint`), opaque until D13.3. Go's
    /// `Clone` copies the slice of pointers, so the elements are shared (`Arc`).
    pub original_table_hints: Vec<Arc<dyn Any + Send + Sync>>,
}

impl StmtHints {
    /// TaskMapNeedBackUp indicates that whether we need to back up taskMap during physical optimizing.
    // go: StmtHints.TaskMapNeedBackUp L1
    pub fn task_map_need_back_up(&self) -> bool {
        self.force_nth_plan != -1
    }

    /// Clone the StmtHints struct and returns the pointer of the new one.
    ///
    /// Like Go, `HintedHypoIndexes` is not copied (the clone has a nil map).
    // go: StmtHints.Clone L1
    pub fn clone_hints(&self) -> StmtHints {
        let vars = if !self.set_vars.is_empty() {
            self.set_vars.clone()
        } else {
            HashMap::new()
        };
        let table_hints = if !self.original_table_hints.is_empty() {
            self.original_table_hints.clone()
        } else {
            Vec::new()
        };
        StmtHints {
            mem_quota_query: self.mem_quota_query,
            max_execution_time: self.max_execution_time,
            replica_read: self.replica_read,
            allow_in_subq_to_join_and_agg: self.allow_in_subq_to_join_and_agg,
            no_index_merge_hint: self.no_index_merge_hint,
            straight_join_order: self.straight_join_order,
            enable_cascades_planner: self.enable_cascades_planner,
            force_nth_plan: self.force_nth_plan,
            resource_group: self.resource_group.clone(),
            write_slow_log: self.write_slow_log,
            has_allow_in_subq_to_join_and_agg_hint: self.has_allow_in_subq_to_join_and_agg_hint,
            has_mem_quota_hint: self.has_mem_quota_hint,
            has_replica_read_hint: self.has_replica_read_hint,
            has_max_execution_time: self.has_max_execution_time,
            has_enable_cascades_planner_hint: self.has_enable_cascades_planner_hint,
            has_resource_group: self.has_resource_group,
            set_vars: vars,
            hinted_hypo_indexes: None,
            original_table_hints: table_hints,
        }
    }
}
