//! go-file: pkg/planner/core/operator/logicalop/base_logical_plan.go
//!
//! `BaseLogicalPlan`, the structure every logical operator embeds, and its methods: the
//! default implementations of `base.LogicalPlan` (the trait's default methods dispatch here).
//!
//! Mapping:
//! - `BaseLogicalPlan` embeds `baseimpl.Plan` as Go does (the open question of the VS1 tasks
//!   list: one `Plan` for both sides, and the query block offset is `isize` as on the
//!   physical side, `M-INT`).
//! - Go's `self` back-pointer is gone (`M-SELF`): a method that needs the outer operator is a
//!   generic function over `P: LogicalPlan`, called by the trait's default method with the
//!   operator itself; `p.self` as a return value is the node's own arena id `this`.
//! - `taskMap` / `taskMapBak` / `taskMapBakTS` are the memo of `FindBestTask` (D10.2), side
//!   tables keyed by `PlanId` (N1 §3.2): their functions here are boundary stubs until then.
//! - `fdSet` is the functional dependency set of `pkg/planner/funcdep` (not ported): the FD
//!   functions are boundary stubs.

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_expression::{CorrelatedColumn, ExprRef, Schema};
use tidb_types::field_name::NameSlice;

use crate::arena::{PlanArena, PlanId};
use crate::base::{self, LogicalPlan};
use crate::baseimpl::{self, Plan};
use crate::logicalop::logical_plans_misc::{
    add_selection, has_max_one_row, push_down_top_n_for_base_logical_plan,
};
use crate::planctx::PlanCtx;
use crate::util::misc::encode_int_as_uint32;
use crate::util::optimizetrace::LogicalOptimizeOp;

/// BaseLogicalPlan is the common structure that used in logical plan.
#[derive(Debug)]
pub struct BaseLogicalPlan {
    /// The embedded `baseimpl.Plan`.
    pub plan: Plan,
    /// maxOneRow.
    pub max_one_row: bool,
    /// children.
    pub children: Vec<PlanId>,
}

// go-exception: EXC.B BaseLogicalPlan.Hash64 (cascades base.HashEquals; D08.1 spec §4 item 2)
// go-exception: EXC.B BaseLogicalPlan.Equals (cascades base.HashEquals; D08.1 spec §4 item 2)

/// ExplainInfo implements Plan interface.
// go: BaseLogicalPlan.ExplainInfo L1
pub fn explain_info() -> String {
    String::new()
}

/// Schema implements Plan Schema interface.
// go: BaseLogicalPlan.Schema L1 [M-PLAN]
pub fn schema<'a>(p: &BaseLogicalPlan, arena: &'a PlanArena) -> &'a Schema {
    arena.get(p.children[0]).schema(arena)
}

/// OutputNames implements Plan Schema interface.
// go: BaseLogicalPlan.OutputNames L1 [M-PLAN]
pub fn output_names<'a>(p: &BaseLogicalPlan, arena: &'a PlanArena) -> &'a NameSlice {
    arena.get(p.children[0]).output_names(arena)
}

/// SetOutputNames implements Plan Schema interface: writes the names of the child (or the
/// first descendant that owns them).
// go: BaseLogicalPlan.SetOutputNames L1 [M-PLAN]
pub fn set_output_names(p: &BaseLogicalPlan, arena: &mut PlanArena, names: NameSlice) {
    arena.set_output_names(p.children[0], names);
}

/// BuildPlanTrace implements Plan. An `Err` is the boundary of an operator's
/// `ExplainInfo` (D10.9).
// go: BaseLogicalPlan.BuildPlanTrace L1 [M-SELF]
pub fn build_plan_trace(
    ctx: &PlanCtx<'_>,
    p: PlanId,
) -> Result<tidb_execdetails::tracing::opt_trace::PlanTrace, Error> {
    let node = ctx.arena.get(p);
    let mut plan_trace = tidb_execdetails::tracing::opt_trace::PlanTrace::default();
    plan_trace.id = i64::from(node.id());
    plan_trace.tp = node.tp().to_string();
    plan_trace.explain_info = node.explain_info(ctx)?;
    for &child in node.children() {
        plan_trace.children.push(build_plan_trace(ctx, child)?);
    }
    Ok(plan_trace)
}

impl BaseLogicalPlan {
    /// HashCode implements LogicalPlan.<0th> interface.
    // go: BaseLogicalPlan.HashCode L1
    pub fn hash_code(&self) -> Vec<u8> {
        // We use PlanID for the default hash, so if two plans do not have
        // the same id, the hash value will never be the same.
        let mut result = Vec::with_capacity(4);
        encode_int_as_uint32(&mut result, self.plan.id() as isize);
        result
    }

    /// MaxOneRow implements the LogicalPlan.<16th> interface.
    // go: BaseLogicalPlan.MaxOneRow L1
    pub fn max_one_row(&self) -> bool {
        self.max_one_row
    }

    /// Children implements LogicalPlan.<17th> interface.
    // go: BaseLogicalPlan.Children L1 [M-PLAN]
    pub fn children(&self) -> &[PlanId] {
        &self.children
    }

