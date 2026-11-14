//! go-file: pkg/planner/util/optimizetrace/opt_tracer.go
//!
//! Block D09.13, ported whole ahead of its block (constitution §4.5): it only refers to
//! `pkg/util/tracing` (D14.0, ported), and `costusage` / `BasePhysicalPlan` (VS1-19) take
//! these options in their signatures.
//!
//! Go's options hold `*tracing.*Tracer` pointers that the statement context also holds; here
//! the tracers are `Arc<Mutex<..>>` (`M-LOCK`), so the options are cheap to clone and pass by
//! value. `CostFlagTrace` is the constant itself: Go copies it from `costusage` in an `init`
//! to avoid an import cycle (`costusage.init`).

#![allow(non_upper_case_globals)]

use std::sync::Arc;

use parking_lot::Mutex;
use tidb_execdetails::tracing::opt_trace::{
    CandidatePlanTrace, LogicalOptimizeTracer, PhysicalOptimizeTracer, PlanTrace,
};

/// A tracer shared by the statement and the options (Go: a pointer).
pub type Shared<T> = Arc<Mutex<T>>;

//********************** below logical optimize trace related *************************

/// LogicalOptimizeOp  is logical optimizing option for tracing.
#[derive(Clone, Debug, Default)]
pub struct LogicalOptimizeOp {
    /// tracer is goring to track optimize steps during rule optimizing
    tracer: Option<Shared<LogicalOptimizeTracer>>,
}

impl LogicalOptimizeOp {
    /// TracerIsNil returns whether inside tracer is nil
    // go: LogicalOptimizeOp.TracerIsNil L1
    pub fn tracer_is_nil(&self) -> bool {
        self.tracer.is_none()
    }

    /// WithEnableOptimizeTracer attach the customized tracer to current LogicalOptimizeOp.
    // go: LogicalOptimizeOp.WithEnableOptimizeTracer L1
    pub fn with_enable_optimize_tracer(
        &mut self,
        tracer: Option<Shared<LogicalOptimizeTracer>>,
    ) -> &mut Self {
        self.tracer = tracer;
        self
    }

    /// AppendBeforeRuleOptimize just appends a before-rule plan tracer.
    // go: LogicalOptimizeOp.AppendBeforeRuleOptimize L1
    pub fn append_before_rule_optimize(
        &self,
        index: isize,
        name: &str,
        build: impl FnOnce() -> PlanTrace,
    ) {
        let Some(tracer) = &self.tracer else {
            return;
        };
        tracer
            .lock()
            .append_rule_tracer_before_rule_optimize(index as i64, name, &build());
    }

    /// AppendStepToCurrent appends a step of current action.
    // go: LogicalOptimizeOp.AppendStepToCurrent L1
    pub fn append_step_to_current(
        &self,
        id: isize,
        tp: &str,
        reason: impl FnOnce() -> String,
        action: impl FnOnce() -> String,
    ) {
        let Some(tracer) = &self.tracer else {
            return;
        };
        let (reason, action) = (reason(), action());
        tracer
            .lock()
            .append_rule_tracer_step_to_current(id as i64, tp, &reason, &action);
    }

    /// RecordFinalLogicalPlan records the final logical plan.
    // go: LogicalOptimizeOp.RecordFinalLogicalPlan L1
    pub fn record_final_logical_plan(&self, build: impl FnOnce() -> PlanTrace) {
        let Some(tracer) = &self.tracer else {
            return;
        };
        tracer.lock().record_final_logical_plan(&build());
    }
}

/// DefaultLogicalOptimizeOption returns the default LogicalOptimizeOp.
// go: DefaultLogicalOptimizeOption L1
pub fn default_logical_optimize_option() -> LogicalOptimizeOp {
    LogicalOptimizeOp::default()
}

//********************** below physical optimize trace related *************************

/// PhysicalOptimizeOp  is logical optimizing option for tracing.
#[derive(Clone, Debug, Default)]
pub struct PhysicalOptimizeOp {
    /// tracer is goring to track optimize steps during physical optimizing
    tracer: Option<Shared<PhysicalOptimizeTracer>>,
}

/// DefaultPhysicalOptimizeOption is default physical optimizing option.
// go: DefaultPhysicalOptimizeOption L1
pub fn default_physical_optimize_option() -> PhysicalOptimizeOp {
    PhysicalOptimizeOp::default()
}

impl PhysicalOptimizeOp {
    /// WithEnableOptimizeTracer is utility func to append the PhysicalOptimizeTracer into current PhysicalOptimizeOp.
    // go: PhysicalOptimizeOp.WithEnableOptimizeTracer L1
    pub fn with_enable_optimize_tracer(
        &mut self,
        tracer: Option<Shared<PhysicalOptimizeTracer>>,
    ) -> &mut Self {
        self.tracer = tracer;
        self
    }

    /// AppendCandidate is utility func to append the CandidatePlanTrace into current PhysicalOptimizeOp.
    /// Like Go it panics (nil pointer dereference) without a tracer.
    // go: PhysicalOptimizeOp.AppendCandidate L1
    pub fn append_candidate(&self, c: CandidatePlanTrace) {
        self.tracer
            .as_ref()
            .expect("runtime error: invalid memory address or nil pointer dereference")
            .lock()
            .append_candidate(c);
    }

    /// GetTracer returns the current op's PhysicalOptimizeTracer.
    // go: PhysicalOptimizeOp.GetTracer L1
    pub fn get_tracer(&self) -> Option<&Shared<PhysicalOptimizeTracer>> {
        self.tracer.as_ref()
    }
}

/// NewDefaultPlanCostOption returns PlanCostOption
// go: NewDefaultPlanCostOption L1
pub fn new_default_plan_cost_option() -> PlanCostOption {
    PlanCostOption::default()
}

/// PlanCostOption indicates option during GetPlanCost
#[derive(Clone, Debug, Default)]
pub struct PlanCostOption {
    /// CostFlag.
    pub cost_flag: u64,
    tracer: Option<PhysicalOptimizeOp>,
}

impl PlanCostOption {
    /// GetTracer returns the current op's PhysicalOptimizeOp.
    // go: PlanCostOption.GetTracer L1
    pub fn get_tracer(&self) -> Option<&PhysicalOptimizeOp> {
        self.tracer.as_ref()
    }

    /// WithCostFlag set cost flag
    ///
    /// Go's nil receiver returns nil; the Rust receiver is never nil.
    // go: PlanCostOption.WithCostFlag L1
    pub fn with_cost_flag(&mut self, flag: u64) -> &mut Self {
        self.cost_flag = flag;
        self
    }

    /// WithOptimizeTracer set tracer
    // go: PlanCostOption.WithOptimizeTracer L1
    pub fn with_optimize_tracer(&mut self, v: Option<PhysicalOptimizeOp>) -> &mut Self {
        if let Some(v) = &v
            && v.tracer.is_some()
        {
            self.cost_flag |= CostFlagTrace;
        }
        self.tracer = v;
        self
    }
}

/// CostFlagTrace is a mirror of costusage.CostFlagTrace, leveraging
/// initialization assignment to avoid import cycle.
pub const CostFlagTrace: u64 = super::costusage::CostFlagTrace;
