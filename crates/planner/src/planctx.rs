//! `PlanCtx` of N1 §6.3 / N2 §3.3: what Go reaches through `p.SCtx()` while planning.
//!
//! glue: N1/N2 (Go: `base.PlanContext`, carried by every plan node).

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_expr_pushdown::infer_pushdown::{PushDownContext, new_push_down_context_from_session_vars};
use tidb_kv::kv::Client;
use tidb_sessionctx::exprctx::{BuildContextRef, EvalContextRef, ExprContext, ExprContextRef};
use tidb_sessionctx::ranger::context::RangerContext;
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
    /// `p.SCtx().GetClient()`: the store's coprocessor client, read by the push down checks
    /// (`util.GetPushDownCtx`). `None` until the session hands it over with the routed
    /// statement (VS1-24 / VS1-37); the push down functions then return a boundary error.
    pub client: Option<Arc<dyn Client>>,
}

impl<'s> PlanCtx<'s> {
    /// A context with an empty arena.
    // glue: N1
    pub fn new(vars: &'s mut SessionVars, expr_ctx: ExprContextRef) -> PlanCtx<'s> {
        PlanCtx {
            arena: PlanArena::new(),
            vars,
            expr_ctx,
            client: None,
        }
    }

    /// `util.GetPushDownCtx(p.SCtx())`: the context of the expression push down checks,
    /// from the session vars, the statement's warning handlers and the store client.
    // glue: N2 (util.GetPushDownCtx over the PlanCtx; the fields are the BuildPBContext's)
    pub fn push_down_ctx(&self) -> Result<PushDownContext<'static>, Error> {
        let Some(client) = &self.client else {
            return Err(blocked(
                "D09.13",
                "the store client of the planning context (GetPushDownCtx)",
            ));
        };
        Ok(new_push_down_context_from_session_vars(
            self.eval_ctx(),
            self.vars,
            Arc::clone(client),
        ))
    }

    /// `p.SCtx().GetExprCtx().GetEvalCtx()`.
    // glue: N2
    pub fn eval_ctx(&self) -> EvalContextRef {
        self.expr_ctx.get_eval_ctx()
    }

    /// `p.SCtx().GetRangerCtx()`: the context of the ranger functions, from the session
    /// vars and the statement context (Go's mock context builds it on each call, the
    /// session caches it in the statement context; the fields are the same). The plan
    /// cache tracker and the range fallback handler are the statement's.
    // glue: N2 (planctx.PlanContext.GetRangerCtx)
    pub fn ranger_ctx(&self) -> RangerContext {
        let sc = self.vars.stmt_ctx();
        RangerContext {
            expr_ctx: Arc::clone(&self.expr_ctx) as BuildContextRef,
            type_ctx: sc.type_ctx(),
            err_ctx: sc.err_ctx(),
            in_prepared_plan_building: sc.in_prepared_plan_building,
            regard_null_as_point: self.vars.regard_null_as_point,
            opt_prefix_index_single_scan: self.vars.opt_prefix_index_single_scan,
            optimizer_fix_control: self.vars.get_optimizer_fix_control_map().clone(),
            plan_cache_tracker: Arc::clone(&sc.plan_cache_tracker),
            range_fallback_handler: Arc::clone(&sc.range_fallback_handler),
            use_cache: false,
        }
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
