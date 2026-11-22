//! go-file: pkg/planner/core/initialize.go
//!
//! The `Init` functions of the physical operators of the first vertical slice and the plan
//! flattening helpers.
//!
//! - Go's `Init` is a method on a struct literal (`PhysicalSelection{...}.Init(ctx, ...)`)
//!   that fills the embedded `BasePhysicalPlan` and returns a pointer; here it consumes the
//!   literal (`PhysicalSelection { .., ..Default::default() }.init(ctx, ...)`) and returns
//!   the operator, which the caller puts into an `Arc` when it enters the tree. The context
//!   is the planning context argument (`M-CTX`), used for the plan id and the session
//!   variables.
//! - `props ...*property.PhysicalProperty` is a `Vec` of required child properties (Go's
//!   nil entries are `None`).
//! - The other operators' `Init` come with the operators; `PointGetPlan.Init` is the point
//!   get's (D10.8).

// Plan type names and read request types keep their Go names (M-GONAME).
#![allow(non_upper_case_globals)]

use std::collections::HashMap;
use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_base::kv;

use crate::base::PhysicalKind;
use crate::core::physical_plans::{
    BatchCop, Cop, MPP, NominalSort, PhysicalLimit, PhysicalProjection, PhysicalSelection,
    PhysicalSort, PhysicalTableDual, PhysicalTableReader, PhysicalTableScan, PhysicalTopN,
    set_mpp_or_batch_cop_for_table_scan,
};
use crate::physicalop::{PhysicalPlanRef, new_base_physical_plan};
use crate::planctx::PlanCtx;
use crate::property::{PhysicalProperty, StatsInfo};

/// `plancodec.TypeSel`.
const TypeSel: &str = "Selection";
/// `plancodec.TypeProj`.
const TypeProj: &str = "Projection";
/// `plancodec.TypeSort`.
const TypeSort: &str = "Sort";
/// `plancodec.TypeTopN`.
const TypeTopN: &str = "TopN";
/// `plancodec.TypeLimit`.
const TypeLimit: &str = "Limit";
/// `plancodec.TypeDual`.
const TypeDual: &str = "TableDual";
/// `plancodec.TypeTableScan`.
const TypeTableScan: &str = "TableScan";
/// `plancodec.TypeTableReader`.
const TypeTableReader: &str = "TableReader";

impl PhysicalSelection {
    /// Init initializes PhysicalSelection.
    // go: PhysicalSelection.Init L1 [M-SELF, M-CTX]
    pub fn init(
        mut self,
        ctx: &PlanCtx<'_>,
        stats: Option<Arc<StatsInfo>>,
        qb_offset: isize,
        props: Vec<Option<Arc<PhysicalProperty>>>,
    ) -> PhysicalSelection {
        self.base = new_base_physical_plan(ctx, TypeSel, qb_offset);
        self.base.set_children_req_props(props);
        self.base.plan.set_stats(stats);
        self
    }
}

impl PhysicalProjection {
    /// Init initializes PhysicalProjection.
    // go: PhysicalProjection.Init L1 [M-SELF, M-CTX]
    pub fn init(
        mut self,
        ctx: &PlanCtx<'_>,
        stats: Option<Arc<StatsInfo>>,
        offset: isize,
        props: Vec<Option<Arc<PhysicalProperty>>>,
    ) -> PhysicalProjection {
        self.sp.base = new_base_physical_plan(ctx, TypeProj, offset);
        self.sp.base.set_children_req_props(props);
        self.sp.base.plan.set_stats(stats);
        self
    }
}

impl PhysicalSort {
    /// Init initializes PhysicalSort.
    // go: PhysicalSort.Init L1 [M-SELF, M-CTX]
    pub fn init(
        mut self,
        ctx: &PlanCtx<'_>,
        stats: Option<Arc<StatsInfo>>,
        offset: isize,
        props: Vec<Option<Arc<PhysicalProperty>>>,
    ) -> PhysicalSort {
        self.base = new_base_physical_plan(ctx, TypeSort, offset);
        self.base.set_children_req_props(props);
        self.base.plan.set_stats(stats);
        self
    }
}

impl NominalSort {
    /// Init initializes NominalSort.
    // go: NominalSort.Init L1 [M-SELF, M-CTX]
    pub fn init(
        mut self,
        ctx: &PlanCtx<'_>,
        stats: Option<Arc<StatsInfo>>,
        offset: isize,
        props: Vec<Option<Arc<PhysicalProperty>>>,
    ) -> NominalSort {
        self.base = new_base_physical_plan(ctx, TypeSort, offset);
        self.base.set_children_req_props(props);
        self.base.plan.set_stats(stats);
        self
    }
}

