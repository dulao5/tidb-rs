//! go-file: pkg/planner/optimize.go
//!
//! Go package `pkg/planner`: `Optimize`, the entry point the compiler calls, and the
//! optimizer driver around it (block D09.13): the statement hints (D13.3), the fast plan
//! (`TryFastPlan`, D10.8), the plan cache (D13.1, prepared statements: `OptimizeExecStmt`),
//! then `optimize`, which builds the logical plan (`PlanBuilder.Build`) and runs
//! `DoOptimize` (`core::optimizer`).
//!
//! The planning context (`PlanCtx`) is built by the caller from the session's parts
//! ([`crate::planctx::SessionPlanCtx`]); PREPARE (`GeneratePlanCacheStmtWithAST`) builds its
//! plan with the same context.
//!
//! Boundaries on the way (each explicit, see the spec `specs/D09.13-optimizer/spec.md`):
//! SQL bindings (the new stack runs only without bindings, VS1 plan §3.2), the non-prepared
//! plan cache (D13.1, off by default), the alternative logical plan rounds
//! (`tidb_opt_enable_alternative_logical_plans`, off by default: they need the statement
//! context's build-state save / restore), the cascades planner (D09.17), the debug trace.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_base::dbterror::plannererrors::{
    ErrNotHintUpdatable, ErrSQLInReadOnlyMode, ErrUnresolvedHintName,
};
use tidb_base::gofmt::Arg;
use tidb_base::kv;
use tidb_base::mysql::SQLMode;
use tidb_parser::ast::{ExecuteStmt, SelectLockType, SelectStmt, SetOprNode, Stmt};
use tidb_parser::model::CIStr;
use tidb_sessionctx::variable::session::SessionVars;
use tidb_sessionctx::variable::tidb_vars::{
    EnableResourceControl, RestrictedReadOnly, VarTiDBSuperReadOnly,
};
use tidb_sessionctx::variable::variable::get_sys_var;
use tidb_types::field_name::NameSlice;

use crate::core::common_plans::Execute;
use crate::core::optimizer::{
    check_table_lock, check_table_mode, do_optimize_plan, visit_info4_priv_check,
};
use crate::core::plan_cache::get_plan_from_plan_cache;
use crate::core::plan_cache_utils::{InfoSchemaLookup, get_prepared_stmt};
use crate::core::plan_cacheable_checker::{CNode, Visit, Walker};
use crate::core::planbuilder::{PlanBuilder, get_db_table_info, new_plan_builder};
use crate::core::point_get_plan::try_fast_plan;
use crate::core::preprocess::{InfoSchemaHandle, try_add_extra_limit};
use crate::core::resolve::NodeW;
use crate::hint::hint::parse_stmt_hints;
use crate::hint::hint_processor::extract_table_hints_from_stmt_node;
use crate::hint::{QBHintHandler, StmtHints};
use crate::physicalop::PhysicalPlanRef;
use crate::planctx::{PlanCtx, SessionPlanCtx};

/// The plan `Optimize` returns (Go's `base.Plan`): the `Execute` plan of an `EXECUTE`
/// statement, or the final physical plan.
pub enum OptimizedPlan {
    /// `*core.Execute`.
    Execute(Box<Execute>),
    /// A `base.PhysicalPlan`.
    Physical(PhysicalPlanRef),
}

/// IsReadOnly check whether the ast.Node is a read only statement.
// go: IsReadOnly L1
pub fn is_read_only(node: &Stmt, vars: &SessionVars) -> Result<bool, Error> {
    if let Stmt::Execute(exec_stmt) = node {
        let prepare_stmt = match get_prepared_stmt(exec_stmt, vars) {
            Ok(p) => p,
            Err(err) => {
                log::warn!("GetPreparedStmt failed: {err}");
                return Ok(false);
            }
        };
        let prepare_stmt = prepare_stmt.lock();
        return ast_is_read_only(&prepare_stmt.prepared_ast.stmt);
    }
    ast_is_read_only(node)
}

