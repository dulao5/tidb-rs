//! The per-statement plan arena of N1 §3.1 / §6.1 (`M-PLAN`).
//!
//! glue: N1 (no Go counterpart; Go reaches every node through interface pointers).
//!
//! Nodes are `Box<dyn LogicalPlan>` slots addressed by [`PlanId`]. A slot can be *detached*
//! ([`PlanArena::take`]) while a method of the node runs with `&mut self` and the rest of the
//! arena (pilot finding F1: this is what lets operator methods keep Go's `p.Field = ...` shape
//! without `PlanCtx::split()`).

use tidb_expression::Schema;

use crate::base::LogicalPlan;
use crate::planctx::PlanCtx;

/// Arena index of a logical plan node (not Go's plan id, N1 §3.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PlanId(pub u32);

/// All logical plan nodes of one statement.
#[derive(Default)]
pub struct PlanArena {
    nodes: Vec<Option<Box<dyn LogicalPlan>>>,
}

impl PlanArena {
    /// An empty arena.
    // glue: N1
    pub fn new() -> PlanArena {
        PlanArena::default()
    }

    /// Stores a node and returns its id.
    // glue: N1
    pub fn alloc(&mut self, p: Box<dyn LogicalPlan>) -> PlanId {
        let id = PlanId(u32::try_from(self.nodes.len()).expect("plan arena overflow"));
        self.nodes.push(Some(p));
        id
    }

    /// Number of slots (allocated nodes, including the ones no longer in the tree).
    // glue: N1
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether no node was allocated.
    // glue: N1
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// The node `id`. Panics while the node is detached (a re-entrant visit, which Go's
    /// tree rules never do).
    // glue: N1
    pub fn get(&self, id: PlanId) -> &dyn LogicalPlan {
        self.nodes[id.0 as usize]
            .as_deref()
            .unwrap_or_else(|| panic!("plan {id:?} is detached"))
    }

    /// The node `id`, mutable.
    // glue: N1
    pub fn get_mut(&mut self, id: PlanId) -> &mut dyn LogicalPlan {
        match self.nodes[id.0 as usize].as_deref_mut() {
            Some(p) => p,
            None => panic!("plan {id:?} is detached"),
        }
    }

    /// Go's type assertion `p.(*T)` (`M-IFACE`).
    // glue: N1
    pub fn downcast_ref<T: LogicalPlan>(&self, id: PlanId) -> Option<&T> {
        self.get(id).as_any().downcast_ref::<T>()
    }

    /// Go's type assertion `p.(*T)`, mutable.
    // glue: N1
    pub fn downcast_mut<T: LogicalPlan>(&mut self, id: PlanId) -> Option<&mut T> {
        self.get_mut(id).as_any_mut().downcast_mut::<T>()
    }

    /// Detaches node `id` so that it can run with `&mut self` next to `&mut PlanArena`.
    // glue: N1
    pub fn take(&mut self, id: PlanId) -> Box<dyn LogicalPlan> {
        self.nodes[id.0 as usize]
            .take()
            .unwrap_or_else(|| panic!("plan {id:?} is detached twice"))
    }

    /// Puts a detached node back.
    // glue: N1
    pub fn put(&mut self, id: PlanId, p: Box<dyn LogicalPlan>) {
        let slot = &mut self.nodes[id.0 as usize];
        assert!(slot.is_none(), "plan {id:?} is not detached");
        *slot = Some(p);
    }

    /// `p.Schema()` for writing, for a node that is in the arena. Go's `Schema()` either
    /// returns the node's own schema (`LogicalSchemaProducer`, initialised lazily from the
    /// child) or the child's (`BaseLogicalPlan`); writes through it land in whichever node
    /// owns it, so the owner is found first.
    // glue: N1 (the mutable path of LogicalSchemaProducer.Schema / BaseLogicalPlan.Schema)
    pub fn schema_mut(&mut self, id: PlanId) -> &mut Schema {
        let mut owner = id;
        while self.get(owner).schema_producer().is_none() {
            owner = self.get(owner).children()[0];
        }
        let n = self.get(owner);
        n.schema_producer()
            .expect("owner")
            .schema(n.children(), self);
        self.get_mut(owner)
            .schema_producer_mut()
            .expect("owner")
            .schema_cell_mut()
    }
}

/// Runs `f` on node `id` detached from the arena: `f` gets `&mut` node and `&mut PlanCtx`
/// (arena included) at once. The node is put back afterwards.
// glue: N1 (dispatch of a Go interface method to a node stored in the arena)
pub fn with_detached<R>(
    ctx: &mut PlanCtx<'_>,
    id: PlanId,
    f: impl FnOnce(&mut dyn LogicalPlan, &mut PlanCtx<'_>) -> R,
) -> R {
    let mut node = ctx.arena.take(id);
    let r = f(&mut *node, ctx);
    ctx.arena.put(id, node);
    r
}
