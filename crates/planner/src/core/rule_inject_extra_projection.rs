//! go-file: pkg/planner/core/rule_inject_extra_projection.go
//!
//! `InjectExtraProjection` of `postOptimize` (block D09.13): projections below Sort / TopN
//! (and the conversion of a NominalSort) so that the scalar functions of the ORDER BY items
//! are evaluated once, eagerly.
//!
//! Mapping (N1 §3.4): the plan is an `Arc` tree. Go rewrites the operators in place
//! (`plan.Children()[i] = ...`, `item.Expr = newArg`); here [`map_children`] and
//! [`make_mut_plan`] do the same while the statement owns the tree alone, and copy one node
//! when it is shared. The aggregation and union arms (`PhysicalHashAgg`,
//! `PhysicalStreamAgg`, `PhysicalUnionAll`) need operators that are not ported (D10.1 /
//! D09.4 / D09.5): such a plan cannot be built yet, and their functions are boundary stubs.

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_base::kv;
use tidb_expression::{Column, Expr, ExprRef, new_schema};

use crate::base::{PhysicalKind, PhysicalPlan};
use crate::core::optimizer::rewrite_table_plan;
use crate::core::physical_plans::{
    NominalSort, PhysicalProjection, PhysicalSort, PhysicalTableReader, PhysicalTopN,
};
use crate::core::resolve_indices::refine4_neighbour_proj;
use crate::physicalop::{PhysicalPlanRef, make_mut_plan, map_children};
use crate::planctx::PlanCtx;
use crate::util::ByItems;

/// InjectExtraProjection is used to extract the expressions of specific
/// operators into a physical Projection operator and inject the Projection below
/// the operators. Thus we can accelerate the expression evaluation by eager
/// evaluation.
/// This function will be called in two situations:
/// 1. In postOptimize.
/// 2. TiDB can be used as a coprocessor, when a plan tree been pushed down to
///    TiDB, we need to inject extra projections for the plan tree as well.
///
/// The failpoint `DisableProjectionPostOptimization` is not ported (no failpoints).
// go: InjectExtraProjection L1 [M-CTX]
pub fn inject_extra_projection(ctx: &PlanCtx<'_>, plan: PhysicalPlanRef) -> PhysicalPlanRef {
    new_proj_injector().inject(ctx, plan)
}

/// projInjector.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProjInjector;

/// NewProjInjector builds a projInjector.
// go: NewProjInjector L1
pub fn new_proj_injector() -> ProjInjector {
    ProjInjector
}

impl ProjInjector {
    /// `inject`. The aggregation and union arms are not reachable: those operators are not
    /// ported, so no plan contains them (see the module doc).
    // go: projInjector.inject L2 (the HashAgg / StreamAgg / UnionAll arms come with those operators)
    pub fn inject(&self, ctx: &PlanCtx<'_>, mut plan: PhysicalPlanRef) -> PhysicalPlanRef {
        map_children(&mut plan, |child| self.inject(ctx, child));

        if plan.kind() == PhysicalKind::TableReader {
            let tr = plan
                .as_any()
                .downcast_ref::<PhysicalTableReader>()
                .expect("a PhysicalTableReader");
            if tr.store_type == kv::TiFlash {
                rewrite_table_plan(&mut plan, |table_plan| Ok(self.inject(ctx, table_plan)))
                    .expect("injecting projections does not fail");
            }
        }

        match plan.kind() {
            // case *PhysicalHashAgg / *PhysicalStreamAgg: InjectProjBelowAgg (not ported operators).
            PhysicalKind::Sort | PhysicalKind::TopN => inject_proj_below_sort(ctx, plan),
            PhysicalKind::NominalSort => {
                let p = plan
                    .as_any()
                    .downcast_ref::<NominalSort>()
                    .expect("a NominalSort");
                let only_column = p.only_column;
                let by_items = p.by_items.clone();
                turn_nominal_sort_into_proj(ctx, plan, only_column, &by_items)
            }
            // case *PhysicalUnionAll: injectProjBelowUnion (not a ported operator).
            _ => plan,
        }
    }
}

