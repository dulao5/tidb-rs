//! `PlanCtx` of N1 §6.3 / N2 §3.3: what Go reaches through `p.SCtx()` while planning.
//!
//! glue: N1/N2 (Go: `base.PlanContext`, carried by every plan node).

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_expr_pushdown::infer_pushdown::{
    PushDownContext, new_push_down_context_from_session_vars,
};
use tidb_kv::kv::Client;
use tidb_privilege::PrivilegeManagerRef;
use tidb_sessionctx::context::InstancePlanCache;
use tidb_sessionctx::exprctx::{BuildContextRef, EvalContextRef, ExprContext, ExprContextRef};
use tidb_sessionctx::ranger::context::RangerContext;
use tidb_sessionctx::session_mgr::SessionManager;
use tidb_sessionctx::variable::session::SessionVars;

use crate::arena::PlanArena;
use crate::core::plan_cache_utils::InfoSchemaLookup;

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
    /// `p.SCtx().HasDirtyContent(tid)`: whether the session's transaction has written the
    /// table (its membuffer). `None` until the session hands it over with the routed
    /// statement (VS1-24 / VS1-37); the plan builder then returns a boundary error.
    pub has_dirty_content: Option<HasDirtyContentFn>,
    /// `p.SCtx().AdviseTxnWarmup()`: advises the transaction to warm up (fetch its start
    /// TSO early). `None` is the mock contexts' `EmptyPlanContextExtended.AdviseTxnWarmup`
    /// (nothing to do).
    pub advise_txn_warmup: Option<AdviseTxnWarmupFn>,
    /// `p.SCtx().GetDomainInfoSchema()`: the domain's newest InfoSchema, read by the plan
    /// replayer capture's `recordSingleTableRuntimeStats`. `None` for a context without a
    /// domain (the test contexts), where that lookup finds no table.
    pub domain_info_schema: Option<Arc<dyn InfoSchemaLookup>>,
    /// `domain.GetDomain(sctx).GetInstancePlanCache()`: the instance-level plan cache (the
    /// plan-cache exception A3). `None` without a domain: nothing is cached.
    pub instance_plan_cache: Option<Arc<dyn InstancePlanCache>>,
    /// `privilege.GetPrivilegeManager(sctx)`: the session's privilege manager (D04.1). `None`
    /// when unbound (Go's `pm == nil` path, skipping runtime privilege checks).
    pub privilege_manager: Option<PrivilegeManagerRef>,
    /// `p.SCtx().GetSessionManager()`: session registry used by `KILL` privilege checks.
    pub session_manager: Option<Arc<dyn SessionManager>>,
}

/// `HasDirtyContent(tid)` of the session.
pub type HasDirtyContentFn = Arc<dyn Fn(i64) -> bool + Send + Sync>;

/// `AdviseTxnWarmup()` of the session.
pub type AdviseTxnWarmupFn = Arc<dyn Fn() -> Result<(), Error> + Send + Sync>;

/// What the session hands the planner for each statement, besides its session variables:
/// the expression context, the store client, the transaction's dirty-table check and the
/// warm-up hook. `planner.Optimize` and PREPARE (`GeneratePlanCacheStmtWithAST`) build their
/// planning context from the same parts ([`SessionPlanCtx::plan_ctx`]), as Go's both use
/// `sctx.GetPlanCtx()`.
// glue: N2 (the session side of base.PlanContext: sctx.GetPlanCtx())
#[derive(Clone)]
pub struct SessionPlanCtx {
    /// `GetExprCtx()`.
    pub expr_ctx: ExprContextRef,
    /// `GetClient()` (`None`: no store client; the push down checks are then a boundary).
    pub client: Option<Arc<dyn Client>>,
    /// `HasDirtyContent(tid)` (`None`: the transaction's dirty tables are unknown; the
    /// plan builder is then a boundary).
    pub has_dirty_content: Option<HasDirtyContentFn>,
    /// `AdviseTxnWarmup()`.
    pub advise_txn_warmup: Option<AdviseTxnWarmupFn>,
    /// `GetDomainInfoSchema()` (`None`: no domain).
    pub domain_info_schema: Option<Arc<dyn InfoSchemaLookup>>,
    /// `domain.GetDomain(sctx).GetInstancePlanCache()` (`None`: no domain, nothing cached).
    pub instance_plan_cache: Option<Arc<dyn InstancePlanCache>>,
    /// `privilege.GetPrivilegeManager(sctx)` (`None`: no privilege manager bound).
    pub privilege_manager: Option<PrivilegeManagerRef>,
    /// `sctx.GetSessionManager()` (`None`: no session manager bound).
    pub session_manager: Option<Arc<dyn SessionManager>>,
}

impl SessionPlanCtx {
    /// The planning context of one statement over the session's variables.
    // glue: N2 (sctx.GetPlanCtx())
    pub fn plan_ctx<'s>(&self, vars: &'s mut SessionVars) -> PlanCtx<'s> {
        let mut ctx = PlanCtx::new(vars, Arc::clone(&self.expr_ctx));
        ctx.client = self.client.as_ref().map(Arc::clone);
        ctx.has_dirty_content = self.has_dirty_content.as_ref().map(Arc::clone);
        ctx.advise_txn_warmup = self.advise_txn_warmup.as_ref().map(Arc::clone);
        ctx.domain_info_schema = self.domain_info_schema.as_ref().map(Arc::clone);
        ctx.instance_plan_cache = self.instance_plan_cache.as_ref().map(Arc::clone);
        ctx.privilege_manager = self.privilege_manager.as_ref().map(Arc::clone);
        ctx.session_manager = self.session_manager.as_ref().map(Arc::clone);
        ctx
    }

    /// The parts of an existing planning context (for the nested `Optimize` of the plan
    /// cache's miss path, which gets the session variables again).
    // glue: N2 (the planning context of the same statement)
    pub fn of(ctx: &PlanCtx<'_>) -> SessionPlanCtx {
        SessionPlanCtx {
            expr_ctx: Arc::clone(&ctx.expr_ctx),
            client: ctx.client.as_ref().map(Arc::clone),
            has_dirty_content: ctx.has_dirty_content.as_ref().map(Arc::clone),
            advise_txn_warmup: ctx.advise_txn_warmup.as_ref().map(Arc::clone),
            domain_info_schema: ctx.domain_info_schema.as_ref().map(Arc::clone),
            instance_plan_cache: ctx.instance_plan_cache.as_ref().map(Arc::clone),
            privilege_manager: ctx.privilege_manager.as_ref().map(Arc::clone),
            session_manager: ctx.session_manager.as_ref().map(Arc::clone),
        }
    }
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
            has_dirty_content: None,
            advise_txn_warmup: None,
            domain_info_schema: None,
            instance_plan_cache: None,
            privilege_manager: None,
            session_manager: None,
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
