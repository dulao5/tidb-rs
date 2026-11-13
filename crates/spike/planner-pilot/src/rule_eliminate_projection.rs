//! go-file: pkg/planner/core/rule_eliminate_projection.go
//!
//! Unlike `PruneColumns` (a method of each operator), `eliminate` is a free function that
//! walks the tree and touches a node and its child at the same time. That is the shape
//! where the arena costs the most lines; see the comments marked `N1:`.

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::mysql::NotNullFlag;
use tidb_expression::constant_fold::fold_constant;
use tidb_expression::util::exprs_has_side_effects;
use tidb_expression::{Expr, ExprRef, Schema};

use crate::arena::{PlanId, with_detached};
use crate::base::{LogicalKind, LogicalOptRule, LogicalPlan, ReplaceMap};
use crate::logicalop::logical_join::build_logical_join_schema;
use crate::logicalop::{LogicalJoin, LogicalProjection};
use crate::optimizetrace::LogicalOptimizeOp;
use crate::physicalop::{PhysicalKind, PhysicalPlanRef, make_mut_plan, map_children};
use crate::planctx::PlanCtx;
use crate::rule_util::resolve_column_and_replace;

/// canProjectionBeEliminatedLoose checks whether a projection can be eliminated,
/// returns true if every expression is a single column.
// go: pkg/planner/core/rule_eliminate_projection.go canProjectionBeEliminatedLoose L1
pub fn can_projection_be_eliminated_loose(p: &LogicalProjection) -> bool {
    // project for expand will assign a new col id for col ref, because these column should be
    // data cloned in the execution time and may be filled with null value at the same time.
    // so it's not a REAL column reference. Detect the column ref in projection here and do
    // the elimination here will restore the Expand's grouping sets column back to use the
    // original column ref again. (which is not right)
    if p.proj4_expand {
        return false;
    }
    for expr in &p.exprs {
        if !matches!(**expr, Expr::Column(_)) {
            return false;
        }
    }
    true
}

/// canProjectionBeEliminatedStrict checks whether a projection can be
/// eliminated, returns true if the projection just copy its child's output.
// go: pkg/planner/core/rule_eliminate_projection.go canProjectionBeEliminatedStrict L1 [M-IFACE]
pub fn can_projection_be_eliminated_strict(p: &crate::physicalop::PhysicalProjection) -> bool {
    // (the MPP final-agg check on PhysicalHashAgg / PhysicalStreamAgg children is
    // blocked-by:D10.3; the pilot has no physical aggregation)
    // If this projection is specially added for `DO`, we keep it.
    if p.calculate_no_delay {
        return false;
    }
    if p.schema.is_empty() {
        return true;
    }
    let child = &p.children[0];
    if p.schema.len() != child.schema().len() {
        return false;
    }
    for (i, expr) in p.exprs.iter().enumerate() {
        match &**expr {
            Expr::Column(col) if col.equal_col(&child.schema().columns[i]) => {}
            _ => return false,
        }
    }
    true
}

/// doPhysicalProjectionElimination. During `postOptimize` the physical tree is still
/// private to the statement: every `Arc` has one owner, so [`make_mut_plan`] changes the
/// node in place (N1 §3.4, pilot proposal P2: no separate `Box` phase).
// go: pkg/planner/core/rule_eliminate_projection.go doPhysicalProjectionElimination L1 [M-PLAN, M-IFACE]
pub fn do_physical_projection_elimination(mut p: PhysicalPlanRef) -> PhysicalPlanRef {
    map_children(&mut p, do_physical_projection_elimination);

    // eliminate projection in a coprocessor task: TiFlash readers only (DEF.MPP), not in the pilot.

    let Some(proj) = p
        .as_any()
        .downcast_ref::<crate::physicalop::PhysicalProjection>()
    else {
        return p;
    };
    if !can_projection_be_eliminated_strict(proj) {
        return p;
    }
    let mut child = proj.children[0].clone();
    if child.kind() == PhysicalKind::Projection {
        // when current projection is an empty projection(schema pruned by column pruner), no need to reset child's schema
        // TODO: avoid producing empty projection in column pruner.
        if !proj.schema.is_empty() {
            // Go shares the schema pointer (`childProj.SetSchema(p.Schema())`); the parent is
            // dropped right after, so moving a copy is the same.
            let schema = proj.schema.clone();
            make_mut_plan(&mut child).set_schema(schema);
        }
    }
    // (the `ColRefFromUpdatePlan` check is for UPDATE; the pilot is read-only)
    child
}