/// `ast.IsReadOnly` of the parser for the statements of the Rust AST: SELECT (with the
/// `readOnlyChecker` walk), set operations and SHOW are Go's arms; EXPLAIN, DO and ADMIN are
/// not in the Rust AST (D02.2), the other statements take Go's `default: return false`.
// go: pkg/parser/ast/util.go IsReadOnly L2 (EXPLAIN / DO / ADMIN are not in the Rust AST)
pub fn ast_is_read_only(node: &Stmt) -> Result<bool, Error> {
    match node {
        Stmt::Select(st) => select_is_read_only(st),
        Stmt::Show(_) => Ok(true),
        Stmt::SetOpr(st) => {
            for sel in &st.select_list.selects {
                if !set_opr_node_is_read_only(sel)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        Stmt::Unported(_) => Err(blocked(
            "D02.2",
            "IsReadOnly of a statement the Rust AST does not have",
        )),
        _ => Ok(false),
    }
}

/// The `*SelectStmt` arm of `ast.IsReadOnly`.
// go-part: pkg/parser/ast/util.go IsReadOnly (the SelectStmt arm) L1
fn select_is_read_only(st: &SelectStmt) -> Result<bool, Error> {
    if let Some(lock_info) = &st.lock_info {
        match lock_info.lock_type {
            SelectLockType::ForUpdate
            | SelectLockType::ForUpdateNoWait
            | SelectLockType::ForUpdateWaitN
            | SelectLockType::ForShare
            | SelectLockType::ForShareNoWait => return Ok(false),
            _ => {}
        }
    }

    let mut checker = ReadOnlyChecker {
        read_only: true,
        err: None,
    };
    Walker(&mut checker).select(st);
    if let Some(e) = checker.err {
        return Err(e);
    }
    Ok(checker.read_only)
}

/// The `*SetOprSelectList` arm of `ast.IsReadOnly` (a list operand) and the SELECT operand.
// go-part: pkg/parser/ast/util.go IsReadOnly (the SetOprSelectList arm) L1
fn set_opr_node_is_read_only(sel: &SetOprNode) -> Result<bool, Error> {
    match sel {
        SetOprNode::Select(s) => select_is_read_only(s),
        SetOprNode::List(list) => {
            for sel in &list.selects {
                if !set_opr_node_is_read_only(sel)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        SetOprNode::Unported(_) => Err(blocked(
            "D02.2",
            "IsReadOnly of a set operation operand the Rust AST does not have",
        )),
    }
}

/// readOnlyChecker checks whether a query's ast is readonly, if it satisfied
/// 1. selectstmt;
/// 2. need not to set var;
///
/// it is readonly statement.
struct ReadOnlyChecker {
    read_only: bool,
    /// A node the Rust AST does not have (the walk stops: the answer is unknown).
    err: Option<Error>,
}

impl Visit for ReadOnlyChecker {
    /// Enter implements Visitor interface.
    // go: pkg/parser/ast/util.go readOnlyChecker.Enter L1
    fn enter(&mut self, n: CNode<'_>) -> bool {
        if let CNode::Variable(node) = n {
            // like func rewriteVariable(), this stands for SetVar.
            if !node.is_system && node.value.is_some() {
                self.read_only = false;
                return true;
            }
        }
        false
    }

    /// Leave implements Visitor interface.
    // go: pkg/parser/ast/util.go readOnlyChecker.Leave L1
    fn leave(&mut self, _n: CNode<'_>) -> bool {
        self.read_only
    }

    // glue: ast.Visitor (a node the Rust AST does not have)
    fn fail(&mut self, e: Error) {
        self.err = Some(e);
    }
}

/// `getPlanFromNonPreparedPlanCache`: the non-prepared plan cache
/// (`tidb_enable_non_prepared_plan_cache`, off by default) is the plan-cache exception A3.
// glue: stub of getPlanFromNonPreparedPlanCache, blocked-by:D13.1 (the non-prepared plan cache is the plan-cache exception A3)
fn get_plan_from_non_prepared_plan_cache() -> Result<(), Error> {
    Err(blocked(
        "D13.1",
        "the non-prepared plan cache (getPlanFromNonPreparedPlanCache)",
    ))
}

/// Optimize does optimization and creates a Plan.
///
/// `domain_is` is the domain's newest InfoSchema, for the metadata lock of the plan cache's
/// preprocessing (`OptimizeExecStmt`). Differences, all boundaries or recorded in the spec:
/// Go's `recover` of a panic is the router's (it answers error 1105, port-block rules); the
/// session keeps no `PointPlanKey` value (multi-statement point plans are not routed to the
/// new stack); SQL bindings: the new stack runs only when the cluster has none (VS1 plan
/// §3.2), so `MatchSQLBinding` never matches; the non-prepared plan cache is a boundary.
// go: Optimize L2 (panics: the router's recovery; bindings never match on the new stack, VS1 plan §3.2; the non-prepared plan cache is blocked-by:D13.1)
pub fn optimize_stmt(
    ctx: &mut PlanCtx<'_>,
    node: &mut NodeW<'_>,
    is: &dyn InfoSchemaLookup,
    domain_is: Option<&dyn InfoSchemaLookup>,
) -> Result<(OptimizedPlan, NameSlice), Error> {
    if ctx.vars.stmt_ctx().enable_optimizer_debug_trace {
        // debugtrace.EnterContextCommon(pctx); defer debugtrace.LeaveContextCommon(pctx)
        // glue: blocked-by:D02.2 (the optimizer debug trace is only switched on by PLAN REPLAYER, not in the Rust AST)
        return Err(blocked(
            "D02.2",
            "the optimizer debug trace (PLAN REPLAYER)",
        ));
    }

    if !ctx.vars.in_restricted_sql
        && (RestrictedReadOnly.load(Ordering::SeqCst)
            || VarTiDBSuperReadOnly.load(Ordering::SeqCst))
    {
        let allowed = allow_in_read_only_mode(node.node)?;
        if !allowed {
            return Err(ErrSQLInReadOnlyMode.clone());
        }
    }

    let res = optimize_stmt_body(ctx, node, is, domain_is);

    // The deferred function: override the resource group if the hint is set, and handle
    // the SetVars hints for cached plans.
    if res.is_ok() {
        optimize_stmt_on_success(ctx)?;
    }
    res
}

/// The deferred function of `Optimize` that runs when it succeeds: the resource group hint
/// and the `SET_VAR` hints. Hints are not parsed by the Rust parser (D02.2), so a statement
/// that reaches here has the default `StmtHints` (no resource group, no `SET_VAR`); the
/// branches are ported for when they come.
// go-part: Optimize (the deferred resource group / SetVars handling) L2 (the privilege manager of the strict resource control mode is D04.1)
fn optimize_stmt_on_success(ctx: &mut PlanCtx<'_>) -> Result<(), Error> {
    // Override the resource group if the hint is set.
    if ctx.vars.stmt_ctx().stmt_hints.has_resource_group {
        if EnableResourceControl.load(Ordering::SeqCst) {
            // only check dynamic privilege when strict-mode is enabled: there is no privilege
            // manager (D04.1), so hasPriv stays true. Updating the transaction's resource
            // group (kv.SetTxnResourceGroup) needs the transaction: a boundary.
            // glue: blocked-by:D15.2 (the resource group of a RESOURCE_GROUP hint: resource control is D15.2)
            return Err(blocked("D15.2", "the RESOURCE_GROUP hint"));
        }
        let err = tidb_infoschema::error::ErrResourceGroupSupportDisabled.clone();
        ctx.vars.stmt_ctx().append_warning(err);
    }

    // Handle SetVars hints for cached plans.
    set_var_hints(ctx.vars);
    Ok(())
}

/// `for name, val := range sessVars.StmtCtx.StmtHints.SetVars { ... }`: set the `SET_VAR`
/// hints' values and remember the old ones to restore after the statement.
// go-part: Optimize (the SetVars loop) L1
fn set_var_hints(vars: &mut SessionVars) {
    let set_vars: Vec<(String, String)> = vars
        .stmt_ctx()
        .stmt_hints
        .set_vars
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    for (name, val) in set_vars {
        let (old_v, err) = vars.set_system_var_with_old_val_as_ret(&name, &val);
        if let Err(err) = err {
            vars.stmt_ctx().append_warning(err);
        }
        vars.stmt_ctx_mut().add_set_var_hint_restore(name, old_v);
    }
}

/// `Optimize` after the read-only check, up to the deferred functions.
// go-part: Optimize (the body between the read-only check and the deferred functions) L2 (see optimize_stmt)
fn optimize_stmt_body(
    ctx: &mut PlanCtx<'_>,
    node: &mut NodeW<'_>,
    is: &dyn InfoSchemaLookup,
    domain_is: Option<&dyn InfoSchemaLookup>,
) -> Result<(OptimizedPlan, NameSlice), Error> {
    // Handle the execute statement. This calls into the prepared plan cache.
    if let Stmt::Execute(exec_ast) = &*node.node {
        let (p, names) = optimize_exec_stmt(ctx, exec_ast, is, domain_is)?;
        return Ok((OptimizedPlan::Execute(Box::new(p)), names));
    }

    let table_hints = extract_table_hints_from_stmt_node(node.node);
    let current_db = ctx.vars.current_db.clone();
    let (origin_stmt_hints, _, warns) = parse_stmt_hints(
        &table_hints,
        &set_var_hint_checker,
        &hypo_index_checker,
        &current_db,
        tidb_kv::option::ReplicaReadFollower.0,
    )?;
    let has_table_hints = !table_hints.is_empty();
    drop(table_hints);
    ctx.vars.stmt_ctx_mut().stmt_hints = origin_stmt_hints.clone_hints();
    for warn in warns {
        ctx.vars.stmt_ctx().append_warning(warn);
    }

    set_var_hints(ctx.vars);

    // This should be handled after `set_var`
    let mut tiflash_removed = None;
    if SQLMode(ctx.vars.sql_mode).has_strict_mode() && !is_read_only(node.node, ctx.vars)? {
        ctx.vars
            .stmt_ctx_mut()
            .tiflash_engine_removed_due_to_strict_sql_mode = true;
        let has_tiflash_access = ctx.vars.state.isolation_read_engines.contains(&kv::TiFlash);
        if has_tiflash_access {
            ctx.vars.state.isolation_read_engines.remove(&kv::TiFlash);
        }
        tiflash_removed = Some(has_tiflash_access);
    }
    let res = optimize_stmt_plan(ctx, node, is, origin_stmt_hints, has_table_hints);
    if let Some(has_tiflash_access) = tiflash_removed {
        // the deferred restore of the TiFlash engine
        ctx.vars
            .stmt_ctx_mut()
            .tiflash_engine_removed_due_to_strict_sql_mode = false;
        if has_tiflash_access {
            ctx.vars.state.isolation_read_engines.insert(kv::TiFlash);
        }
    }
    res
}

/// `Optimize` from the fast plan on (inside the strict-mode TiFlash defer).
// go-part: Optimize (the fast plan, bindings, plan cache and optimize) L2 (see optimize_stmt)
fn optimize_stmt_plan(
    ctx: &mut PlanCtx<'_>,
    node: &mut NodeW<'_>,
    is: &dyn InfoSchemaLookup,
    origin_stmt_hints: StmtHints,
    has_table_hints: bool,
) -> Result<(OptimizedPlan, NameSlice), Error> {
    if ctx.vars.state.isolation_read_engines.contains(&kv::TiKV) {
        // if fpv, ok := sctx.Value(core.PointPlanKey).(core.PointPlanVal): the session keeps
        // no point plan of a multi-statement query (see optimize_stmt).
        let fp = try_fast_plan(ctx, node)?;
        if let Some(fp) = fp {
            let names = fp.output_names();
            return Ok((OptimizedPlan::Physical(fp), names));
        }
    }
    if let Some(warmup) = &ctx.advise_txn_warmup {
        warmup()?;
    }

    let enable_use_binding = ctx.vars.use_plan_baselines;
    // binding, match, scope := bindinfo.MatchSQLBinding(sctx, stmtNode): no binding on the
    // new stack (VS1 plan §3.2), so nothing matches.
    let matched = false;
    let is_stmt_node = true;
    let use_binding = enable_use_binding && is_stmt_node && matched;
    // add the extra Limit after matching the bind record
    try_add_extra_limit(ctx.vars, node.node);

    // try to get Plan from the NonPrepared Plan Cache
    if ctx.vars.enable_non_prepared_plan_cache && is_stmt_node && !use_binding {
        // TODO: support binding
        get_plan_from_non_prepared_plan_cache()?;
    }

    // useBinding is false (see above): the binding loop does not run; bestPlanFromBind
    // stays nil.
    let _ = has_table_hints;

    // No plan found from the bindings, or the bindings are ignored.
    ctx.vars.stmt_ctx_mut().stmt_hints = origin_stmt_hints;
    let (best_plan, names, _) = optimize(ctx, node, is)?;

    // Add a baseline evolution task if the returned plan is from bindings: it is not
    // (bestPlanFromBind is nil), so the saved hints are restored unchanged.
    Ok((OptimizedPlan::Physical(best_plan), names))
}

/// `OptimizeAstNode` (the function the plan cache's miss path calls, `generateNewPlan`):
/// `planner.Optimize` for the prepared statement's AST, with the planning context of the
/// statement that runs it (`parts`) over the session variables the cache hands back.
// glue: OptimizeAstNode (core's function variable set to planner.Optimize by Go's init)
pub fn optimize_ast_node(
    parts: &SessionPlanCtx,
    vars: &mut SessionVars,
    node: &mut NodeW<'_>,
    is: &dyn InfoSchemaLookup,
) -> Result<(PhysicalPlanRef, NameSlice), Error> {
    let mut ctx = parts.plan_ctx(vars);
    let (p, names) = optimize_stmt(&mut ctx, node, is, None)?;
    match p {
        OptimizedPlan::Physical(p) => Ok((p, names)),
        // A prepared statement is never an EXECUTE (PREPARE rejects it).
        OptimizedPlan::Execute(_) => Err(tidb_base::errors::new(
            "invalid result plan type, should be a physical plan",
        )),
    }
}

/// `allowInReadOnlyMode`: Go returns true at once when no privilege manager is registered.
/// tidb-rs has none until D04.1 (VS1 plan §3.2: the PoC's single-user model), so that is
/// the branch taken; the rest of the function (the dynamic privilege, the statement kinds,
/// COMMIT's transaction check) comes with D04.1.
// go: allowInReadOnlyMode L1 [M-CTX]
pub fn allow_in_read_only_mode(node: &Stmt) -> Result<bool, Error> {
    let _ = node;
    // pm := privilege.GetPrivilegeManager(sctx); pm == nil
    Ok(true)
}

/// `saveLogicalPlanBuildCtx` / `restoreLogicalPlanBuildCtx` save the statement context's
/// logical-plan state, `PlannerSelectBlockAsName` (D13.3), `MapScalarSubQ`,
/// `MapHashCode2UniqueID4ExtendedCol` and `RewritePhaseInfo` between the alternative build
/// rounds (`tidb_opt_enable_alternative_logical_plans`, off by default). The statement
/// context's build state (`StmtCtx.SaveLogicalPlanBuildState`) is not ported, so turning
/// the rounds on is a boundary.
// glue: stub of saveLogicalPlanBuildCtx, blocked-by:D09.13 (StmtCtx.SaveLogicalPlanBuildState of the alternative logical plan rounds, tidb_opt_enable_alternative_logical_plans)
// glue: stub of restoreLogicalPlanBuildCtx, blocked-by:D09.13 (StmtCtx.RestoreLogicalPlanBuildState of the alternative logical plan rounds)
pub fn save_logical_plan_build_ctx() -> Result<(), Error> {
    Err(blocked(
        "D09.13",
        "the alternative logical plan rounds (saveLogicalPlanBuildCtx)",
    ))
}

/// `buildAndOptimizeLogicalPlanRound`: one build of the logical plan and its optimization.
///
/// `PlanBuilder.Build` returns a logical plan for every statement it builds (the other
/// statements are boundaries), so Go's non-logical branch is not reached. The privilege
/// check needs the privilege manager, which the new stack does not have (VS1 plan §3.2):
/// Go skips it when no manager is registered. The cascades planner (`tidb_enable_cascades_planner`
/// / the `USE_CASCADES` hint) is a boundary. Returns the best plan, its names and cost.
// go: buildAndOptimizeLogicalPlanRound L2 (Build returns only logical plans; no privilege manager, D04.1; cascades is blocked-by:D09.17)
fn build_and_optimize_logical_plan_round(
    ctx: &mut PlanCtx<'_>,
    node: &NodeW<'_>,
    is: &dyn InfoSchemaLookup,
    hint_processor: &Arc<QBHintHandler>,
    checked: &mut bool,
) -> Result<(PhysicalPlanRef, NameSlice, f64), Error> {
    let mut builder = new_plan_builder(&[]);
    builder.init(
        ctx.vars,
        Some(InfoSchemaHandle(is.snapshot())),
        Some(Arc::clone(hint_processor)),
    );

    let res = build_and_optimize_with_builder(ctx, node, is, &mut builder, checked);
    // defer builder.HandleUnusedViewHints()
    builder.handle_unused_view_hints(ctx.vars);
    res
}

/// The body of [`build_and_optimize_logical_plan_round`] after `builder.Init`.
// go-part: buildAndOptimizeLogicalPlanRound (after Init) L2 (see build_and_optimize_logical_plan_round)
fn build_and_optimize_with_builder<'ast>(
    ctx: &mut PlanCtx<'_>,
    node: &'ast NodeW<'_>,
    is: &dyn InfoSchemaLookup,
    builder: &mut PlanBuilder<'ast>,
    checked: &mut bool,
) -> Result<(PhysicalPlanRef, NameSlice, f64), Error> {
    // todo: you can customize each round's special builder (like semi join rewrite or not by signal)
    let p = build_logical_plan(ctx, node, builder)?;
    let names = ctx.arena.get(p).output_names(&ctx.arena).clone();

    if !*checked {
        // Keep privilege and lock checks fail-fast. These depend on visitInfo
        // produced by the logical build, but not on the later cost winner.
        // if pm := privilege.GetPrivilegeManager(sctx); pm != nil: there is no privilege
        // manager (D04.1), VisitInfo4PrivCheck / CheckPrivilege are not called.
        let _ = visit_info4_priv_check;

        check_table_lock(builder.get_visit_info())?;

        check_table_mode(node)?;
        *checked = true;
    }
    let _ = is;

    // core.RecheckCTE(logic): CTEs are D09.5 (the builder stops at WITH).

    // todo: also you can customize each round's special logical opt flag here (like decorrelate rule or not)
    let opt_flag = builder.get_opt_flag();
    if ctx.vars.get_enable_cascades_planner() {
        // glue: blocked-by:D09.17 (the cascades planner: cascades.DefaultOptimizer.FindBestPlan)
        return Err(blocked("D09.17", "the cascades planner"));
    }
    let (final_plan, cost) = do_optimize_plan(ctx, opt_flag, p)?;
    Ok((final_plan, names, cost))
}

/// `optimize`. The alternative logical plan rounds (`tidb_opt_enable_alternative_logical_plans`,
/// off by default) need the statement context's build-state save / restore, a boundary
/// ([`save_logical_plan_build_ctx`]); without them there is one round, whose plan is the
/// best plan. `DurationOptimization` is recorded like Go's deferred function.
// go: optimize L2 (the alternative rounds are blocked-by:D09.13: StmtCtx.SaveLogicalPlanBuildState)
pub fn optimize(
    ctx: &mut PlanCtx<'_>,
    node: &mut NodeW<'_>,
    is: &dyn InfoSchemaLookup,
) -> Result<(PhysicalPlanRef, NameSlice, f64), Error> {
    if ctx.vars.stmt_ctx().enable_optimizer_debug_trace {
        // glue: blocked-by:D02.2 (the optimizer debug trace is only switched on by PLAN REPLAYER, not in the Rust AST)
        return Err(blocked(
            "D02.2",
            "the optimizer debug trace (PLAN REPLAYER)",
        ));
    }
    let begin_opt = std::time::Instant::now();

    // Build the logical plan from the raw AST. The hint processor only keeps
    // AST-derived metadata; per-build state is allocated inside PlanBuilder.
    let mut hint_processor = crate::hint::hint_query_block::new_qb_hint_handler();
    hint_processor.accept(node.node, Some(ctx.vars.stmt_ctx()))?;
    let hint_processor = Arc::new(hint_processor);

    // build multi logical plan from raw AST.
    let need_restore_logical_plan_ctx = ctx.vars.enable_alternative_logical_plans;
    let mut checked = false;
    if need_restore_logical_plan_ctx {
        // initialLogicalPlanCtx = saveLogicalPlanBuildCtx(sessVars)
        save_logical_plan_build_ctx()?;
    }

    let res = build_and_optimize_logical_plan_round(ctx, node, is, &hint_processor, &mut checked);
    // optimizeStarted is set when the round reaches the logical optimization; the round
    // reaches it whenever it builds a plan.
    let (best_plan, best_names, best_cost) = res?;
    ctx.vars.duration_optimization = begin_opt.elapsed().as_nanos() as i64;

    // The alternative rounds: none is enabled without tidb_opt_enable_alternative_logical_plans.
    Ok((best_plan, best_names, best_cost))
}

/// OptimizeExecStmt to handle the "execute" statement
///
/// `buildLogicalPlan` (`PlanBuilder.Build`) has no `EXECUTE` arm yet, so its arm
/// (`buildExecute`) is called directly after the same resets. `GetPlanFromPlanCache` is the
/// plan cache's miss path (nothing is cached, the plan-cache exception A3); it ends in
/// `OptimizeAstNode` with this statement's planning context. `exec.Stmt` (the prepared AST)
/// stays in `exec.prep_stmt`. `domain_is` is the session's newest InfoSchema for the metadata
/// lock of the cache's preprocessing (`domain.GetDomain(sctx).InfoSchema()`).
// go: OptimizeExecStmt L2 (Build has no EXECUTE arm yet: buildExecute is called directly)
pub fn optimize_exec_stmt(
    ctx: &mut PlanCtx<'_>,
    exec_ast: &ExecuteStmt,
    is: &dyn InfoSchemaLookup,
    domain_is: Option<&dyn InfoSchemaLookup>,
) -> Result<(Execute, NameSlice), Error> {
    let mut builder = new_plan_builder(&[]);
    builder.init(ctx.vars, Some(InfoSchemaHandle(is.snapshot())), None);

    reset_for_build(ctx.vars);
    let mut exec = builder.build_execute(ctx.vars, exec_ast)?;
    {
        let prep_stmt = exec.prep_stmt.lock();
        let tables = get_db_table_info(&prep_stmt_visit_infos(&prep_stmt));
        ctx.vars.stmt_ctx_mut().tables = tables;
    }
    let parts = SessionPlanCtx::of(ctx);
    let prep_stmt = Arc::clone(&exec.prep_stmt);
    let mut prep_stmt = prep_stmt.lock();
    let (plan, names) = get_plan_from_plan_cache(
        ctx.vars,
        false,
        is,
        domain_is,
        &mut prep_stmt,
        &exec.params,
        &mut |vars, node, is| optimize_ast_node(&parts, vars, node, is),
    )?;
    exec.plan = Some(plan);
    exec.output_names = names.clone();
    Ok((exec, names))
}

/// `exec.PrepStmt.VisitInfos`: `PlanCacheStmt` does not keep the visit infos yet (D13.1:
/// they feed the privilege check of a cached plan, and there is no privilege manager).
// glue: PlanCacheStmt.VisitInfos (not kept by the Rust PlanCacheStmt)
fn prep_stmt_visit_infos(
    _prep_stmt: &crate::core::plan_cache_utils::PlanCacheStmt,
) -> Vec<crate::core::planbuilder::VisitInfo> {
    Vec::new()
}

/// The resets of `buildLogicalPlan` before `Build`: the plan and column ids,
/// `MapScalarSubQ`, `MapHashCode2UniqueID4ExtendedCol` (not on the Rust session: only
/// subqueries and the extended columns of window functions use it) and the rewrite phase
/// information.
// go-part: buildLogicalPlan (the resets before Build) L1
fn reset_for_build(vars: &mut SessionVars) {
    vars.state.plan_id.0.store(0, Ordering::SeqCst);
    vars.state.plan_column_id.0.store(0, Ordering::SeqCst);
    vars.state.map_scalar_sub_q = Vec::new();
    // failpoint mockRandomPlanID: not ported (no failpoints).

    // reset fields about rewrite (RewritePhaseInfo.Reset: the fields are flattened into the
    // session variables)
    vars.duration_rewrite = 0;
    vars.duration_preprocess_sub_query = 0;
    vars.preprocess_sub_queries = 0;
}

/// `buildLogicalPlan`: `PlanBuilder.Build` after the resets, then the statement's table
/// list. The `Execute` branch of the table list is [`optimize_exec_stmt`]'s (Build has no
/// EXECUTE arm yet).
// go: buildLogicalPlan L2 (the Execute branch is optimize_exec_stmt's)
pub fn build_logical_plan<'ast>(
    ctx: &mut PlanCtx<'_>,
    node: &'ast NodeW<'_>,
    builder: &mut PlanBuilder<'ast>,
) -> Result<crate::PlanId, Error> {
    reset_for_build(ctx.vars);
    let begin_rewrite = std::time::Instant::now();
    let p = builder.build(ctx, node.node)?;
    ctx.vars.duration_rewrite = begin_rewrite.elapsed().as_nanos() as i64;
    ctx.vars.stmt_ctx_mut().tables = get_db_table_info(builder.get_visit_info());
    Ok(p)
}

/// `setVarHintChecker` checks a `SET_VAR` hint: the variable exists and may be set by a
/// hint (a warning otherwise).
// go: setVarHintChecker L1 [M-ERRVAL]
pub fn set_var_hint_checker(var_name: &str, hint: &str) -> (bool, Option<Error>) {
    let Some(sys_var) = get_sys_var(var_name) else {
        // no such a variable
        return (
            false,
            Some(ErrUnresolvedHintName.fast_gen_by_args(vec![
                Arg::go_string(var_name.as_bytes().to_vec()),
                Arg::go_string(hint.as_bytes().to_vec()),
            ])),
        );
    };
    let mut warning = None;
    if !sys_var.is_hint_updatable_verified {
        warning = Some(
            ErrNotHintUpdatable
                .fast_gen_by_args(vec![Arg::go_string(var_name.as_bytes().to_vec())]),
        );
    }
    (true, warning)
}

/// `hypoIndexChecker(ctx, is)`: the column offset of a `HYPO_INDEX` hint's column. Only
/// `ParseStmtHints` calls it, for a `HYPO_INDEX` hint, which the Rust parser does not parse
/// (D02.2): never called.
// glue: stub of hypoIndexChecker, blocked-by:D02.2 (only HYPO_INDEX hints call it; the parser's hint grammar is not ported)
fn hypo_index_checker(db: &CIStr, tbl: &CIStr, col: &CIStr) -> Result<isize, Error> {
    let _ = (db, tbl, col);
    Err(blocked("D02.2", "HYPO_INDEX hints (hypoIndexChecker)"))
}

// Go's `init` stores `Optimize` / `IsReadOnly` into function variables of `core`,
// `indexadvisor` and `bindinfo` (import cycles); the crate calls them directly.
// go: init L2 (cross-package function variables; the crate calls the functions directly)
