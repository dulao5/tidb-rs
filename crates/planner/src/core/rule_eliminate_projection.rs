//! go-file: pkg/planner/core/rule_eliminate_projection.go
//!
//! The logical projection elimination rule (block D09.7). The physical half
//! (`eliminatePhysicalProjection`, run by `postOptimize`) needs the physical projection,
//! table reader and aggregation operators: it is a boundary stub until the optimizer driver
//! (VS1-24) and the physical operators come.

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_base::errors::RedactLogDisable;
use tidb_base::mysql::NotNullFlag;
use tidb_expression::constant_fold::fold_constant;
use tidb_expression::util::exprs_has_side_effects;
use tidb_expression::{Expr, ExprRef, Schema};

use crate::arena::{PlanId, with_detached};
use crate::base::{LogicalKind, LogicalOptRule, LogicalPlan, ReplaceMap};
use crate::core::rule_util::resolve_column_and_replace;
use crate::logicalop::LogicalProjection;
use crate::planctx::PlanCtx;
use crate::util::logicaltrace::{TraceId, trace_id};
use crate::util::optimizetrace::LogicalOptimizeOp;

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
        if !matches!(&**expr, Expr::Column(_)) {
            return false;
        }
    }
    true
}

// glue: stub of canProjectionBeEliminatedStrict, blocked-by:D10.1 (PhysicalProjection / PhysicalHashAgg / PhysicalStreamAgg; postOptimize, VS1-24)
// glue: stub of doPhysicalProjectionElimination, blocked-by:D10.1 (PhysicalProjection / PhysicalTableReader; postOptimize, VS1-24)
// glue: stub of eliminatePhysicalProjection, blocked-by:D10.1 (the physical half of the rule, run by postOptimize, VS1-24)
/// The physical projection elimination of `postOptimize`.
pub fn eliminate_physical_projection() -> Result<(), Error> {
    Err(blocked(
        "D10.1",
        "the physical projection elimination of postOptimize (eliminatePhysicalProjection)",
    ))
}

/// For select, insert, delete list
/// The projection eliminate in logical optimize will optimize the projection under the projection, window, agg
/// The projection eliminate in post optimize will optimize other projection
///
/// ProjectionEliminator is for update stmt
/// The projection eliminate in logical optimize has been forbidden.
/// The projection eliminate in post optimize will optimize the projection under the projection, window, agg (the condition is same as logical optimize)
pub struct ProjectionEliminator;

impl LogicalOptRule for ProjectionEliminator {
    /// Optimize implements the logicalOptRule interface.
    // go: ProjectionEliminator.Optimize L1
    fn optimize(
        &self,
        ctx: &mut PlanCtx<'_>,
        lp: PlanId,
        opt: &LogicalOptimizeOp,
    ) -> Result<(PlanId, bool), Error> {
        let plan_changed = false;
        let root = self.eliminate(ctx, lp, &mut ReplaceMap::new(), false, opt);
        Ok((root, plan_changed))
    }

    /// Name implements the logicalOptRule.<1st> interface.
    // go: ProjectionEliminator.Name L1
    fn name(&self) -> &'static str {
        "projection_eliminate"
    }
}

impl ProjectionEliminator {
    /// eliminate eliminates the redundant projection in a logical plan. Go's type switch
    /// arms for operators not ported yet (CTE, UnionAll, Aggregation, Window, Join, Apply)
    /// cannot be reached: those operators are not in the tree.
    // go: ProjectionEliminator.eliminate L1 [M-PLAN, M-IFACE, M-EXPR, M-FT]
    pub fn eliminate(
        &self,
        ctx: &mut PlanCtx<'_>,
        p: PlanId,
        replace: &mut ReplaceMap,
        can_eliminate: bool,
        opt: &LogicalOptimizeOp,
    ) -> PlanId {
        // LogicalCTE's logical optimization is independent. (no CTE operator yet)
        let kind = ctx.arena.get(p).kind();
        let is_proj = kind == LogicalKind::Projection;
        let mut child_flag = can_eliminate;
        // *LogicalUnionAll → false; *LogicalAggregation, *LogicalWindow → true (not ported yet).
        if is_proj {
            child_flag = true;
        }
        // The child list is read by index because `ctx` is lent to the recursive call.
        for i in 0..ctx.arena.get(p).children().len() {
            let child = ctx.arena.get(p).children()[i];
            let new_child = self.eliminate(ctx, child, replace, child_flag, opt);
            ctx.arena.get_mut(p).children_mut()[i] = new_child;
        }

        // replace logical plan schema
        // (*LogicalJoin / *LogicalApply: BuildLogicalJoinSchema, with the join, D09.3)
        {
            let schema = ctx.arena.schema_mut(p);
            for dst in schema.columns.iter_mut() {
                *dst = resolve_column_and_replace(dst, replace);
            }
        }
        ctx.arena.get_mut(p).replace_expr_columns(replace);

        // eliminate duplicate projection: projection with child projection
        if is_proj {
            let child = ctx.arena.get(p).children()[0];
            if let Some(child_proj) = ctx.arena.downcast_ref::<LogicalProjection>(child)
                && !exprs_has_side_effects(&child_proj.exprs)
            {
                // `proj` (mutable) and `child` (read) are two arena slots at once, so `proj`
                // is detached for the loop.
                with_detached(ctx, p, |node, ctx| {
                    let proj = node
                        .as_any_mut()
                        .downcast_mut::<LogicalProjection>()
                        .expect("LogicalProjection");
                    let child = ctx
                        .arena
                        .downcast_ref::<LogicalProjection>(child)
                        .expect("LogicalProjection");
                    let ectx = ctx.eval_ctx();
                    for i in 0..proj.exprs.len() {
                        replace_column_of_expr(&mut proj.exprs[i], child, child.schema(&ctx.arena));
                        let mut folded_expr =
                            fold_constant(&*ctx.expr_ctx, Arc::clone(&proj.exprs[i]));
                        // the folded expr should have the same null flag with the original expr, especially for the projection under union, so forcing it here.
                        // Go sets the flag on the folded node in place, which may be a node
                        // (and *FieldType) shared with the child's schema; here the node is
                        // copied if shared (M-FT audit of the VS1 plan).
                        let flag = (folded_expr.get_type(&*ectx).get_flag() & !NotNullFlag)
                            | (proj.exprs[i].get_type(&*ectx).get_flag() & NotNullFlag);
                        set_ret_type_flag(&mut folded_expr, flag);
                        proj.exprs[i] = folded_expr;
                    }
                    proj.children_mut()[0] = child.children()[0];
                    append_dup_proj_eliminate_trace_step(proj, child, ctx, opt);
                });
            }
        }

        let eliminable = is_proj
            && can_eliminate
            && can_projection_be_eliminated_loose(
                ctx.arena
                    .downcast_ref::<LogicalProjection>(p)
                    .expect("LogicalProjection"),
            );
        if !eliminable {
            return p;
        }
        let proj = ctx
            .arena
            .downcast_ref::<LogicalProjection>(p)
            .expect("LogicalProjection");
        let exprs = &proj.exprs;
        for (i, col) in proj.schema(&ctx.arena).columns.iter().enumerate() {
            let Expr::Column(c) = &*exprs[i] else {
                unreachable!("canProjectionBeEliminatedLoose");
            };
            // Go stores the expression's own *Column; the Rust expression holds the column
            // inline, so it is copied into an `Arc<Column>`.
            replace.insert(col.hash_code().to_vec(), Arc::new(c.clone()));
        }
        append_proj_eliminate_trace_step(trace_id(proj), opt);
        proj.children()[0]
    }
}