/// eliminatePhysicalProjection should be called after physical optimization to
/// eliminate the redundant projection left after logical projection elimination.
// go: pkg/planner/core/rule_eliminate_projection.go eliminatePhysicalProjection L1
pub fn eliminate_physical_projection(p: PhysicalPlanRef) -> PhysicalPlanRef {
    do_physical_projection_elimination(p)
}

/// ProjectionEliminator is for update stmt
/// The projection eliminate in logical optimize has been forbidden.
/// The projection eliminate in post optimize will optimize the projection under the projection, window, agg (the condition is same as logical optimize)
#[derive(Clone, Copy, Debug, Default)]
pub struct ProjectionEliminator;

impl LogicalOptRule for ProjectionEliminator {
    /// Optimize implements the logicalOptRule interface.
    // go: pkg/planner/core/rule_eliminate_projection.go (*ProjectionEliminator).Optimize L1 [M-PLAN]
    fn optimize(
        &self,
        ctx: &mut PlanCtx<'_>,
        lp: PlanId,
        opt: &mut LogicalOptimizeOp,
    ) -> Result<(PlanId, bool), Error> {
        let plan_changed = false;
        let root = self.eliminate(ctx, lp, &mut ReplaceMap::new(), false, opt);
        Ok((root, plan_changed))
    }

    /// Name implements the logicalOptRule.<1st> interface.
    // go: pkg/planner/core/rule_eliminate_projection.go (*ProjectionEliminator).Name L1
    fn name(&self) -> &'static str {
        "projection_eliminate"
    }
}

impl ProjectionEliminator {
    /// eliminate eliminates the redundant projection in a logical plan.
    // go: pkg/planner/core/rule_eliminate_projection.go (*ProjectionEliminator).eliminate L1 [M-PLAN, M-IFACE, M-EXPR]
    pub fn eliminate(
        &self,
        ctx: &mut PlanCtx<'_>,
        p: PlanId,
        replace: &mut ReplaceMap,
        can_eliminate: bool,
        opt: &mut LogicalOptimizeOp,
    ) -> PlanId {
        // LogicalCTE's logical optimization is independent. (no CTE in the pilot)
        let kind = ctx.arena.get(p).kind();
        let is_proj = kind == LogicalKind::Projection;
        let mut child_flag = can_eliminate;
        // (LogicalUnionAll → false, LogicalWindow → true: not in the pilot)
        if kind == LogicalKind::Aggregation || is_proj {
            child_flag = true;
        }
        // N1: the child list is read by index because `ctx` is lent to the recursive call.
        for i in 0..ctx.arena.get(p).children().len() {
            let child = ctx.arena.get(p).children()[i];
            let new_child = self.eliminate(ctx, child, replace, child_flag, opt);
            ctx.arena.get_mut(p).children_mut()[i] = new_child;
        }

        // replace logical plan schema
        match kind {
            LogicalKind::Join => {
                // N1: compute with `&arena`, then assign with `&mut arena`.
                let join = ctx.arena.downcast_ref::<LogicalJoin>(p).expect("join");
                let schema = build_logical_join_schema(join.join_type, join, &ctx.arena);
                ctx.arena
                    .downcast_mut::<LogicalJoin>(p)
                    .expect("join")
                    .sp
                    .set_schema(Some(schema));
            }
            _ => {
                let schema = ctx.arena.schema_mut(p);
                for dst in schema.columns.iter_mut() {
                    *dst = resolve_column_and_replace(dst, replace);
                }
            }
        }

        ctx.arena.get_mut(p).replace_expr_columns(replace);

        // eliminate duplicate projection: projection with child projection
        if is_proj {
            let child = ctx.arena.get(p).children()[0];
            if let Some(child_proj) = ctx.arena.downcast_ref::<LogicalProjection>(child)
                && !exprs_has_side_effects(&child_proj.exprs)
            {
                // N1: `proj` (mutable) and `child` (read) are two arena slots at once, so
                // `proj` is detached for the loop.
                with_detached(ctx, p, |node, ctx| {
                    let proj = node
                        .as_any_mut()
                        .downcast_mut::<LogicalProjection>()
                        .expect("proj");
                    let child = ctx
                        .arena
                        .downcast_ref::<LogicalProjection>(child)
                        .expect("child proj");
                    let ectx = ctx.eval_ctx();
                    for i in 0..proj.exprs.len() {
                        replace_column_of_expr(&mut proj.exprs[i], child, child.schema(&ctx.arena));
                        let mut folded_expr = fold_constant(&*ctx.expr_ctx, proj.exprs[i].clone());
                        // the folded expr should have the same null flag with the original expr, especially for the projection under union, so forcing it here.
                        // Audit item E3 (pilot report): Go sets the flag on the folded node in
                        // place, which may be a `*Column` shared with the child's schema; here
                        // the node is copied if shared.
                        let flag = (folded_expr.get_type(&*ectx).get_flag() & !NotNullFlag)
                            | (proj.exprs[i].get_type(&*ectx).get_flag() & NotNullFlag);
                        set_ret_type_flag(&mut folded_expr, flag);
                        proj.exprs[i] = folded_expr;
                    }
                    proj.children_mut()[0] = child.children()[0];
                    append_dup_proj_eliminate_trace_step(proj, child, opt);
                });
            }
        }

        let eliminable = is_proj && can_eliminate && {
            let proj = ctx
                .arena
                .downcast_ref::<LogicalProjection>(p)
                .expect("proj");
            can_projection_be_eliminated_loose(proj)
        };
        if !eliminable {
            return p;
        }
        let proj = ctx
            .arena
            .downcast_ref::<LogicalProjection>(p)
            .expect("proj");
        let exprs = &proj.exprs;
        for (i, col) in proj.schema(&ctx.arena).columns.iter().enumerate() {
            let Expr::Column(c) = &*exprs[i] else {
                unreachable!("canProjectionBeEliminatedLoose")
            };
            // N1 cost: Go stores the same `*Column` as the expression; the Rust expression
            // holds the column inline, so it is copied into an `Arc<Column>`.
            replace.insert(col.hash_code().to_vec(), Arc::new(c.clone()));
        }
        append_proj_eliminate_trace_step(proj, opt);
        proj.children()[0]
    }
}

