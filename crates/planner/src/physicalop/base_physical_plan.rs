//! go-file: pkg/planner/core/operator/physicalop/base_physical_plan.go
//!
//! `BasePhysicalPlan`, the structure every physical operator embeds, in N1's representation:
//!
//! - Go's `Self base.PhysicalPlan` back-pointer is gone (`M-SELF`): the methods that call
//!   back into the concrete operator (`p.Self.TP()`, `child.ResolveIndices()`) are the
//!   [`PhysicalPlan`] trait's default methods, which dispatch to the functions here with the
//!   node as `&dyn PhysicalPlan`; the ones that only read embedded fields are methods of
//!   [`BasePhysicalPlan`].
//! - Children are `Arc`s (N1 §3.4). `ResolveIndices` changes children and reaches them with
//!   [`make_mut_plan`] (in place while the statement owns the tree).
//! - The cost memo (`PlanCostInit` / `PlanCost` / `PlanCostVer2`) is written through `&self`:
//!   in `FindBestTask` a child is shared by many candidates and the `taskMap`, and Go fills
//!   the memo once on the shared node for all of them. Writing through `make_mut_plan` would
//!   copy every shared node on the costed path and leave the memo on the copy (review of
//!   #223). The memo is a `parking_lot::Mutex<PlanCostCache>` (`M-LOCK`): the node must stay
//!   `Sync`; `CostFlagRecalculate` overwrites a filled memo, which rules out `OnceLock`; the
//!   memo holds a `CostVer2` (a float and an `Arc` trace), too large for atomics; the lock is
//!   uncontended (one planning task) and never held while children are costed.
//! - `childrenReqProps []*PhysicalProperty` (nil entries allowed) is
//!   `Vec<Option<Arc<PhysicalProperty>>>`.
//! - `probeParents` (the IndexJoin / Apply operators above this one) keeps Go's type; nothing
//!   sets it before `propagateProbeParents` (D09.13, VS1-24), which decides how to refer to
//!   ancestors without a cycle (N1 §3.4: no parent pointers, no `Weak`).
//! - The context argument of `Clone` / `CloneWithSelf` / `CloneForPlanCacheWithSelf`
//!   (`newCtx`) is dropped (`M-CTX`, the node keeps none).

use std::sync::Arc;

use tidb_base::blocked::blocked;
use tidb_base::kv::StoreType;
use tidb_base::{Error, errors};
use tidb_execdetails::execdetails::RuntimeStatsColl;
use tidb_execdetails::tracing::opt_trace::{CandidatePlanTrace, PlanTrace};
use tidb_expression::{CorrelatedColumn, Schema};
use tidb_sessionctx::planctx::BuildPBContext;

use parking_lot::Mutex;

use super::{PhysicalPlanRef, make_mut_plan};
use crate::base::PhysicalPlan;
use crate::baseimpl::{self, Plan};
use crate::core::plan::{
    get_actual_probe_cnt_from_probe_parents, get_estimated_probe_cnt_from_probe_parents,
};
use crate::planctx::PlanCtx;
use crate::property::{PhysicalProperty, TaskType};
use crate::util::costusage::{
    CostFlagRecalculate, CostVer2, has_cost_flag, new_zero_cost_ver2, sum_cost_ver2, trace_cost,
};
use crate::util::optimizetrace::{PhysicalOptimizeOp, PlanCostOption};

/// `size.SizeOfSlice`.
const SIZE_OF_SLICE: i64 = 24;
/// `size.SizeOfPointer`.
const SIZE_OF_POINTER: i64 = 8;
/// `size.SizeOfInterface`.
const SIZE_OF_INTERFACE: i64 = 16;
/// `size.SizeOfFloat64`.
const SIZE_OF_FLOAT64: i64 = 8;
/// `size.SizeOfUint64`.
const SIZE_OF_UINT64: i64 = 8;
/// `size.SizeOfBool`.
const SIZE_OF_BOOL: i64 = 1;