    /// SetChildren implements LogicalPlan.<18th> interface.
    // go: BaseLogicalPlan.SetChildren L1 [M-PLAN]
    pub fn set_children(&mut self, children: Vec<PlanId>) {
        self.children = children;
    }

    /// SetChild implements LogicalPlan.<19th> interface.
    // go: BaseLogicalPlan.SetChild L1 [M-PLAN]
    pub fn set_child(&mut self, i: usize, child: PlanId) {
        self.children[i] = child;
    }

    /// ChildLen returns the child length of BaseLogicalPlan.
    // go: BaseLogicalPlan.ChildLen L1
    pub fn child_len(&self) -> usize {
        self.children.len()
    }

    /// SetMaxOneRow sets the maxOneRow of BaseLogicalPlan.
    // go: BaseLogicalPlan.SetMaxOneRow L1
    pub fn set_max_one_row(&mut self, b: bool) {
        self.max_one_row = b;
    }
}

/// PredicatePushDown implements LogicalPlan.<1st> interface.
// go: BaseLogicalPlan.PredicatePushDown L1 [M-SELF, M-PLAN]
pub fn predicate_push_down<P: LogicalPlan + ?Sized>(
    p: &mut P,
    ctx: &mut PlanCtx<'_>,
    this: PlanId,
    predicates: Vec<ExprRef>,
    opt: &LogicalOptimizeOp,
) -> Result<(Vec<ExprRef>, PlanId), Error> {
    if p.children().is_empty() {
        return Ok((predicates, this));
    }
    let child = p.children()[0];
    let (rest, new_child) = base::predicate_push_down(ctx, child, predicates, opt)?;
    add_selection(p, ctx, new_child, rest, 0, opt);
    Ok((Vec::new(), this))
}

/// PruneColumns implements LogicalPlan.<2nd> interface.
// go: BaseLogicalPlan.PruneColumns L1 [M-SELF, M-PLAN]
pub fn prune_columns<P: LogicalPlan + ?Sized>(
    p: &mut P,
    ctx: &mut PlanCtx<'_>,
    this: PlanId,
    parent_used_cols: Vec<Arc<tidb_expression::Column>>,
    opt: &LogicalOptimizeOp,
) -> Result<PlanId, Error> {
    if p.children().is_empty() {
        return Ok(this);
    }
    let child = p.children()[0];
    p.children_mut()[0] = base::prune_columns(ctx, child, parent_used_cols, opt)?;
    Ok(this)
}

/// BuildKeyInfo implements LogicalPlan.<4th> interface: the `maxOneRow` part (the operators
/// build their schema keys themselves). The child flags are read from the arena.
// go: BaseLogicalPlan.BuildKeyInfo L1 [M-SELF, M-PLAN]
pub fn build_key_info<P: LogicalPlan + ?Sized>(p: &mut P, arena: &PlanArena) {
    let child_max_one_row: Vec<bool> = p
        .children()
        .iter()
        .map(|&c| arena.get(c).max_one_row())
        .collect();
    let v = has_max_one_row(p.kind(), &child_max_one_row);
    p.base_mut().max_one_row = v;
}

/// PushDownTopN implements the LogicalPlan.<5th> interface.
// go: BaseLogicalPlan.PushDownTopN L1 [M-SELF]
pub fn push_down_top_n<P: LogicalPlan + ?Sized>(
    p: &mut P,
    ctx: &mut PlanCtx<'_>,
    this: PlanId,
    top_n: Option<PlanId>,
    opt: &LogicalOptimizeOp,
) -> PlanId {
    push_down_top_n_for_base_logical_plan(p, ctx, this, top_n, opt)
}

/// DeriveTopN implements the LogicalPlan.<6th> interface.
// go: BaseLogicalPlan.DeriveTopN L1 [M-SELF]
pub fn derive_top_n<P: LogicalPlan + ?Sized>(
    p: &mut P,
    ctx: &mut PlanCtx<'_>,
    this: PlanId,
    opt: &LogicalOptimizeOp,
) -> Result<PlanId, Error> {
    if ctx.vars.allow_derive_top_n {
        for i in 0..p.children().len() {
            let child = p.children()[i];
            let new_child = base::derive_top_n(ctx, child, opt)?;
            p.set_child(i, new_child);
        }
    }
    Ok(this)
}

/// PredicateSimplification implements the LogicalPlan.<7th> interface.
// go: BaseLogicalPlan.PredicateSimplification L1 [M-SELF]
pub fn predicate_simplification<P: LogicalPlan + ?Sized>(
    p: &mut P,
    ctx: &mut PlanCtx<'_>,
    this: PlanId,
    opt: &LogicalOptimizeOp,
) -> Result<PlanId, Error> {
    for i in 0..p.children().len() {
        let child = p.children()[i];
        let new_child = base::predicate_simplification(ctx, child, opt)?;
        p.set_child(i, new_child);
    }
    Ok(this)
}

/// ConstantPropagation implements the LogicalPlan.<8th> interface.
// go: BaseLogicalPlan.ConstantPropagation L1
pub fn constant_propagation() -> Option<PlanId> {
    // Only LogicalJoin can apply constant propagation
    // Other Logical plan do nothing
    None
}

