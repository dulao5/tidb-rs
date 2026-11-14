//! Go package `pkg/planner/core/operator/physicalop`: the `Arc` tree of N1 §3.4–§3.5.
//!
//! Physical plans are `Arc<dyn PhysicalPlan>` from the start. While the statement optimizes
//! them every node has a single owner and [`make_mut_plan`] changes it in place
//! (`Arc::get_mut`), line by line like Go's in-place `postOptimize` passes and
//! `ResolveIndices`; once the plan is shared (plan cache, `taskMap` candidates, executor
//! workers) the same call copies the node first (copy-on-write of one node, children stay
//! shared). Plan-cache range rebuild uses the same two functions (path copying, N1 §3.5).
//!
//! Precondition of the in-place path (N1 §3.4): `FindBestTask`'s memo is released before
//! `postOptimize`, and no `Weak` points into the tree; otherwise every node looks shared and
//! is silently copied. D10.2 (VS1-26 / VS1-28) checks this with
//! `debug_assert!(Arc::strong_count(..) == 1)` at the entry of `postOptimize`.
//!
//! `BasePhysicalPlan` is VS1-19; the operators are ported by VS1-27 (D10.1).

use std::sync::Arc;

use crate::base::PhysicalPlan;

pub mod base_physical_plan;

pub use base_physical_plan::{BasePhysicalPlan, new_base_physical_plan};

/// A physical plan node, shared.
pub type PhysicalPlanRef = Arc<dyn PhysicalPlan>;

/// The node for writing (`M-PLAN`): in place when `p` is its only owner, otherwise a
/// shallow copy ([`PhysicalPlan::clone_node`]) replaces `p` first.
// glue: N1 §3.4/§3.5
pub fn make_mut_plan(p: &mut PhysicalPlanRef) -> &mut dyn PhysicalPlan {
    if Arc::get_mut(p).is_none() {
        *p = p.clone_node();
    }
    Arc::get_mut(p).expect("unique after the copy")
}

/// Go's `for i, child := range p.Children() { p.Children()[i] = f(child) }` on an `Arc` tree
/// (`M-PLAN`). When `p` is owned alone its children are moved out and back, so `f` sees them
/// with their own reference counts (a naive `child.clone()` would make every child look
/// shared and force a copy, pilot finding B5); when `p` is shared it is copied only if a
/// child actually changes.
// glue: N1 §3.4
pub fn map_children(
    p: &mut PhysicalPlanRef,
    mut f: impl FnMut(PhysicalPlanRef) -> PhysicalPlanRef,
) {
    if let Some(node) = Arc::get_mut(p) {
        let children = std::mem::take(node.children_mut());
        *node.children_mut() = children.into_iter().map(f).collect();
        return;
    }
    for i in 0..p.children().len() {
        let new_child = f(p.children()[i].clone());
        if !Arc::ptr_eq(&new_child, &p.children()[i]) {
            make_mut_plan(p).children_mut()[i] = new_child;
        }
    }
}

/// The per-type methods of [`crate::PhysicalPlan`] for an operator that is `Clone` and
/// embeds a [`BasePhysicalPlan`] in the field `base` (`M-IFACE`, `M-SELF`).
///
/// ```ignore
/// impl PhysicalPlan for PhysicalSelection {
///     physical_plan_boilerplate!(Selection);
/// }
/// ```
#[macro_export]
macro_rules! physical_plan_boilerplate {
    ($kind:ident) => {
        // glue: M-IFACE (generated per operator)
        fn kind(&self) -> $crate::PhysicalKind {
            $crate::PhysicalKind::$kind
        }
        // glue: M-IFACE (generated per operator)
        fn as_any(&self) -> &dyn ::std::any::Any {
            self
        }
        // glue: M-IFACE (generated per operator)
        fn as_any_mut(&mut self) -> &mut dyn ::std::any::Any {
            self
        }
        // glue: M-SELF (generated per operator)
        fn as_dyn(&self) -> &dyn $crate::PhysicalPlan {
            self
        }
        // glue: M-SELF (generated per operator)
        fn base_physical(&self) -> &$crate::physicalop::BasePhysicalPlan {
            &self.base
        }
        // glue: M-SELF (generated per operator)
        fn base_physical_mut(&mut self) -> &mut $crate::physicalop::BasePhysicalPlan {
            &mut self.base
        }
        // glue: M-IFACE (generated per operator)
        fn clone_node(&self) -> $crate::PhysicalPlanRef {
            ::std::sync::Arc::new(self.clone())
        }
    };
}