/// Go's `PlanCostInit`, `PlanCost` and `PlanCostVer2` fields of `BasePhysicalPlan`: the cost
/// memo `GetPlanCostVer1/2` fill.
#[derive(Clone, Debug, Default)]
pub struct PlanCostCache {
    /// PlanCostInit.
    pub plan_cost_init: bool,
    /// PlanCost.
    pub plan_cost: f64,
    /// PlanCostVer2.
    pub plan_cost_ver2: CostVer2,
}

/// BasePhysicalPlan is the common structure that used in physical plan.
#[derive(Debug, Default)]
pub struct BasePhysicalPlan {
    /// The embedded `baseimpl.Plan`.
    pub plan: Plan,

    children_req_props: Vec<Option<Arc<PhysicalProperty>>>,
    /// children.
    pub children: Vec<PhysicalPlanRef>,

    /// used by the new cost interface: `PlanCostInit`, `PlanCost`, `PlanCostVer2`. Written
    /// through `&self` (see the module doc).
    pub cost: Mutex<PlanCostCache>,

    /// probeParents records the IndexJoins and Applys with this operator in their inner children.
    /// Please see comments in op.PhysicalPlan for details.
    probe_parents: Vec<PhysicalPlanRef>,

    /// Only for MPP. If TiFlashFineGrainedShuffleStreamCount > 0:
    /// 1. For ExchangeSender, means its output will be partitioned by hash key.
    /// 2. For ExchangeReceiver/Window/Sort, means its input is already partitioned.
    pub tiflash_fine_grained_shuffle_stream_count: u64,
}

impl Clone for BasePhysicalPlan {
    /// Go's struct copy: the cost memo is copied with the other fields.
    // glue: Clone of a struct with a lock (Go copies the fields)
    fn clone(&self) -> BasePhysicalPlan {
        BasePhysicalPlan {
            plan: self.plan.clone(),
            children_req_props: self.children_req_props.clone(),
            children: self.children.clone(),
            cost: Mutex::new(self.cost.lock().clone()),
            probe_parents: self.probe_parents.clone(),
            tiflash_fine_grained_shuffle_stream_count: self
                .tiflash_fine_grained_shuffle_stream_count,
        }
    }
}

impl std::fmt::Debug for dyn PhysicalPlan {
    // glue: Debug of a plan node (its explain id)
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.explain_id(None))
    }
}

// ******************************* start implementation of Plan interface *******************************

impl BasePhysicalPlan {
    /// ExplainInfo implements Plan ExplainInfo interface.
    // go: BasePhysicalPlan.ExplainInfo L1
    pub fn explain_info(&self) -> String {
        String::new()
    }

    /// Schema implements Plan Schema interface.
    // go: BasePhysicalPlan.Schema L1
    pub fn schema(&self) -> &Schema {
        self.children[0].schema()
    }
}

/// BuildPlanTrace implements Plan BuildPlanTrace interface.
///
/// `p` is the operator (Go's `p.Self`, never nil here).
// go: BasePhysicalPlan.BuildPlanTrace L1 [M-SELF]
pub fn build_plan_trace(p: &dyn PhysicalPlan) -> PlanTrace {
    let tp = p.tp().to_string();
    let info = p.explain_info();

    let mut plan_trace = PlanTrace::default();
    plan_trace.id = i64::from(p.id());
    plan_trace.tp = tp;
    plan_trace.explain_info = info;
    for child in p.children() {
        plan_trace.children.push(child.build_plan_trace());
    }
    plan_trace
}

// ******************************* end implementation of Plan interface *********************************

// *************************** start implementation of PhysicalPlan interface ***************************

impl BasePhysicalPlan {
    /// GetPlanCostVer1 implements the base.PhysicalPlan.<0th> interface.
    /// which calculates the cost of the plan if it has not been calculated yet and returns the cost.
    // go: BasePhysicalPlan.GetPlanCostVer1 L1 [M-LOCK]
    pub fn get_plan_cost_ver1(
        &self,
        task_type: TaskType,
        option: &PlanCostOption,
    ) -> Result<f64, Error> {
        let cost_flag = option.cost_flag;
        {
            let c = self.cost.lock();
            if c.plan_cost_init && !has_cost_flag(cost_flag, CostFlagRecalculate) {
                // just calculate the cost once and always reuse it
                return Ok(c.plan_cost);
            }
        }
        let mut plan_cost = 0.0; // the default implementation, the operator have no cost
        for child in &self.children {
            let child_cost = child.get_plan_cost_ver1(task_type, option)?;
            plan_cost += child_cost;
        }
        let mut c = self.cost.lock();
        c.plan_cost = plan_cost;
        c.plan_cost_init = true;
        Ok(c.plan_cost)
    }