/// `injectProjBelowUnion`: needs `PhysicalUnionAll` (set operations, D09.5).
// glue: stub of injectProjBelowUnion, blocked-by:D09.5 (PhysicalUnionAll is not ported)
pub fn inject_proj_below_union() -> Result<(), Error> {
    Err(blocked("D09.5", "injectProjBelowUnion (PhysicalUnionAll)"))
}

/// `InjectProjBelowAgg`: needs the physical aggregations (D10.1).
// glue: stub of InjectProjBelowAgg, blocked-by:D10.1 (PhysicalHashAgg / PhysicalStreamAgg are not ported)
pub fn inject_proj_below_agg() -> Result<(), Error> {
    Err(blocked(
        "D10.1",
        "InjectProjBelowAgg (the physical aggregations)",
    ))
}

/// The ORDER BY items of a Sort / TopN, for writing (Go passes `p.ByItems` next to `p`).
// glue: the orderByItems argument of InjectProjBelowSort (p.ByItems of PhysicalSort / PhysicalTopN)
fn sort_by_items_mut(p: &mut dyn PhysicalPlan) -> &mut Vec<ByItems> {
    match p.kind() {
        PhysicalKind::Sort => {
            &mut p
                .as_any_mut()
                .downcast_mut::<PhysicalSort>()
                .expect("a PhysicalSort")
                .by_items
        }
        PhysicalKind::TopN => {
            &mut p
                .as_any_mut()
                .downcast_mut::<PhysicalTopN>()
                .expect("a PhysicalTopN")
                .by_items
        }
        k => panic!("{k:?} has no ORDER BY items"),
    }
}

/// The ORDER BY items of a Sort / TopN.
// glue: the orderByItems argument of InjectProjBelowSort (p.ByItems of PhysicalSort / PhysicalTopN)
fn sort_by_items(p: &dyn PhysicalPlan) -> &[ByItems] {
    match p.kind() {
        PhysicalKind::Sort => {
            &p.as_any()
                .downcast_ref::<PhysicalSort>()
                .expect("a PhysicalSort")
                .by_items
        }
        PhysicalKind::TopN => {
            &p.as_any()
                .downcast_ref::<PhysicalTopN>()
                .expect("a PhysicalTopN")
                .by_items
        }
        k => panic!("{k:?} has no ORDER BY items"),
    }
}

/// The child columns of a bottom projection: a copy of each column of the child's schema
/// with its index in that schema (the loop shared by `InjectProjBelowSort` and
/// `TurnNominalSortIntoProj`).
// go-part: InjectProjBelowSort (the bottom projection's child columns, also TurnNominalSortIntoProj) L1
fn bottom_proj_child_columns(child_plan: &dyn PhysicalPlan) -> (Vec<Arc<Column>>, Vec<ExprRef>) {
    let child_schema = child_plan.schema();
    let mut bottom_proj_schema_cols = Vec::with_capacity(child_schema.columns.len());
    let mut bottom_proj_exprs = Vec::with_capacity(child_schema.columns.len());
    for col in &child_schema.columns {
        let mut new_col = Column::clone(col);
        new_col.index = child_schema.column_index(&new_col);
        bottom_proj_exprs.push(Arc::new(Expr::Column(new_col.clone())));
        bottom_proj_schema_cols.push(Arc::new(new_col));
    }
    (bottom_proj_schema_cols, bottom_proj_exprs)
}

/// Go puts the same `*Column` into the bottom projection's expressions and its schema for
/// the child's columns, so `refine4NeighbourProj`'s writes to the expressions' indices show in
/// the schema too. Here they are two copies: the schema's first `n` columns take the
/// expressions' indices again.
// glue: Go's shared *Column between a bottom projection's expressions and schema (M-EXPR)
fn sync_aliased_schema_columns(bottom_proj: &mut PhysicalProjection, n: usize) {
    let mut schema = bottom_proj.schema().clone();
    for (i, e) in bottom_proj.exprs.iter().take(n).enumerate() {
        if let Expr::Column(c) = &**e
            && schema.columns[i].index != c.index
        {
            Arc::make_mut(&mut schema.columns[i]).index = c.index;
        }
    }
    bottom_proj.set_schema(schema);
}

