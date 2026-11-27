//! go-file: pkg/planner/core/plan_cache.go
//! go-file: pkg/planner/core/plan_cache_param.go
//!
//! The entry of the plan cache for EXECUTE (block D13.1): the parameters, the schema check
//! before every execution, the cache key, the lookup in the instance-level plan cache and
//! the rebuild of a hit, and `generateNewPlan`, which calls the optimizer and puts the plan
//! into the cache. The cache is the plan-cache exception A3
//! (`specs/exceptions/A3-plan-cache/spec.md`, first stage): only the instance-level cache
//! exists, the cached plan is shared without a clone, and a hit copies only the nodes whose
//! ranges change.
//!
//! `OptimizeAstNode` is a package variable in Go (set by `pkg/planner` at init); here it is
//! the `optimize_ast_node` argument, `crate::optimize::optimize_ast_node` in production.

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::blocked_by;
use tidb_base::contextutil::{SessionNonPrepared, SessionPrepared};
use tidb_base::dbterror::plannererrors::{ErrSchemaChanged, ErrWrongParamCount};
use tidb_base::gofmt::Arg;
use tidb_parser::ast::Stmt;
use tidb_sessionctx::variable::session::SessionVars;
use tidb_types::datum::Datum;
use tidb_types::field_name::NameSlice;

use tidb_parser::types::FieldType;

use crate::PhysicalPlanRef;
use crate::core::plan_cache_instance::InstancePlanCacheImpl;
use crate::core::plan_cache_rebuild::rebuild_plan4_cached_plan;
use crate::core::plan_cache_utils::{
    InfoSchemaLookup, PlanCacheKeyEnv, PlanCacheStmt, PlanCacheValue, new_plan_cache_key,
    new_plan_cache_value, parse_param_types,
};
use crate::core::plan_cacheable_checker::is_plan_cacheable;
use crate::core::preprocess::{
    InfoSchemaHandle, PreprocessOpt, PreprocessorReturn, in_prepare, preprocess,
    try_lock_mdl_and_update_schema_if_necessary, with_domain_info_schema, with_preprocessor_return,
};
use crate::core::resolve::{NodeW, new_node_w, new_node_w_with_ctx};
use crate::planctx::SessionPlanCtx;

/// Go's `OptimizeAstNode` function variable (see the module doc).
pub type OptimizeAstNodeFn<'f> = &'f mut dyn FnMut(
    &mut SessionVars,
    &mut NodeW<'_>,
    &dyn InfoSchemaLookup,
) -> Result<(PhysicalPlanRef, NameSlice), Error>;

/// SetParameterValuesIntoSCtx sets these parameters into session context.
///
/// `params` are the values of Go's `[]expression.Expression`: the binary protocol's arguments
/// are constants, whose `Eval` is the value. `EXECUTE ... USING @var` (text protocol, a
/// `GetVar` function, and its `isGetVarBinaryLiteral`) needs the expression rewriter (D09.2). The markers are
/// not written (plan_cache_utils module doc), and the debug trace of the parameters is not
/// ported (optimizer debug trace, D09.13).
// go: SetParameterValuesIntoSCtx L2 (constants only: EXECUTE ... USING needs the expression rewriter)
pub fn set_parameter_values_into_sctx(vars: &mut SessionVars, is_non_prep: bool, params: &[Datum]) {
    let mut plan_cache_params = vars.state.plan_cache_params.lock();
    plan_cache_params.reset();
    for usage_param in params {
        let val = usage_param.clone();
        plan_cache_params.append(std::slice::from_ref(&val));
    }
    plan_cache_params.set_for_non_prep_cache(is_non_prep);
}