    /// GetPlanCostVer2 implements the base.PhysicalPlan.<1st> interface.
    /// which calculates the cost of the plan if it has not been calculated yet and returns the cost.
    // go: BasePhysicalPlan.GetPlanCostVer2 L1 [M-LOCK]
    pub fn get_plan_cost_ver2(
        &self,
        task_type: TaskType,
        option: &PlanCostOption,
    ) -> Result<CostVer2, Error> {
        {
            let c = self.cost.lock();
            if c.plan_cost_init && !has_cost_flag(option.cost_flag, CostFlagRecalculate) {
                return Ok(c.plan_cost_ver2.clone());
            }
        }
        let mut child_costs = Vec::with_capacity(self.children.len());
        for child in &self.children {
            let child_cost = child.get_plan_cost_ver2(task_type, option)?;
            child_costs.push(child_cost);
        }
        let mut c = self.cost.lock();
        if child_costs.is_empty() {
            c.plan_cost_ver2 = new_zero_cost_ver2(trace_cost(Some(option)));
        } else {
            c.plan_cost_ver2 = sum_cost_ver2(&child_costs);
        }
        c.plan_cost_init = true;
        Ok(c.plan_cost_ver2.clone())
    }
}

/// Attach2Task implements the base.PhysicalPlan.<2nd> interface.
///
/// `base.Task`, `Task.ConvertToRootTask` and `attachPlan2Task` are the task part of D10.1
/// (VS1-27).
// glue: stub of BasePhysicalPlan.Attach2Task, blocked-by:D10.1 (VS1-27: base.Task, attachPlan2Task)
pub fn attach2_task(p: &dyn PhysicalPlan) -> Result<(), Error> {
    let _ = p;
    Err(blocked("D10.1", "Attach2Task (base.Task)"))
}

impl BasePhysicalPlan {
    /// ToPB implements the base.PhysicalPlan.<3rd> interface.
    ///
    /// Go formats `p.Plan.ExplainID()` with the node's stored context; the node keeps none
    /// (`M-CTX`) and `BuildPBContext` carries no statement context, so the id is formatted as
    /// with a nil context (`<tp>_<id>`; Go drops the suffix only under
    /// `IgnoreExplainIDSuffix`, which no ToPB caller sets).
    // go: BasePhysicalPlan.ToPB L1 [M-CTX]
    pub fn to_pb(
        &self,
        _ctx: &BuildPBContext,
        _store_type: StoreType,
    ) -> Result<tipb::Executor, Error> {
        Err(errors::errorf(
            "plan %s fails converts to PB",
            &[tidb_base::gofmt::Arg::go_string(
                self.plan.explain_id(None).into_bytes(),
            )],
        ))
    }

    /// GetChildReqProps implements the base.PhysicalPlan.<4th> interface.
    // go: BasePhysicalPlan.GetChildReqProps L1
    pub fn get_child_req_props(&self, idx: isize) -> Option<&Arc<PhysicalProperty>> {
        self.children_req_props[idx as usize].as_ref()
    }

    /// StatsCount implements the base.PhysicalPlan.<5th> interface.
    ///
    /// Panics without stats, as Go dereferences the nil pointer.
    // go: BasePhysicalPlan.StatsCount L1
    pub fn stats_count(&self) -> f64 {
        self.plan
            .stats_info()
            .expect("runtime error: invalid memory address or nil pointer dereference")
            .row_count
    }