/// A projection that outputs the columns of `schema` by position (`col.Index = i`).
// go-part: InjectProjBelowSort (the top projection's expressions, also TurnNominalSortIntoProj) L1
fn top_proj_exprs(schema: &tidb_expression::Schema) -> Vec<ExprRef> {
    let mut top_proj_exprs = Vec::with_capacity(schema.columns.len());
    for (i, c) in schema.columns.iter().enumerate() {
        let mut col = Column::clone(c);
        col.index = i as isize;
        top_proj_exprs.push(Arc::new(Expr::Column(col)));
    }
    top_proj_exprs
}

/// The child's required property (`p.GetChildReqProps(0).CloneEssentialFields()`): Go
/// dereferences it, so a plan without one is a bug (a panic, as in Go).
// go-part: InjectProjBelowSort (childProp, also TurnNominalSortIntoProj) L1
fn child_prop(p: &dyn PhysicalPlan) -> crate::property::PhysicalProperty {
    p.get_child_req_props(0)
        .expect("the child's required property")
        .clone_essential_fields()
}

/// InjectProjBelowSort extracts the ScalarFunctions of `orderByItems` into a
/// PhysicalProjection and injects it below PhysicalTopN/PhysicalSort. The schema
/// of PhysicalSort and PhysicalTopN are the same as the schema of their
/// children. When a projection is injected as the child of PhysicalSort and
/// PhysicalTopN, some extra columns will be added into the schema of the
/// Projection, thus we need to add another Projection upon them to prune the
/// redundant columns.
///
/// `orderByItems` is `p.ByItems` (Go's only callers pass it), read and rewritten through `p`.
// go: InjectProjBelowSort L2 (orderByItems is p's own ByItems, see the doc)
pub fn inject_proj_below_sort(ctx: &PlanCtx<'_>, mut p: PhysicalPlanRef) -> PhysicalPlanRef {
    let mut order_by_items = sort_by_items(&*p).to_vec();
    let num_order_by_items = order_by_items.len();
    let mut has_scalar_func = false;
    let mut i = 0;
    while !has_scalar_func && i < num_order_by_items {
        let is_scalar_func = matches!(&*order_by_items[i].expr, Expr::ScalarFunction(_));
        has_scalar_func = has_scalar_func || is_scalar_func;
        i += 1;
    }
    if !has_scalar_func {
        return p;
    }

    let mut top_proj = PhysicalProjection {
        exprs: top_proj_exprs(p.schema()),
        ..Default::default()
    }
    .init(
        ctx,
        p.stats_info().cloned(),
        p.query_block_offset(),
        vec![None],
    );
    top_proj.set_schema(p.schema().clone());

    let child_plan = Arc::clone(&p.children()[0]);
    let (mut bottom_proj_schema_cols, mut bottom_proj_exprs) =
        bottom_proj_child_columns(&*child_plan);

    let ectx = ctx.eval_ctx();
    for item in order_by_items.iter_mut() {
        let item_expr = Arc::clone(&item.expr);
        if !matches!(&*item_expr, Expr::ScalarFunction(_)) {
            continue;
        }
        let new_arg = Column {
            unique_id: ctx.vars.alloc_plan_column_id(),
            ret_type: Arc::new(item_expr.get_type(&*ectx).into_owned()),
            index: bottom_proj_schema_cols.len() as isize,
            ..Default::default()
        };
        bottom_proj_exprs.push(item_expr);
        item.expr = Arc::new(Expr::Column(new_arg.clone()));
        bottom_proj_schema_cols.push(Arc::new(new_arg));
    }

    let child_prop = child_prop(&*p);
    let mut bottom_proj = PhysicalProjection {
        exprs: bottom_proj_exprs,
        ..Default::default()
    }
    .init(
        ctx,
        Some(
            child_plan
                .stats_info()
                .expect("the child's stats")
                .scale_by_expect_cnt(child_prop.expected_cnt),
        ),
        p.query_block_offset(),
        vec![Some(Arc::new(child_prop))],
    );
    bottom_proj.set_schema(new_schema(bottom_proj_schema_cols));
    bottom_proj.set_children(vec![Arc::clone(&child_plan)]);

    if let Some(orig_child_proj) = child_plan.as_any().downcast_ref::<PhysicalProjection>() {
        refine4_neighbour_proj(&mut bottom_proj, orig_child_proj);
        sync_aliased_schema_columns(&mut bottom_proj, child_plan.schema().len());
    }
    refine4_neighbour_proj(&mut top_proj, &bottom_proj);

    {
        let node = make_mut_plan(&mut p);
        *sort_by_items_mut(node) = order_by_items;
        node.set_children(vec![Arc::new(bottom_proj)]);
    }
    top_proj.set_children(vec![p]);
    Arc::new(top_proj)
}