/// planCachePreprocess: the parameter count and values, the metadata lock and schema version
/// of each table, and a new preprocess when the schema changed.
///
/// Step 5 (`admin flush instance plan_cache` expiration) deletes cached plans: there are none
/// (A3). Step 6 (`TblInfo2UnionScan` of dirty tables) is the write path: a read-only session
/// has no dirty table.
// go: planCachePreprocess L2 (step 5 deletes cached plans: A3; step 6 is the write path)
pub fn plan_cache_preprocess(
    vars: &mut SessionVars,
    is_non_prepared: bool,
    is: &dyn InfoSchemaLookup,
    domain_is: Option<&dyn InfoSchemaLookup>,
    pm: Option<&tidb_privilege::PrivilegeManagerRef>,
    stmt: &mut PlanCacheStmt,
    params: &[Datum],
) -> Result<(), Error> {
    vars.stmt_ctx_mut().stmt_type = stmt.prepared_ast.stmt_type.clone();

    // step 1: check parameter number
    if stmt.params.len() != params.len() {
        return Err(ErrWrongParamCount.clone());
    }

    // step 2: set parameter values
    set_parameter_values_into_sctx(vars, is_non_prepared, params);

    // step 3: add metadata lock and check each table's schema version
    let mut schema_not_match = false;
    for i in 0..stmt.db_name.len() {
        let tbl = match is.table_by_id(stmt.tbls[i].id)? {
            Some(tbl) => tbl,
            None => {
                let tbl_by_name = is
                    .table_by_name(&stmt.db_name[i], &stmt.tbls[i].name)
                    .map_err(|err| {
                        if blocked_by(&err).is_some() {
                            return err;
                        }
                        schema_change_caused_error(&err)
                    })?;
                // Table ID is changed, for example, drop & create table, truncate table.
                stmt.relate_version.remove(&stmt.tbls[i].id);
                tbl_by_name
            }
        };
        // newTbl is the 'should be used' table info for this execution.
        let new_tbl = match try_lock_mdl_and_update_schema_if_necessary(
            vars,
            &stmt.db_name[i],
            Arc::clone(&tbl),
            is,
            domain_is,
        ) {
            Ok(t) => t,
            Err(err) => {
                // A boundary stub stops the statement (the PoC answers) instead of being
                // logged like Go's errors (D13.1 spec §3.3).
                if blocked_by(&err).is_some() {
                    return Err(err);
                }
                log::warn!(
                    "meet error during tryLockMDLAndUpdateSchemaIfNecessary, table name: {}, error: {err}",
                    tbl.name.o
                );
                // Invalid the cache key related fields to avoid using plan cache.
                stmt.relate_version.insert(tbl.id, u64::MAX);
                schema_not_match = true;
                continue;
            }
        };
        if stmt.tbls[i].revision != new_tbl.revision {
            schema_not_match = true;
        }
        // Update the cache key related fields.
        stmt.relate_version.insert(new_tbl.id, new_tbl.revision);
        stmt.tbls[i] = new_tbl;
    }

    // step 4: check schema version
    if schema_not_match || stmt.schema_version != is.schema_meta_version() {
        // In order to avoid some correctness issues, we have to clear the
        // cached plan once the schema version is changed.
        // Cached plan in prepared struct does NOT have a "cache key" with
        // schema version like prepared plan cache key
        stmt.point_get.executor = None;
        stmt.point_get.column_infos = None;
        // If the schema version has changed we need to preprocess it again,
        // if this time it failed, the real reason for the error is schema changed.
        // Example:
        // When running update in prepared statement's schema version distinguished from the one of execute statement
        // We should reset the tableRefs in the prepared update statements, otherwise, the ast nodes still hold the old
        // tableRefs columnInfo which will cause chaos in logic of trying point get plan. (should ban non-public column)
        let ret = PreprocessorReturn {
            info_schema: Some(InfoSchemaHandle(is.snapshot())),
            ..Default::default()
        };
        let mut node_w = new_node_w(&mut stmt.prepared_ast.stmt);
        node_w.set_text(&stmt.stmt_text);
        let mut opts: Vec<PreprocessOpt> =
            vec![Box::new(in_prepare), with_preprocessor_return(ret)];
        if let Some(d) = domain_is {
            opts.push(with_domain_info_schema(d.snapshot()));
        }
        if let Some(pm) = pm {
            opts.push(crate::core::preprocess::with_privilege_manager(Arc::clone(
                pm,
            )));
        }
        let (_, res) = preprocess(vars, &mut node_w, opts);
        if let Err(err) = res {
            if blocked_by(&err).is_some() {
                return Err(err);
            }
            return Err(schema_change_caused_error(&err));
        }
        stmt.resolve_ctx = node_w.get_resolve_context();
        stmt.schema_version = is.schema_meta_version();
    }

    // step 5: handle expiration: no cached plan to delete (A3).
    // step 6: initialize the tableInfo2UnionScan: no dirty table on a read-only session.
    Ok(())
}

