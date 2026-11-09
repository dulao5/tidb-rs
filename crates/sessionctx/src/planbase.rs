//! go-file: pkg/planner/core/base/plan_base.go
//!
//! Block D00.1 holds only `PlanCounterTp` of this file; the plan interfaces (`Plan`,
//! `LogicalPlan`, `PhysicalPlan`, `Task`) come with the planner blocks (P3).

/// PlanCounterTp is used in hint nth_plan() to indicate which plan to use.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlanCounterTp(pub i64);

impl PlanCounterTp {
    /// Dec minus PlanCounterTp value by x.
    ///
    /// Go computes `int64(*c) - x` with wrapping int64 arithmetic.
    // go: PlanCounterTp.Dec L1
    pub fn dec(&mut self, x: i64) {
        if self.0 <= 0 {
            return;
        }
        self.0 = self.0.wrapping_sub(x);
        if self.0 < 0 {
            self.0 = 0;
        }
    }

    /// Empty indicates whether the PlanCounterTp is clear now.
    // go: PlanCounterTp.Empty L1
    pub fn empty(&self) -> bool {
        self.0 == 0
    }

    /// IsForce indicates whether to force a plan.
    // go: PlanCounterTp.IsForce L1
    pub fn is_force(&self) -> bool {
        self.0 != -1
    }
}