impl PhysicalTopN {
    /// Init initializes PhysicalTopN.
    // go: PhysicalTopN.Init L1 [M-SELF, M-CTX]
    pub fn init(
        mut self,
        ctx: &PlanCtx<'_>,
        stats: Option<Arc<StatsInfo>>,
        offset: isize,
        props: Vec<Option<Arc<PhysicalProperty>>>,
    ) -> PhysicalTopN {
        self.base = new_base_physical_plan(ctx, TypeTopN, offset);
        self.base.set_children_req_props(props);
        self.base.plan.set_stats(stats);
        self
    }
}

impl PhysicalLimit {
    /// Init initializes PhysicalLimit.
    // go: PhysicalLimit.Init L1 [M-SELF, M-CTX]
    pub fn init(
        mut self,
        ctx: &PlanCtx<'_>,
        stats: Option<Arc<StatsInfo>>,
        offset: isize,
        props: Vec<Option<Arc<PhysicalProperty>>>,
    ) -> PhysicalLimit {
        self.sp.base = new_base_physical_plan(ctx, TypeLimit, offset);
        self.sp.base.set_children_req_props(props);
        self.sp.base.plan.set_stats(stats);
        self
    }
}

impl PhysicalTableDual {
    /// Init initializes PhysicalTableDual.
    // go: PhysicalTableDual.Init L1 [M-SELF, M-CTX]
    pub fn init(
        mut self,
        ctx: &PlanCtx<'_>,
        stats: Option<Arc<StatsInfo>>,
        offset: isize,
    ) -> PhysicalTableDual {
        self.sp.base = new_base_physical_plan(ctx, TypeDual, offset);
        self.sp.base.plan.set_stats(stats);
        self
    }
}

impl PhysicalTableScan {
    /// Init initializes PhysicalTableScan.
    // go: PhysicalTableScan.Init L1 [M-SELF, M-CTX]
    pub fn init(mut self, ctx: &PlanCtx<'_>, offset: isize) -> PhysicalTableScan {
        self.sp.base = new_base_physical_plan(ctx, TypeTableScan, offset);
        self
    }
}

impl PhysicalTableReader {
    /// `adjustReadReqType`. `PhysicalExchangeSender`, `PhysicalHashAgg` and
    /// `PhysicalStreamAgg` are not ported (MPP and aggregation are outside the first slice),
    /// so no table plan is one of them.
    // go: PhysicalTableReader.adjustReadReqType L1 [M-CTX, M-IFACE]
    fn adjust_read_req_type(&mut self, ctx: &PlanCtx<'_>) {
        if self.store_type == kv::TiFlash {
            // `p.tablePlan.(*PhysicalExchangeSender)`: not ported (see the doc).
            let table_scans = self.get_table_scans();
            // When PhysicalTableReader's store type is tiflash, has table scan
            // and all table scans contained are not keepOrder, try to use batch cop.
            if !table_scans.is_empty() {
                for table_scan in &table_scans {
                    if table_scan.keep_order {
                        return;
                    }
                }

                // When allow batch cop is 1, only agg / topN uses batch cop.
                // When allow batch cop is 2, every query uses batch cop.
                match ctx.vars.allow_batch_cop {
                    1 => {
                        for plan in &self.table_plans {
                            // `case *PhysicalHashAgg, *PhysicalStreamAgg, *PhysicalTopN`.
                            if plan.kind() == PhysicalKind::TopN {
                                self.read_req_type = BatchCop;
                                return;
                            }
                        }
                    }
                    2 => {
                        self.read_req_type = BatchCop;
                    }
                    _ => {}
                }
            }
        }
    }

    /// Init initializes PhysicalTableReader.
    ///
    /// Go points the reader's schema at the table plan's (`p.schema = p.tablePlan.Schema()`);
    /// the reader gets a copy (spec §5, schema sharing). `setMppOrBatchCopForTableScan`
    /// rewrites the scans of the table plan; Go's `TablePlans` hold the same scan pointers,
    /// here the list is flattened again from the rewritten plan.
    // go: PhysicalTableReader.Init L1 [M-SELF, M-CTX, M-PLAN]
    pub fn init(mut self, ctx: &PlanCtx<'_>, offset: isize) -> PhysicalTableReader {
        self.sp.base = new_base_physical_plan(ctx, TypeTableReader, offset);
        self.read_req_type = Cop;
        let Some(table_plan) = &self.table_plan else {
            return self;
        };
        self.table_plans = flatten_list_push_down_plan(table_plan);
        self.sp.set_schema(Some(table_plan.schema().clone_schema()));
        self.adjust_read_req_type(ctx);
        if self.read_req_type == BatchCop || self.read_req_type == MPP {
            self.table_plans.clear();
            let table_plan = self.table_plan.as_mut().expect("table plan");
            set_mpp_or_batch_cop_for_table_scan(table_plan);
            self.table_plans = flatten_list_push_down_plan(table_plan);
        }
        self
    }
}

