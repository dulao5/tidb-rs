//! `PlanCtx` of N1 §6.3 / N2 §3.3: what Go reaches through `p.SCtx()` while planning.
//!
//! glue: N1/N2 (Go: `base.PlanContext`, carried by every plan node).

use tidb_sessionctx::exprctx::{EvalContextRef, ExprContext, ExprContextRef};
use tidb_sessionctx::variable::session::SessionVars;

use crate::arena::PlanArena;

/// The planning context of one statement.
///
/// Fields are public on purpose: inside one function, `ctx.arena`, `ctx.expr_ctx` and
/// `ctx.vars` are disjoint field borrows and need no [`PlanCtx::split`] (pilot finding F3).
///
/// TODO(VS1-10): the statement context (`&'s StmtCtx`) and the InfoSchema the statement was
/// routed with (VS1 plan §4.1) join these fields with the plan builder.
pub struct PlanCtx<'s> {
    /// The statement's logical plan arena (N1).
    pub arena: PlanArena,
    /// `p.SCtx().GetSessionVars()` (N2: the session's vars, borrowed for the statement).
    pub vars: &'s mut SessionVars,
    /// `p.SCtx().GetExprCtx()`: a shared `Arc<dyn ExprContext>` whose warnings and
    /// column-id allocator are interior-mutable (N4), so reading it never needs `&mut`.
    pub expr_ctx: ExprContextRef,
}

impl<'s> PlanCtx<'s> {
    /// A context with an empty arena.
    // glue: N1
    pub fn new(vars: &'s mut SessionVars, expr_ctx: ExprContextRef) -> PlanCtx<'s> {
        PlanCtx {
            arena: PlanArena::new(),
            vars,
            expr_ctx,
        }
    }

    /// `p.SCtx().GetExprCtx().GetEvalCtx()`.
    // glue: N2
    pub fn eval_ctx(&self) -> EvalContextRef {
        self.expr_ctx.get_eval_ctx()
    }

    /// N1 §4, the fallback idiom: disjoint borrows of the arena (mutable), the expression
    /// context (shared) and the session vars (mutable), for code that must hand them out
    /// through one `&mut PlanCtx` across a function boundary. The standard idiom is detached
    /// dispatch plus field borrows; a block plan that uses `split()` says why.
    // glue: N1
    pub fn split(&mut self) -> (&mut PlanArena, &dyn ExprContext, &mut SessionVars) {
        (&mut self.arena, &*self.expr_ctx, &mut *self.vars)
    }

    /// Go's plan id (`SessionVars.PlanID.Add(1)` in `baseimpl.NewBasePlan`), allocated where
    /// Go allocates it so that EXPLAIN ids match (N1 §3.6).
    // glue: N1
    pub fn alloc_plan_id(&self) -> i32 {
        i32::try_from(self.vars.alloc_new_plan_id()).expect("plan id")
    }
}