/// `foldedExpr.GetType(ctx).SetFlag(flag)` (copy-on-write when the node is shared).
// glue: M-EXPR (Go mutates the shared FieldType in place)
fn set_ret_type_flag(e: &mut ExprRef, flag: usize) {
    if e.get_static_type().get_flag() == flag {
        return;
    }
    match Arc::make_mut(e) {
        Expr::Column(c) => c.ret_type.set_flag(flag),
        Expr::CorrelatedColumn(c) => c.column.ret_type.set_flag(flag),
        Expr::Constant(c) => c.ret_type.set_flag(flag),
        Expr::ScalarFunction(f) => f.ret_type_mut().set_flag(flag),
    }
}

/// ReplaceColumnOfExpr replaces column of expression by another LogicalProjection. Go
/// returns the (possibly in-place modified) expression; here the slot is updated.
// go: pkg/planner/core/rule_eliminate_projection.go ReplaceColumnOfExpr L1 [M-EXPR]
pub fn replace_column_of_expr(expr: &mut ExprRef, proj: &LogicalProjection, schema: &Schema) {
    match &**expr {
        Expr::Column(v) => {
            let idx = schema.column_index(v);
            if idx != -1 && (idx as usize) < proj.exprs.len() {
                *expr = proj.exprs[idx as usize].clone();
            }
        }
        Expr::ScalarFunction(v) => {
            // Go: v.GetArgs()[i] = ReplaceColumnOfExpr(...) in place; copy-on-write here,
            // and only when an argument changes.
            let mut new_args = v.get_args().to_vec();
            let mut changed = false;
            for arg in new_args.iter_mut() {
                let before = arg.clone();
                replace_column_of_expr(arg, proj, schema);
                changed |= !Arc::ptr_eq(&before, arg);
            }
            if changed && let Expr::ScalarFunction(f) = Arc::make_mut(expr) {
                f.function.args = new_args;
            }
        }
        _ => {}
    }
}

/// appendDupProjEliminateTraceStep (the action text lists the merged expressions in Go).
// glue: stub, blocked-by:D09.13
fn append_dup_proj_eliminate_trace_step(
    parent: &LogicalProjection,
    child: &LogicalProjection,
    opt: &mut LogicalOptimizeOp,
) {
    let (pid, ptp, cid, ctp) = (parent.id(), parent.tp(), child.id(), child.tp());
    opt.append_step_to_current(
        cid,
        ctp,
        || format!("{ptp}_{pid}'s child {ctp}_{cid} is redundant"),
        || format!("{ctp}_{cid} is eliminated"),
    );
}

/// appendProjEliminateTraceStep.
// go: pkg/planner/core/rule_eliminate_projection.go appendProjEliminateTraceStep L1 [M-PLAN]
fn append_proj_eliminate_trace_step(proj: &LogicalProjection, opt: &mut LogicalOptimizeOp) {
    let (id, tp) = (proj.id(), proj.tp());
    opt.append_step_to_current(
        id,
        tp,
        || format!("{tp}_{id}'s Exprs are all Columns"),
        || format!("{tp}_{id} is eliminated"),
    );
}