/// PullUpConstantPredicates implements the LogicalPlan.<9th> interface.
// go: BaseLogicalPlan.PullUpConstantPredicates L1
pub fn pull_up_constant_predicates() -> Vec<ExprRef> {
    // Only LogicalProjection and LogicalSelection can get constant predicates
    // Other Logical plan return nil
    Vec::new()
}

/// ExhaustPhysicalPlans implements LogicalPlan.<14th> interface.
// go: BaseLogicalPlan.ExhaustPhysicalPlans L1
pub fn exhaust_physical_plans() -> ! {
    panic!("baseLogicalPlan.ExhaustPhysicalPlans() should never be called.");
}

/// ExtractCorrelatedCols implements LogicalPlan.<15th> interface.
// go: BaseLogicalPlan.ExtractCorrelatedCols L1
pub fn extract_correlated_cols() -> Vec<Arc<CorrelatedColumn>> {
    Vec::new()
}

/// CanPushToCop implements LogicalPlan.<21st> interface.
/// it checks if it can be pushed to some stores. For TiKV, it only checks datasource.
/// For TiFlash, it will check whether the operator is supported, but note that the check
/// might be inaccurate.
// go: BaseLogicalPlan.CanPushToCop L1 [M-SELF]
pub fn can_push_to_cop<P: LogicalPlan + ?Sized>(
    p: &P,
    ctx: &PlanCtx<'_>,
    store_tp: tidb_base::kv::StoreType,
) -> bool {
    crate::logicalop::logical_plans_misc::can_push_to_cop_impl(p, ctx, store_tp, false)
}

/// ExtractFD implements LogicalPlan.<22nd> interface.
// glue: stub of BaseLogicalPlan.ExtractFD, blocked-by:D09.14 (the functional dependency set, pkg/planner/funcdep, is not ported; used by ONLY_FULL_GROUP_BY checks and the FD-based rules)
pub fn extract_fd() -> Result<(), Error> {
    Err(blocked(
        "D09.14",
        "functional dependencies of a logical plan (ExtractFD)",
    ))
}

// go: BaseLogicalPlan.GetBaseLogicalPlan L1 [M-SELF]
// GetBaseLogicalPlan returns the embedded BaseLogicalPlan: `LogicalPlan::base`.

/// ConvertOuterToInnerJoin implements LogicalPlan.<24th> interface.
// go: BaseLogicalPlan.ConvertOuterToInnerJoin L1 [M-SELF]
pub fn convert_outer_to_inner_join<P: LogicalPlan + ?Sized>(
    p: &mut P,
    ctx: &mut PlanCtx<'_>,
    this: PlanId,
    predicates: Vec<ExprRef>,
) -> PlanId {
    for i in 0..p.children().len() {
        let child = p.children()[i];
        let new_child = base::convert_outer_to_inner_join(ctx, child, predicates.clone());
        p.set_child(i, new_child);
    }
    this
}

// glue: stub of BaseLogicalPlan.GetLogicalTS4TaskMap, blocked-by:D10.2 (taskMap is the FindBestTask memo, a side table keyed by PlanId, N1 §3.2)
// glue: stub of BaseLogicalPlan.GetTask, blocked-by:D10.2 (taskMap is the FindBestTask memo, a side table keyed by PlanId, N1 §3.2)
// glue: stub of BaseLogicalPlan.StoreTask, blocked-by:D10.2 (taskMap is the FindBestTask memo, a side table keyed by PlanId, N1 §3.2)
/// The memo of `FindBestTask` (`GetLogicalTS4TaskMap`, `GetTask`, `StoreTask`) comes with
/// physical optimization (D10.2).
pub fn task_map() -> Result<(), Error> {
    Err(blocked(
        "D10.2",
        "the FindBestTask memo of a logical plan (taskMap)",
    ))
}

// go: BaseLogicalPlan.Self L1 [M-SELF]
// go: BaseLogicalPlan.SetSelf L1 [M-SELF]
// Self / SetSelf: the back-pointer is gone (M-SELF); the node's own arena id is the `this`
// argument of every method that needs it.

// glue: stub of BaseLogicalPlan.SetFDs, blocked-by:D09.14 (pkg/planner/funcdep is not ported)
// glue: stub of BaseLogicalPlan.FDs, blocked-by:D09.14 (pkg/planner/funcdep is not ported)
/// `SetFDs` / `FDs` store the functional dependency set (`pkg/planner/funcdep`).
pub fn fds() -> Result<(), Error> {
    Err(blocked(
        "D09.14",
        "functional dependencies of a logical plan (FDs)",
    ))
}

/// NewBaseLogicalPlan is the basic constructor of BaseLogicalPlan.
// go: NewBaseLogicalPlan L1 [M-SELF, M-CTX]
pub fn new_base_logical_plan(
    ctx: &PlanCtx<'_>,
    tp: &'static str,
    qb_offset: isize,
) -> BaseLogicalPlan {
    BaseLogicalPlan {
        plan: baseimpl::new_base_plan(ctx, tp, qb_offset),
        max_one_row: false,
        children: Vec::new(),
    }
}