/// `plannererrors.ErrSchemaChanged.GenWithStack("Schema change caused error: %s", err.Error())`.
// glue: the error of planCachePreprocess
fn schema_change_caused_error(err: &Error) -> Error {
    ErrSchemaChanged.gen_with_stack(
        "Schema change caused error: %s",
        vec![Arg::from(
            String::from_utf8_lossy(&err.error()).into_owned(),
        )],
    )
}

/// GetPlanFromPlanCache is the entry point of Plan Cache.
/// It tries to get a valid cached plan from plan cache.
/// If there is no such a plan, it'll call the optimizer to generate a new one.
/// isNonPrepared indicates whether to use the non-prepared plan cache or the prepared plan cache.
///
/// `parts` is the statement's planning context (Go's `sctx`: the expression context the
/// rebuild evaluates the parameters in, and the domain's instance plan cache). The only
/// cache is the instance-level one (the plan-cache exception A3).
// go: GetPlanFromPlanCache L2 (the planning context is passed as its parts; only the instance cache, A3 spec §1.2)
#[allow(clippy::too_many_arguments)]
pub fn get_plan_from_plan_cache(
    parts: &SessionPlanCtx,
    vars: &mut SessionVars,
    is_non_prepared: bool,
    is: &dyn InfoSchemaLookup,
    domain_is: Option<&dyn InfoSchemaLookup>,
    stmt: &mut PlanCacheStmt,
    params: &[Datum],
    optimize_ast_node: OptimizeAstNodeFn<'_>,
) -> Result<(PhysicalPlanRef, NameSlice), Error> {
    plan_cache_preprocess(
        vars,
        is_non_prepared,
        is,
        domain_is,
        parts.privilege_manager.as_ref(),
        stmt,
        params,
    )?;

    let cache_enabled;
    {
        let tracker = &vars.stmt_ctx().plan_cache_tracker;
        if is_non_prepared {
            tracker.set_cache_type(SessionNonPrepared);
            cache_enabled = vars.enable_non_prepared_plan_cache; // plan-cache might be disabled after prepare.
        } else {
            tracker.set_cache_type(SessionPrepared);
            cache_enabled = vars.enable_prepared_plan_cache;
        }
        if stmt.stmt_cacheable && cache_enabled {
            tracker.enable_plan_cache();
        }
        if !stmt.uncacheable_reason.is_empty() {
            tracker.warn_skip_plan_cache(&stmt.uncacheable_reason);
        }
    }

    let mut binding = String::new();
    if vars.stmt_ctx().plan_cache_tracker.use_cache() {
        let (b, cacheable, reason) =
            new_plan_cache_key(vars, stmt, &PlanCacheKeyEnv { domain_is })?;
        binding = b;
        if !cacheable {
            vars.stmt_ctx()
                .plan_cache_tracker
                .set_skip_plan_cache(&reason);
        }
    }

    let mut param_types = Vec::with_capacity(params.len());
    parse_param_types(params, &mut param_types);
    if vars.stmt_ctx().plan_cache_tracker.use_cache()
        && let Some(cached_val) = lookup_plan_cache(parts, stmt, &param_types)
        && let Some(res) = adjust_cached_plan(
            parts,
            vars,
            is,
            &cached_val,
            is_non_prepared,
            &binding,
            stmt,
        )?
    {
        return Ok(res);
    }

    generate_new_plan(
        parts,
        vars,
        is_non_prepared,
        is,
        stmt,
        &param_types,
        optimize_ast_node,
    )
}

/// instancePlanCacheEnabled: `tidb_enable_instance_plan_cache` (default ON in tidb-rs, A3
/// decision 1). Go's test context key `PlanCacheKeyEnableInstancePlanCache` is not needed:
/// tests set the variable.
// go: instancePlanCacheEnabled L2 (no test context key)
pub fn instance_plan_cache_enabled() -> bool {
    tidb_sessionctx::variable::tidb_vars::EnableInstancePlanCache
        .load(std::sync::atomic::Ordering::SeqCst)
}

/// The domain's instance plan cache when it is enabled (`None`: off, or no domain).
// glue: domain.GetDomain(sctx).GetInstancePlanCache() with instancePlanCacheEnabled
fn instance_plan_cache(parts: &SessionPlanCtx) -> Option<&InstancePlanCacheImpl> {
    if !instance_plan_cache_enabled() {
        return None;
    }
    parts
        .instance_plan_cache
        .as_ref()?
        .as_any()
        .downcast_ref::<InstancePlanCacheImpl>()
}

