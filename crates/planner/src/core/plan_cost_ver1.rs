//! go-file: pkg/planner/core/plan_cost_ver1.go
//!
//! The cost model v1 (`tidb_cost_model_version=1`) is not ported: it is exception B5 (a
//! non-default path; `getPlanCost` returns a boundary error for it, `plan_cost_ver2.rs`).
//! This file holds the helpers of `plan_cost_ver1.go` that the cost model v2 calls
//! (`hasCostFlag`, `getOperatorActRows`, `getCardinality`, the model version constants) and
//! the stub the point get executor calls. The FMA sites of the file (mapping rule `M-FMA`)
//! are all in the v1 formulas; none is in the functions ported here
//! (`specs/D10.5-cost-model/spec.md` §5).

// The model version constants keep their Go names (M-GONAME).
#![allow(non_upper_case_globals)]

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_base::gostd::math;

use crate::base::PhysicalPlan;
use crate::core::point_get_plan::PointGetPlan;
use crate::planctx::PlanCtx;
use crate::util::costusage::CostFlagUseTrueCardinality;

/// The cost model v1.
pub const modelVer1: i64 = 1;
/// The cost model v2.
pub const modelVer2: i64 = 2;

// go: hasCostFlag L1
pub fn has_cost_flag(cost_flag: u64, flag: u64) -> bool {
    (cost_flag & flag) > 0
}

impl PointGetPlan {
    /// `GetAvgRowSize` (`plan_cost_ver1.go`, D10.5): the row size estimate of the
    /// closest-adaptive replica read adjuster (cardinality of the accessed columns).
    // glue: stub of PointGetPlan.GetAvgRowSize, blocked-by:D10.5 (row size of a point get for tidb_replica_read=closest-adaptive)
    pub fn get_avg_row_size(&self) -> Result<f64, Error> {
        Err(blocked(
            "D10.5",
            "the row size of a point get (tidb_replica_read=closest-adaptive)",
        ))
    }
}

/// `getOperatorActRows`: the actual rows of the operator from the statement's runtime stats.
///
/// Go's nil `operator` returns 0; a `&dyn PhysicalPlan` is never nil. Go dereferences a nil
/// `RuntimeStatsColl` (no EXPLAIN ANALYZE); so does this.
// go: getOperatorActRows L1 [M-CTX]
pub fn get_operator_act_rows(ctx: &PlanCtx<'_>, operator: &dyn PhysicalPlan) -> f64 {
    let runtime_info = ctx
        .vars
        .stmt_ctx()
        .runtime_stats_coll
        .as_ref()
        .expect("runtime error: invalid memory address or nil pointer dereference");
    let id = i64::from(operator.id());
    if runtime_info.exists_root_stats(id) {
        runtime_info.get_root_stats(id).get_act_rows() as f64
    } else if runtime_info.exists_cop_stats(id) {
        let cop = runtime_info
            .get_cop_stats(id)
            .expect("runtime error: invalid memory address or nil pointer dereference");
        cop.lock().unwrap_or_else(|e| e.into_inner()).get_act_rows() as f64
    } else {
        0.0 // no data for this operator
    }
}

/// `getCardinality`: the estimated rows of the operator (with `CostFlagUseTrueCardinality`,
/// the actual rows per probe from the runtime stats).
///
/// Go reads the session's `StmtCtx.RuntimeStatsColl` and passes it to `GetActualProbeCnt`
/// even when it is nil (the probe parents do not dereference it when there are none, and
/// `getOperatorActRows` then dereferences it); the Rust method takes a reference, so a nil
/// collection panics before the probe count, with the same result (the count of an operator
/// without probe parents is 1, never 0).
// go: getCardinality L1 [M-CTX]
pub fn get_cardinality(ctx: &PlanCtx<'_>, operator: &dyn PhysicalPlan, cost_flag: u64) -> f64 {
    if has_cost_flag(cost_flag, CostFlagUseTrueCardinality) {
        let stats_coll = ctx
            .vars
            .stmt_ctx()
            .runtime_stats_coll
            .as_ref()
            .expect("runtime error: invalid memory address or nil pointer dereference");
        let actual_probe_cnt = operator.get_actual_probe_cnt(stats_coll);
        if actual_probe_cnt == 0 {
            return 0.0;
        }
        return math::max(
            0.0,
            get_operator_act_rows(ctx, operator) / actual_probe_cnt as f64,
        );
    }
    let mut rows = operator.stats_count();
    if rows <= 0.0 && ctx.vars.cost_model_version == modelVer2 {
        // 0 est-row can lead to 0 operator cost which makes plan choice unstable.
        rows = 1.0;
    }
    rows
}
