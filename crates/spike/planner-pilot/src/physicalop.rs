//! go-package: pkg/planner/core (physical plans; spike subset for N1 §3.4–§3.5)
//!
//! Physical plans are `Arc<dyn PhysicalPlan>` from the start (pilot proposal P2): while the
//! statement optimizes them every node has a single owner and [`make_mut_plan`] changes it
//! in place (`Arc::get_mut`), which is the "mutable owned tree" of N1 §3.4 without a
//! separate `Box` representation and without a freezing pass; once the plan is shared (plan
//! cache, executor workers) the same call copies the node (path copying, N1 §3.5).
//! The operators carry only the fields the pilot needs.

use std::any::Any;
use std::sync::Arc;

use tidb_expression::{ExprRef, Schema};
use tidb_model::TableInfo;

/// A physical plan node, shared.
pub type PhysicalPlanRef = Arc<dyn PhysicalPlan>;

/// Operator kinds (`M-IFACE`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhysicalKind {
    /// `*PhysicalTableScan`.
    TableScan,
    /// `*PhysicalTableReader`.
    TableReader,
    /// `*PhysicalSelection`.
    Selection,
    /// `*PhysicalProjection`.
    Projection,
}

/// PhysicalPlan is a tree of the physical operators.
pub trait PhysicalPlan: Any + Send + Sync {
    /// The operator kind.
    // glue: N1
    fn kind(&self) -> PhysicalKind;
    /// Go's plan id.
    // glue: N1
    fn id(&self) -> i32;
    /// For type assertions.
    // glue: N1
    fn as_any(&self) -> &dyn Any;
    /// For type assertions.
    // glue: N1
    fn as_any_mut(&mut self) -> &mut dyn Any;
    /// Schema.
    // glue: N1
    fn schema(&self) -> &Schema;
    /// SetSchema.
    // glue: N1
    fn set_schema(&mut self, schema: Schema);
    /// Children.
    // glue: N1
    fn children(&self) -> &[PhysicalPlanRef];
    /// `p.Children()[i] = ...`.
    // glue: N1
    fn children_mut(&mut self) -> &mut Vec<PhysicalPlanRef>;
    /// A shallow copy of this node: fields copied, children and expressions shared. This
    /// replaces Go's generated `CloneForPlanCache` (a deep copy) for path copying.
    // glue: N1 §3.5
    fn clone_node(&self) -> PhysicalPlanRef;
}

/// The node for writing: in place when `p` is its only owner, otherwise a shallow copy
/// replaces `p` first (copy-on-write of one node; children stay shared).
// glue: N1 §3.4/§3.5
pub fn make_mut_plan(p: &mut PhysicalPlanRef) -> &mut dyn PhysicalPlan {
    if Arc::get_mut(p).is_none() {
        *p = p.clone_node();
    }
    Arc::get_mut(p).expect("unique after the copy")
}

/// Go's `for i, child := range p.Children() { p.Children()[i] = f(child) }` on an `Arc` tree.
/// When `p` is owned alone its children are moved out and back, so `f` sees them with
/// their own reference counts (a naive `child.clone()` would make every child look shared
/// and force a copy, pilot finding B5); when `p` is shared it is copied only if a child
/// actually changes.
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

macro_rules! physical_plan_boilerplate {
    ($kind:ident) => {
        fn kind(&self) -> PhysicalKind {
            PhysicalKind::$kind
        }
        fn id(&self) -> i32 {
            self.id
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
        fn as_any_mut(&mut self) -> &mut dyn Any {
            self
        }
        fn schema(&self) -> &Schema {
            &self.schema
        }
        fn set_schema(&mut self, schema: Schema) {
            self.schema = schema;
        }
        fn children(&self) -> &[PhysicalPlanRef] {
            &self.children
        }
        fn children_mut(&mut self) -> &mut Vec<PhysicalPlanRef> {
            &mut self.children
        }
        fn clone_node(&self) -> PhysicalPlanRef {
            Arc::new(self.clone())
        }
    };
}

/// A key range of a table scan (blocked-by:D10.4, `ranger.Range`): `[low, high]` on an int
/// handle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Range {
    /// LowVal.
    pub low: i64,
    /// HighVal.
    pub high: i64,
}

/// PhysicalTableScan represents a table scan plan.
#[derive(Clone)]
pub struct PhysicalTableScan {
    /// Plan id.
    pub id: i32,
    /// Schema.
    pub schema: Schema,
    /// Always empty.
    pub children: Vec<PhysicalPlanRef>,
    /// AccessCondition is used to calculate range.
    pub access_condition: Vec<ExprRef>,
    /// Table.
    pub table: Arc<TableInfo>,
    /// Ranges.
    pub ranges: Vec<Range>,
}

/// PhysicalTableReader is the table reader in tidb.
#[derive(Clone)]
pub struct PhysicalTableReader {
    /// Plan id.
    pub id: i32,
    /// Schema.
    pub schema: Schema,
    /// Always empty: the cop plan is `table_plans`, not a child (as in Go).
    pub children: Vec<PhysicalPlanRef>,
    /// TablePlans flats the tablePlan to construct executor pb (`[0]` is the top).
    pub table_plans: Vec<PhysicalPlanRef>,
}

/// PhysicalSelection represents a filter.
#[derive(Clone)]
pub struct PhysicalSelection {
    /// Plan id.
    pub id: i32,
    /// Schema.
    pub schema: Schema,
    /// Children.
    pub children: Vec<PhysicalPlanRef>,
    /// Conditions.
    pub conditions: Vec<ExprRef>,
}

/// PhysicalProjection is the physical operator of projection.
#[derive(Clone)]
pub struct PhysicalProjection {
    /// Plan id.
    pub id: i32,
    /// Schema.
    pub schema: Schema,
    /// Children.
    pub children: Vec<PhysicalPlanRef>,
    /// Exprs.
    pub exprs: Vec<ExprRef>,
    /// CalculateNoDelay.
    pub calculate_no_delay: bool,
}

impl PhysicalPlan for PhysicalTableScan {
    physical_plan_boilerplate!(TableScan);
}
impl PhysicalPlan for PhysicalTableReader {
    physical_plan_boilerplate!(TableReader);
}
impl PhysicalPlan for PhysicalSelection {
    physical_plan_boilerplate!(Selection);
}
impl PhysicalPlan for PhysicalProjection {
    physical_plan_boilerplate!(Projection);
}