/// lookupPlanCache: the instance cache's plan for the key and parameter types. With the
/// instance cache off nothing is cached (A3 decision 2: there is no session-level cache).
/// The hit is not cloned: the plan is immutable and shared (A3 spec §3.2).
// go: lookupPlanCache L2 (no clone; no session-level cache, A3 decision 2)
fn lookup_plan_cache(
    parts: &SessionPlanCtx,
    stmt: &PlanCacheStmt,
    param_types: &[FieldType],
) -> Option<Arc<PlanCacheValue>> {
    instance_plan_cache(parts)?.get(&stmt.cache_key, param_types)
}

/// adjustCachedPlan: the privilege check, the rebuild of the ranges for the current
/// parameters (path copying, A3 spec §3.2), then what a hit records. `None`: the cached plan
/// cannot be used (the caller optimizes a new one). The plan digest (`SetPlanDigest`) is not
/// kept: `NormalizePlan` is not ported (plan digests, D14.x), as on the miss path.
// go: adjustCachedPlan L2 (the plan digest is not kept, as generateNewPlan)
#[allow(clippy::too_many_arguments)]
fn adjust_cached_plan(
    parts: &SessionPlanCtx,
    vars: &mut SessionVars,
    is: &dyn InfoSchemaLookup,
    cached_val: &PlanCacheValue,
    is_non_prepared: bool,
    bind_sql: &str,
    stmt: &PlanCacheStmt,
) -> Result<Option<(PhysicalPlanRef, NameSlice)>, Error> {
    vars.found_in_plan_cache = false;
    check_prepared_priv(vars, is, parts.privilege_manager.as_deref(), stmt)?;
    let plan = {
        let mut ctx = parts.plan_ctx(vars);
        match rebuild_plan4_cached_plan(&mut ctx, &cached_val.plan)? {
            Some(p) => p,
            None => return Ok(None),
        }
    };
    vars.found_in_plan_cache = true;
    if !bind_sql.is_empty() {
        // We're using binding, set this to true.
        vars.found_in_binding = true;
    }
    if !is_non_prepared {
        tidb_metrics::plan_cache_hit_prepared();
    }
    vars.stmt_ctx_mut().stmt_hints = cached_val.stmt_hints.clone_hints();
    Ok(Some((plan, cached_val.output_columns.clone())))
}

/// generateNewPlan call the optimizer to generate a new plan for current statement
/// and try to add it to cache
///
/// The plan goes into the instance cache as it is (no clone: it is sealed and shared, A3
/// spec §3.3); a plan with an operator the instance cache cannot hold is silently not cached
/// (Go's failed `CloneForPlanCache`). The flush generation is read before optimizing (A3
/// spec §3.4). `NormalizePlan` and the plan digest are not ported (D14.x); `SetPlan` is
/// done by `Compile` for the final plan. The prepared statement's AST keeps spans into the
/// text PREPARE parsed (`StmtText`), which the node wrapper carries to the planner (Go's
/// nodes keep their own text).
// go: generateNewPlan L2 (no clone; the flush generation; NormalizePlan / SetPlanDigest are not ported; the node carries the statement text)
pub fn generate_new_plan(
    parts: &SessionPlanCtx,
    vars: &mut SessionVars,
    is_non_prepared: bool,
    is: &dyn InfoSchemaLookup,
    stmt: &mut PlanCacheStmt,
    param_types: &[FieldType],
    optimize_ast_node: OptimizeAstNodeFn<'_>,
) -> Result<(PhysicalPlanRef, NameSlice), Error> {
    if !is_non_prepared {
        tidb_metrics::plan_cache_miss_prepared();
    }
    let flush_gen = instance_plan_cache(parts).map(|c| c.flush_generation());
    vars.stmt_ctx_mut().in_prepared_plan_building = true;
    let resolve_ctx = std::sync::Arc::clone(&stmt.resolve_ctx);
    let stmt_ast: &mut Stmt = &mut stmt.prepared_ast.stmt;
    let mut node_w = new_node_w_with_ctx(stmt_ast, resolve_ctx);
    node_w.set_text(&stmt.stmt_text);
    let res = optimize_ast_node(vars, &mut node_w, is);
    vars.stmt_ctx_mut().in_prepared_plan_building = false;
    let (p, names) = res?;

    // check whether this plan is cacheable.
    if vars.stmt_ctx().plan_cache_tracker.use_cache() {
        let (cacheable, reason) =
            is_plan_cacheable(vars, &p, param_types.len(), stmt.limits, stmt.has_subquery)?;
        if !cacheable {
            vars.stmt_ctx()
                .plan_cache_tracker
                .set_skip_plan_cache(&reason);
        }
    }

    // put this plan into the plan cache.
    if vars.stmt_ctx().plan_cache_tracker.use_cache()
        && let (Some(cache), Some(flush_gen)) = (instance_plan_cache(parts), flush_gen)
        && let Some(cached) =
            new_plan_cache_value(&p, &names, param_types, &vars.stmt_ctx().stmt_hints)
    {
        cache.put(&stmt.cache_key, Arc::new(cached), param_types, flush_gen);
    }
    vars.found_in_plan_cache = false;
    Ok((p, names))
}