    /// ExtractCorrelatedCols implements the base.PhysicalPlan.<6th> interface.
    // go: BasePhysicalPlan.ExtractCorrelatedCols L1
    pub fn extract_correlated_cols(&self) -> Vec<Arc<CorrelatedColumn>> {
        Vec::new()
    }

    /// Children implements the base.PhysicalPlan.<7th> interface.
    // go: BasePhysicalPlan.Children L1
    pub fn children(&self) -> &[PhysicalPlanRef] {
        &self.children
    }

    /// SetChildren implements the base.PhysicalPlan.<8th> interface.
    // go: BasePhysicalPlan.SetChildren L1
    pub fn set_children(&mut self, children: Vec<PhysicalPlanRef>) {
        self.children = children;
    }

    /// SetChild implements the base.PhysicalPlan.<9th> interface.
    // go: BasePhysicalPlan.SetChild L1
    pub fn set_child(&mut self, i: isize, child: PhysicalPlanRef) {
        self.children[i as usize] = child;
    }

    /// ResolveIndices implements the base.PhysicalPlan.<10th> interface.
    // go: BasePhysicalPlan.ResolveIndices L1 [M-PLAN]
    pub fn resolve_indices(&mut self) -> Result<(), Error> {
        for child in &mut self.children {
            make_mut_plan(child).resolve_indices()?;
        }
        Ok(())
    }

    // StatsInfo inherits the BasePhysicalPlan.Plan's implementation for <11th>.

    // SetStats inherits the BasePhysicalPlan.Plan's implementation for <12th>.

    /// ExplainNormalizedInfo implements the base.PhysicalPlan.<13th> interface.
    // go: BasePhysicalPlan.ExplainNormalizedInfo L1
    pub fn explain_normalized_info(&self) -> String {
        String::new()
    }
}

/// Clone implements the base.PhysicalPlan.<14th> interface.
///
/// Go's message names the operator's Go type (`%T` of `p.Self`, e.g.
/// `*core.PhysicalSelection`); Rust has no Go type names and uses the operator's TP.
// go: BasePhysicalPlan.Clone L2 (%T of the Go type is the operator's TP)
pub fn clone_plan(p: &dyn PhysicalPlan) -> Result<PhysicalPlanRef, Error> {
    Err(errors::errorf(
        "%s doesn't support cloning",
        &[tidb_base::gofmt::Arg::from(p.tp())],
    ))
}

/// AppendChildCandidate implements the base.PhysicalPlan.<15th> interface.
///
/// Panics like Go (nil pointer dereference) when the tracer has no candidate for `p`.
// go: BasePhysicalPlan.AppendChildCandidate L1 [M-SELF]
pub fn append_child_candidate(p: &dyn PhysicalPlan, op: &PhysicalOptimizeOp) {
    if p.children().is_empty() {
        return;
    }
    let mut children_id = Vec::new();
    for child in p.children() {
        let mut trace = PlanTrace::default();
        trace.tp = child.tp().to_string();
        trace.id = i64::from(child.id());
        trace.explain_info = child.explain_info();
        let child_candidate = CandidatePlanTrace {
            plan_trace: Some(trace),
            ..Default::default()
        };
        op.append_candidate(child_candidate);
        child.append_child_candidate(op);
        children_id.push(i64::from(child.id()));
    }
    let tracer = op
        .get_tracer()
        .expect("runtime error: invalid memory address or nil pointer dereference");
    let mut tracer = tracer.lock();
    tracer
        .candidates
        .as_mut()
        .and_then(|c| c.get_mut(&i64::from(p.id())))
        .and_then(|c| c.plan_trace.as_mut())
        .expect("runtime error: invalid memory address or nil pointer dereference")
        .append_children_id(&children_id);
}

impl BasePhysicalPlan {
    /// MemoryUsage implements the base.PhysicalPlan.<16th> interface.
    // go: BasePhysicalPlan.MemoryUsage L1
    pub fn memory_usage(&self) -> i64 {
        let mut sum = self.plan.memory_usage()
            + SIZE_OF_SLICE
            + self.children_req_props.capacity() as i64 * SIZE_OF_POINTER
            + SIZE_OF_SLICE
            + (self.children.capacity() as i64 + 1) * SIZE_OF_INTERFACE
            + SIZE_OF_FLOAT64
            + SIZE_OF_UINT64
            + SIZE_OF_BOOL;

        for prop in self.children_req_props.iter().flatten() {
            sum += prop.memory_usage();
        }
        for plan in &self.children {
            sum += plan.memory_usage();
        }
        sum
    }