/// `foldedExpr.GetType(ctx).SetFlag(flag)`: copy-on-write when the node or its type is
/// shared.
// glue: M-FT (Go changes the *FieldType of a possibly shared node in place)
fn set_ret_type_flag(e: &mut ExprRef, flag: usize) {
    if e.get_static_type().get_flag() == flag {
        return;
    }
    match Arc::make_mut(e) {
        Expr::Column(c) => Arc::make_mut(&mut c.ret_type).set_flag(flag),
        Expr::CorrelatedColumn(c) => Arc::make_mut(&mut c.column.ret_type).set_flag(flag),
        Expr::Constant(c) => c.ret_type.set_flag(flag),
        Expr::ScalarFunction(f) => Arc::make_mut(&mut f.function.tp).set_flag(flag),
    }
}

/// ReplaceColumnOfExpr replaces column of expression by another LogicalProjection. Go
/// returns the (possibly in-place modified) expression; here the slot is updated.
// go: ReplaceColumnOfExpr L1 [M-EXPR]
pub fn replace_column_of_expr(expr: &mut ExprRef, proj: &LogicalProjection, schema: &Schema) {
    match &**expr {
        Expr::Column(v) => {
            let idx = schema.column_index(v);
            if idx != -1 && (idx as usize) < proj.exprs.len() {
                *expr = Arc::clone(&proj.exprs[idx as usize]);
            }
        }
        Expr::ScalarFunction(v) => {
            // Go: v.GetArgs()[i] = ReplaceColumnOfExpr(...) in place; copy-on-write here,
            // and only when an argument changes.
            let mut new_args = v.get_args().to_vec();
            let mut changed = false;
            for arg in new_args.iter_mut() {
                let before = Arc::clone(arg);
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

// go: appendDupProjEliminateTraceStep L1 [M-CTX]
fn append_dup_proj_eliminate_trace_step(
    parent: &LogicalProjection,
    child: &LogicalProjection,
    ctx: &PlanCtx<'_>,
    opt: &LogicalOptimizeOp,
) {
    let (ptp, pid) = trace_id(parent);
    let (ctp, cid) = trace_id(child);
    let action = || {
        let ectx = ctx.eval_ctx();
        let mut buffer =
            format!("{ctp}_{cid} is eliminated, {ptp}_{pid}'s expressions changed into[");
        for (i, expr) in parent.exprs.iter().enumerate() {
            if i > 0 {
                buffer.push(',');
            }
            buffer.push_str(&String::from_utf8_lossy(
                &expr.string_with_ctx(&*ectx, RedactLogDisable),
            ));
        }
        buffer.push(']');
        buffer
    };
    let reason = || format!("{ptp}_{pid}'s child {ctp}_{cid} is redundant");
    opt.append_step_to_current(cid as isize, ctp, reason, action);
}

// go: appendProjEliminateTraceStep L1 [M-CTX]
fn append_proj_eliminate_trace_step(proj: TraceId, opt: &LogicalOptimizeOp) {
    let (tp, id) = proj;
    let reason = || format!("{tp}_{id}'s Exprs are all Columns");
    let action = || format!("{tp}_{id} is eliminated");
    opt.append_step_to_current(id as isize, tp, reason, action);
}
