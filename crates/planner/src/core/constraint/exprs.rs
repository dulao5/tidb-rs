//! go-file: pkg/planner/core/constraint/exprs.go
//!
//! `DeleteTrueExprs`, called by predicate push down (D09.8). `DeleteTrueExprsBySchema` and
//! its helper belong to the join's predicate push down (block D09.3) and stay with it.

use std::sync::Arc;

use tidb_expression::constant_fold::maybe_over_optimized4_plan_cache;
use tidb_expression::{Expr, ExprRef};

use crate::planctx::PlanCtx;

/// DeleteTrueExprs deletes the surely true expressions. Go passes the plan for
/// `p.SCtx()`; here the context is the `PlanCtx` (`M-CTX`).
// go: DeleteTrueExprs L1 [M-CTX]
pub fn delete_true_exprs(ctx: &PlanCtx<'_>, conds: &[ExprRef]) -> Vec<ExprRef> {
    let mut new_conds = Vec::with_capacity(conds.len());
    for cond in conds {
        let Expr::Constant(con) = &**cond else {
            new_conds.push(Arc::clone(cond));
            continue;
        };
        if maybe_over_optimized4_plan_cache(&*ctx.expr_ctx, std::slice::from_ref(cond)) {
            new_conds.push(Arc::clone(cond));
            continue;
        }
        let sc = ctx.vars.stmt_ctx();
        let (is_true, err) = con.value.to_bool(&sc.type_ctx());
        if err.is_ok() && is_true == 1 {
            continue;
        }
        new_conds.push(Arc::clone(cond));
    }
    new_conds
}
