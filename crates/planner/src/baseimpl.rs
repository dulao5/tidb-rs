//! go-file: pkg/planner/core/operator/baseimpl/plan.go
//!
//! Go package `pkg/planner/core/operator/baseimpl`: `Plan`, the structure every plan node
//! embeds.
//!
//! Mapping:
//! - Go's `ctx planctx.PlanContext` field is not stored (`M-CTX`; N1 §7 item 2: a plan node
//!   must not keep the session context, the instance plan cache shares nodes between
//!   sessions). Methods that read it take the context they need as an argument
//!   (`ExplainID`), `NewBasePlan` takes the statement's `PlanCtx`. So `SCtx` / `SetSCtx`
//!   have nothing to return or set: the context is the `PlanCtx` (or `StatementContext`) the
//!   caller already has, and `CloneWithNewCtx` is a copy.
//! - `stats *property.StatsInfo` (`plan-cache-clone:"shallow"`) is a shared
//!   `Arc<StatsInfo>`; `id` is Go's plan id, not the arena index (N1 §3.6).
//! - `ExplainID` returns Go's `stringutil.MemoizeStr` value already computed.

use std::sync::Arc;

use tidb_execdetails::tracing::opt_trace::PlanTrace;
use tidb_expression::Column;
use tidb_sessionctx::stmtctx::StatementContext;
use tidb_types::field_name::NameSlice;

use crate::planctx::PlanCtx;
use crate::property::StatsInfo;

/// Plan Should be used as embedded struct in Plan implementations.
#[derive(Clone, Debug, Default)]
pub struct Plan {
    stats: Option<Arc<StatsInfo>>,
    tp: &'static str,
    id: i32,
    /// Query Block offset
    qb_block: isize,
}

/// NewBasePlan creates a new base plan.
// go: NewBasePlan L1 [M-CTX]
pub fn new_base_plan(ctx: &PlanCtx<'_>, tp: &'static str, qb_block: isize) -> Plan {
    let id = ctx.alloc_plan_id();
    Plan {
        tp,
        id,
        qb_block,
        stats: None,
    }
}

// go: Plan.SCtx L1 [M-CTX]
// go: Plan.SetSCtx L1 [M-CTX]
// SCtx / SetSCtx: the node keeps no context (see the module doc); callers use the `PlanCtx`
// they are given, which is what `p.SCtx()` returns in Go.

impl Plan {
    /// OutputNames returns the outputting names of each column.
    // go: Plan.OutputNames L1
    pub fn output_names(&self) -> NameSlice {
        NameSlice::new()
    }

    /// SetOutputNames sets the outputting name by the given slice.
    // go: Plan.SetOutputNames L1
    pub fn set_output_names(&mut self, _names: NameSlice) {}

    /// ReplaceExprColumns implements Plan interface.
    // go: Plan.ReplaceExprColumns L1
    pub fn replace_expr_columns(
        &mut self,
        _replace: &std::collections::HashMap<String, Arc<Column>>,
    ) {
    }

    /// ID is to get the id.
    // go: Plan.ID L1
    pub fn id(&self) -> i32 {
        self.id
    }

    /// SetID is to set id.
    // go: Plan.SetID L1
    pub fn set_id(&mut self, id: i32) {
        self.id = id;
    }

    /// StatsInfo is to get the stats info.
    // go: Plan.StatsInfo L1
    pub fn stats_info(&self) -> Option<&Arc<StatsInfo>> {
        self.stats.as_ref()
    }

    /// ExplainInfo is to get the explain information.
    // go: Plan.ExplainInfo L1
    pub fn explain_info(&self) -> String {
        "N/A".to_string()
    }

    /// ExplainID is to get the explain ID.
    ///
    /// `sc` is the statement context of Go's `p.ctx` (`None` for a nil context).
    // go: Plan.ExplainID L1 [M-CTX]
    pub fn explain_id(&self, sc: Option<&StatementContext>) -> String {
        if let Some(sc) = sc
            && sc.ignore_explain_id_suffix
        {
            return self.tp.to_string();
        }
        format!("{}_{}", self.tp, self.id)
    }

    /// TP is to get the tp.
    // go: Plan.TP L1
    pub fn tp(&self) -> &'static str {
        self.tp
    }

    /// SetTP is to set the tp.
    // go: Plan.SetTP L1
    pub fn set_tp(&mut self, tp: &'static str) {
        self.tp = tp;
    }

    /// QueryBlockOffset is to get the select block offset.
    // go: Plan.QueryBlockOffset L1
    pub fn query_block_offset(&self) -> isize {
        self.qb_block
    }

    /// SetStats sets the stats
    // go: Plan.SetStats L1
    pub fn set_stats(&mut self, s: Option<Arc<StatsInfo>>) {
        self.stats = s;
    }

    /// MemoryUsage return the memory usage of BasePlan
    // go: Plan.MemoryUsage L2 (unsafe.Sizeof(Plan{}) is Go's size on 64-bit targets)
    pub fn memory_usage(&self) -> i64 {
        PlanSize + self.tp.len() as i64
    }

    /// BuildPlanTrace is to build the plan trace.
    // go: Plan.BuildPlanTrace L1
    pub fn build_plan_trace(&self) -> PlanTrace {
        let mut plan_trace = PlanTrace::default();
        plan_trace.id = i64::from(self.id());
        plan_trace.tp = self.tp().to_string();
        plan_trace
    }

    /// CloneWithNewCtx clones the plan with new context.
    // go: Plan.CloneWithNewCtx L1 [M-CTX]
    pub fn clone_with_new_ctx(&self) -> Plan {
        self.clone()
    }

    /// CloneForPlanCache clones the plan for Plan Cache.
    // go: Plan.CloneForPlanCache L1
    pub fn clone_for_plan_cache(&self) -> Option<Plan> {
        None
    }
}

/// PlanSize is the size of BasePlan (`unsafe.Sizeof(Plan{})` of Go v8.5.8 on amd64 / arm64).
#[allow(non_upper_case_globals)]
pub const PlanSize: i64 = 56;