/// TurnNominalSortIntoProj will turn nominal sort into two projections. This is to check if the scalar functions will
/// overflow.
// go: TurnNominalSortIntoProj L1 [M-CTX]
pub fn turn_nominal_sort_into_proj(
    ctx: &PlanCtx<'_>,
    p: PhysicalPlanRef,
    only_column: bool,
    order_by_items: &[ByItems],
) -> PhysicalPlanRef {
    if only_column {
        return Arc::clone(&p.children()[0]);
    }

    let child_plan = Arc::clone(&p.children()[0]);
    let (mut bottom_proj_schema_cols, mut bottom_proj_exprs) =
        bottom_proj_child_columns(&*child_plan);

    let ectx = ctx.eval_ctx();
    for item in order_by_items {
        let item_expr = Arc::clone(&item.expr);
        if !matches!(&*item_expr, Expr::ScalarFunction(_)) {
            continue;
        }
        let new_arg = Column {
            unique_id: ctx.vars.alloc_plan_column_id(),
            ret_type: Arc::new(item_expr.get_type(&*ectx).into_owned()),
            index: bottom_proj_schema_cols.len() as isize,
            ..Default::default()
        };
        bottom_proj_exprs.push(item_expr);
        bottom_proj_schema_cols.push(Arc::new(new_arg));
    }

    let child_prop = child_prop(&*p);
    let child_stats = child_plan
        .stats_info()
        .expect("the child's stats")
        .scale_by_expect_cnt(child_prop.expected_cnt);
    let child_prop = Arc::new(child_prop);
    let mut bottom_proj = PhysicalProjection {
        exprs: bottom_proj_exprs,
        ..Default::default()
    }
    .init(
        ctx,
        Some(Arc::clone(&child_stats)),
        p.query_block_offset(),
        vec![Some(Arc::clone(&child_prop))],
    );
    bottom_proj.set_schema(new_schema(bottom_proj_schema_cols));
    bottom_proj.set_children(vec![Arc::clone(&child_plan)]);

    let mut top_proj = PhysicalProjection {
        exprs: top_proj_exprs(child_plan.schema()),
        ..Default::default()
    }
    .init(
        ctx,
        Some(child_stats),
        p.query_block_offset(),
        vec![Some(child_prop)],
    );
    top_proj.set_schema(child_plan.schema().clone());

    if let Some(orig_child_proj) = child_plan.as_any().downcast_ref::<PhysicalProjection>() {
        refine4_neighbour_proj(&mut bottom_proj, orig_child_proj);
        sync_aliased_schema_columns(&mut bottom_proj, child_plan.schema().len());
    }
    refine4_neighbour_proj(&mut top_proj, &bottom_proj);
    top_proj.set_children(vec![Arc::new(bottom_proj)]);

    Arc::new(top_proj)
}