    /// SetProbeParents implements base.PhysicalPlan.<17th> interface.
    // go: BasePhysicalPlan.SetProbeParents L1
    pub fn set_probe_parents(&mut self, probe_parents: Vec<PhysicalPlanRef>) {
        self.probe_parents = probe_parents;
    }

    /// GetEstRowCountForDisplay implements base.PhysicalPlan.<18th> interface.
    ///
    /// Panics without stats, as Go dereferences the nil pointer.
    // go: BasePhysicalPlan.GetEstRowCountForDisplay L1
    pub fn get_est_row_count_for_display(&self) -> f64 {
        self.stats_count() * get_estimated_probe_cnt_from_probe_parents(&self.probe_parents)
    }

    /// GetActualProbeCnt implements base.PhysicalPlan.<19th> interface.
    // go: BasePhysicalPlan.GetActualProbeCnt L1
    pub fn get_actual_probe_cnt(&self, stats_coll: &RuntimeStatsColl) -> i64 {
        get_actual_probe_cnt_from_probe_parents(&self.probe_parents, stats_coll)
    }

    // *************************** end implementation of PhysicalPlan interface *****************************

    /// CloneForPlanCacheWithSelf clones the plan with new self.
    ///
    /// Go's `(nil, false)` is `None`.
    // go: BasePhysicalPlan.CloneForPlanCacheWithSelf L1 [M-SELF, M-CTX]
    pub fn clone_for_plan_cache_with_self(&self) -> Option<BasePhysicalPlan> {
        let mut cloned = self.clone();
        cloned.children = Vec::with_capacity(self.children.len());
        for child in &self.children {
            let cloned_child = child.clone_for_plan_cache()?;
            cloned.children.push(cloned_child);
        }
        Some(cloned)
    }

    /// CloneWithSelf clones the plan with new self.
    // go: BasePhysicalPlan.CloneWithSelf L1 [M-SELF, M-CTX]
    pub fn clone_with_self(&self) -> Result<BasePhysicalPlan, Error> {
        let mut base = BasePhysicalPlan {
            plan: self.plan.clone(),
            tiflash_fine_grained_shuffle_stream_count: self
                .tiflash_fine_grained_shuffle_stream_count,
            probe_parents: self.probe_parents.clone(),
            ..Default::default()
        };
        for child in &self.children {
            let cloned = child.clone_plan()?;
            base.children.push(cloned);
        }
        for prop in self.children_req_props.iter().flatten() {
            base.children_req_props
                .push(Some(Arc::new(prop.clone_essential_fields())));
        }
        Ok(base)
    }

    /// SetChildrenReqProps set the BasePhysicalPlan's childrenReqProps.
    // go: BasePhysicalPlan.SetChildrenReqProps L1
    pub fn set_children_req_props(&mut self, req_props: Vec<Option<Arc<PhysicalProperty>>>) {
        self.children_req_props = req_props;
    }

    /// SetXthChildReqProps set the BasePhysicalPlan's x-th child as required property.
    // go: BasePhysicalPlan.SetXthChildReqProps L1
    pub fn set_xth_child_req_props(&mut self, x: isize, req_props: Option<Arc<PhysicalProperty>>) {
        self.children_req_props[x as usize] = req_props;
    }
}

/// NewBasePhysicalPlan creates a new BasePhysicalPlan.
// go: NewBasePhysicalPlan L1 [M-SELF, M-CTX]
pub fn new_base_physical_plan(
    ctx: &PlanCtx<'_>,
    tp: &'static str,
    offset: isize,
) -> BasePhysicalPlan {
    BasePhysicalPlan {
        plan: baseimpl::new_base_plan(ctx, tp, offset),
        ..Default::default()
    }
}