impl crate::core::point_get_plan::PointGetPlan {
    /// Init initializes PointGetPlan (the point get of FindBestTask; the fast path builds
    /// with `newPointGetPlan`).
    // go: PointGetPlan.Init L1 [M-SELF, M-CTX]
    pub fn init(
        mut self,
        ctx: &PlanCtx<'_>,
        stats: Option<Arc<StatsInfo>>,
        offset: isize,
    ) -> crate::core::point_get_plan::PointGetPlan {
        self.base = crate::physicalop::BasePhysicalPlan::default();
        self.base.plan =
            crate::baseimpl::new_base_plan(ctx, crate::core::point_get_plan::TypePointGet, offset);
        self.base.plan.set_stats(stats);
        let tbl_columns: Vec<Arc<tidb_model::ColumnInfo>> = self
            .tbl_info
            .columns
            .iter()
            .flatten()
            .map(Arc::clone)
            .collect();
        self.columns = crate::core::physical_plans::expand_virtual_column(
            &self.columns,
            &mut self.schema,
            &tbl_columns,
        );
        self
    }
}

/// The `Init` of the batch point get plan.
// glue: stub of BatchPointGetPlan.Init, blocked-by:D10.8 (BatchPointGetPlan)
pub fn init_point_get_plan() -> Result<(), Error> {
    Err(blocked("D10.8", "the Init of a batch point get plan"))
}

/// flattenPlanWithPreorderTraversal flattens a plan tree to a list with preorder traversal.
// go: flattenPlanWithPreorderTraversal L1
fn flatten_plan_with_preorder_traversal(plan: &PhysicalPlanRef, plans: &mut Vec<PhysicalPlanRef>) {
    plans.push(Arc::clone(plan));
    for child in plan.children() {
        flatten_plan_with_preorder_traversal(child, plans);
    }
}

/// flattenListPushDownPlan converts a plan tree to a list, whose head is the leaf node like table scan.
/// It is used to flatten the plan tree that all parents have only one child.
// go: flattenListPushDownPlan L1
pub fn flatten_list_push_down_plan(p: &PhysicalPlanRef) -> Vec<PhysicalPlanRef> {
    let mut plans = Vec::with_capacity(5);
    flatten_plan_with_preorder_traversal(p, &mut plans);
    for i in 0..plans.len() / 2 {
        let j = plans.len() - i - 1;
        plans.swap(i, j);
    }
    plans
}

/// flattenPlanWithPostorderTraversal flattens a plan tree to a list with postorder traversal.
/// The unNatureOrdersMaps the childPlanIndex => parentPlanIndex for those plans whose parent's index
/// is not equal to the child's index + 1.
///
/// Go's nil map is `None`.
// go: flattenPlanWithPostorderTraversal L1 [M-INT]
fn flatten_plan_with_postorder_traversal(
    plan: &PhysicalPlanRef,
    mut plans: Vec<PhysicalPlanRef>,
    mut un_nature_orders_map: Option<HashMap<isize, isize>>,
) -> (Vec<PhysicalPlanRef>, Option<HashMap<isize, isize>>) {
    let mut un_nature_order_children: Vec<isize> = Vec::new();
    let children = plan.children();
    if children.len() > 1 && un_nature_orders_map.is_none() {
        un_nature_orders_map = Some(HashMap::new());
        un_nature_order_children = Vec::with_capacity(children.len());
    }
    for child in children {
        (plans, un_nature_orders_map) =
            flatten_plan_with_postorder_traversal(child, plans, un_nature_orders_map);
        un_nature_order_children.push(plans.len() as isize - 1);
    }
    plans.push(Arc::clone(plan));
    for child_index in un_nature_order_children {
        let parent_index = plans.len() as isize - 1;
        if parent_index != child_index + 1 {
            un_nature_orders_map
                .as_mut()
                .expect("assignment to entry in nil map")
                .insert(child_index, parent_index);
        }
    }
    (plans, un_nature_orders_map)
}

/// flattenTreePushDownPlan converts a plan tree to a list, whose head is the leaf node like table scan.
/// The returned list follows the order of depth-first search with post-order traversal.
/// That is: left child first, then right child, then parent.
/// For example, a DAG:
///
/// ```text
///          A
///         /
///        B
///      /   \
///     C     D
///    /     / \
///   E     F   G
///  /
/// H
/// ```
///
/// Its order should be: [H, E, C, F, G, D, B, A]
/// This function also returns a map which records the childPlanIndex => parentPlanIndex if
/// the child's index + 1 is not equal to the parent's index.
/// This function should NOT be used by TiFlash
// go: flattenTreePushDownPlan L1
pub fn flatten_tree_push_down_plan(
    p: &PhysicalPlanRef,
) -> (Vec<PhysicalPlanRef>, Option<HashMap<isize, isize>>) {
    flatten_plan_with_postorder_traversal(p, Vec::with_capacity(5), None)
}