/// The parameterization of the non-prepared plan cache (`plan_cache_param.go`): the new stack
/// never uses the non-prepared plan cache.
// glue: stub of GetParamSQLFromAST, blocked-by:D13.1 (the non-prepared plan cache is not used by the new stack)
// glue: stub of ParameterizeAST, blocked-by:D13.1 (the non-prepared plan cache is not used by the new stack)
// glue: stub of Params2Expressions, blocked-by:D13.1 (the non-prepared plan cache is not used by the new stack)
// glue: stub of ParseParameterizedSQL, blocked-by:D13.1 (the non-prepared plan cache is not used by the new stack)
// glue: stub of RestoreASTWithParams, blocked-by:D13.1 (the non-prepared plan cache is not used by the new stack)
// glue: stub of paramReplacer.Enter, blocked-by:D13.1 (the non-prepared plan cache is not used by the new stack)
// glue: stub of paramReplacer.Leave, blocked-by:D13.1 (the non-prepared plan cache is not used by the new stack)
// glue: stub of paramReplacer.Reset, blocked-by:D13.1 (the non-prepared plan cache is not used by the new stack)
// glue: stub of paramRestorer.Enter, blocked-by:D13.1 (the non-prepared plan cache is not used by the new stack)
// glue: stub of paramRestorer.Leave, blocked-by:D13.1 (the non-prepared plan cache is not used by the new stack)
// glue: stub of paramRestorer.Reset, blocked-by:D13.1 (the non-prepared plan cache is not used by the new stack)
pub fn parameterize_ast() -> Result<(), Error> {
    Err(tidb_base::blocked::blocked(
        "D13.1",
        "the non-prepared plan cache",
    ))
}

/// checkPreparedPriv checks the privilege of the prepared statement against the session's live
/// `PrivilegeManager` and current `active_roles` (no cached boolean decision,
/// `specs/D04.1-privilege/spec.md` §5.2 item 8). `CheckTableLock` belongs to the write path.
// go: checkPreparedPriv L2 (CheckTableLock: write path)
pub fn check_prepared_priv(
    vars: &SessionVars,
    is: &dyn InfoSchemaLookup,
    pm: Option<&dyn tidb_privilege::PrivilegeManager>,
    stmt: &PlanCacheStmt,
) -> Result<(), Error> {
    if let Some(pm) = pm {
        let visit_info = crate::core::optimizer::visit_info4_priv_check(
            is,
            Some(&stmt.prepared_ast.stmt),
            &stmt.visit_infos,
        );
        crate::core::optimizer::check_privilege(&vars.active_roles, pm, &visit_info)?;
    }
    Ok(())
}

/// IsSafeToReusePointGetExecutor checks whether this is a PointGet Plan and safe to reuse its executor.
///
/// Go reads the stale-read flag through `staleread.IsStmtStaleness(sctx)`, which is the
/// statement context's `IsStaleness`; the planner reads it from the session variables
/// directly (`sessiontxn` is above the planner). `Compile` calls this for the point plan
/// short path (D05.3 spec §10.1), whose `ExecStmt.PointGet` reuses the executor kept in the
/// prepared statement.
// go: IsSafeToReusePointGetExecutor L1
pub fn is_safe_to_reuse_point_get_executor(
    vars: &SessionVars,
    is: &dyn InfoSchemaLookup,
    stmt: &PlanCacheStmt,
) -> bool {
    if vars.stmt_ctx().is_staleness {
        return false;
    }
    // check auto commit
    if !crate::core::common_plans::is_auto_commit_txn(vars) {
        return false;
    }
    if stmt.schema_version != is.schema_meta_version() {
        return false;
    }
    true
}
