//! go-file: pkg/planner/core/plan_cache.go
//! go-file: pkg/planner/core/plan_cache_param.go
//!
//! The entry of the plan cache for EXECUTE (block D13.1), the miss path only
//! (prepared-statement planning, VS1-15): the parameters, the schema check before every
//! execution, and `generateNewPlan`, which calls the optimizer. The new stack caches nothing:
//! the cache (the key, the lookup, adjusting a cached plan, putting a plan into the cache) is
//! the plan-cache exception A3, whose spec is not confirmed, and every lookup is a miss.
//! These boundary stubs (`blocked-by:D13.1`, naming A3) behave like a session
//! whose cache is always empty; the observable difference to Go is that a repeated EXECUTE is
//! never `last_plan_from_cache` (spec §4).
//!
//! `OptimizeAstNode` is a package variable in Go (set by `pkg/planner` at init); here it is
//! the `optimize_ast_node` argument, `crate::optimize::optimize_ast_node` in production.

use tidb_base::Error;
use tidb_base::blocked::blocked_by;
use tidb_base::contextutil::{SessionNonPrepared, SessionPrepared};
use tidb_base::dbterror::plannererrors::{ErrSchemaChanged, ErrWrongParamCount};
use tidb_base::gofmt::Arg;
use tidb_parser::ast::Stmt;
use tidb_sessionctx::variable::session::SessionVars;
use tidb_types::datum::Datum;
use tidb_types::field_name::NameSlice;

use crate::PhysicalPlanRef;
use crate::core::plan_cache_utils::{InfoSchemaLookup, PlanCacheStmt, new_plan_cache_key};
use crate::core::plan_cacheable_checker::is_plan_cacheable;
use crate::core::preprocess::{
    PreprocessOpt, PreprocessorReturn, in_prepare, preprocess,
    try_lock_mdl_and_update_schema_if_necessary, with_preprocessor_return,
};
use crate::core::resolve::{NodeW, new_node_w, new_node_w_with_ctx};

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
/// `GetVar` function, and its `isGetVarBinaryLiteral`) is not in the Rust AST. The markers are
/// not written (plan_cache_utils module doc), and the debug trace of the parameters is not
/// ported (optimizer debug trace, D09.13).
// go: SetParameterValuesIntoSCtx L2 (constants only: EXECUTE ... USING is not in the Rust AST)
pub fn set_parameter_values_into_sctx(vars: &mut SessionVars, is_non_prep: bool, params: &[Datum]) {
    vars.state.plan_cache_params.reset();
    for usage_param in params {
        let val = usage_param.clone();
        vars.state
            .plan_cache_params
            .append(std::slice::from_ref(&val));
    }
    vars.state
        .plan_cache_params
        .set_for_non_prep_cache(is_non_prep);
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
        if let Err(err) = try_lock_mdl_and_update_schema_if_necessary() {
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
        // tryLockMDLAndUpdateSchemaIfNecessary returns the table to use; the stub returns
        // before it can.
        let new_tbl = tbl;
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
            has_info_schema: true,
            ..Default::default()
        };
        let mut node_w = new_node_w(&mut stmt.prepared_ast.stmt);
        let opts: Vec<PreprocessOpt> = vec![Box::new(in_prepare), with_preprocessor_return(ret)];
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
/// The cache is not ported (A3, see the module doc): `NewPlanCacheKey`, `parseParamTypes`
/// (the key's parameter types) and `lookupPlanCache` are boundary stubs, the lookup always
/// misses.
// go: GetPlanFromPlanCache L2 (the cache key and lookup are the plan-cache exception A3: always a miss)
pub fn get_plan_from_plan_cache(
    vars: &mut SessionVars,
    is_non_prepared: bool,
    is: &dyn InfoSchemaLookup,
    stmt: &mut PlanCacheStmt,
    params: &[Datum],
    optimize_ast_node: OptimizeAstNodeFn<'_>,
) -> Result<(PhysicalPlanRef, NameSlice), Error> {
    plan_cache_preprocess(vars, is_non_prepared, is, stmt, params)?;

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

    if vars.stmt_ctx().plan_cache_tracker.use_cache() {
        // NewPlanCacheKey is a stand-in (plan_cache_utils): always cacheable.
        let (_cache_key, _binding, cacheable, reason) = new_plan_cache_key(stmt)?;
        if !cacheable {
            vars.stmt_ctx()
                .plan_cache_tracker
                .set_skip_plan_cache(&reason);
        }
    }

    // paramTypes := parseParamTypes(sctx, params): the key's parameter types (A3); only the
    // count is used below.
    let param_types = params.len();
    if vars.stmt_ctx().plan_cache_tracker.use_cache() {
        // cachedVal, hit := lookupPlanCache(ctx, sctx, cacheKey, paramTypes); a hit goes to
        // adjustCachedPlan.
        // glue: stub of lookupPlanCache, blocked-by:D13.1 (the cache is the plan-cache exception A3: always a miss; adjustCachedPlan is not reached)
    }

    generate_new_plan(
        vars,
        is_non_prepared,
        is,
        stmt,
        param_types,
        optimize_ast_node,
    )
}

/// generateNewPlan call the optimizer to generate a new plan for current statement
/// and try to add it to cache
///
/// The miss counter (`core_metrics.GetPlanCacheMissCounter`, D14.1) is not ported; putting
/// the plan into the cache (`NewPlanCacheValue`, `NormalizePlan`, the session or instance
/// cache's `Put`) is A3.
// go: generateNewPlan L2 (putting the plan into the cache: the plan-cache exception A3; the miss metric: D14.1)
pub fn generate_new_plan(
    vars: &mut SessionVars,
    is_non_prepared: bool,
    is: &dyn InfoSchemaLookup,
    stmt: &mut PlanCacheStmt,
    param_types: usize,
    optimize_ast_node: OptimizeAstNodeFn<'_>,
) -> Result<(PhysicalPlanRef, NameSlice), Error> {
    let _ = is_non_prepared;
    vars.stmt_ctx_mut().in_prepared_plan_building = true;
    let resolve_ctx = std::sync::Arc::clone(&stmt.resolve_ctx);
    let stmt_ast: &mut Stmt = &mut stmt.prepared_ast.stmt;
    let mut node_w = new_node_w_with_ctx(stmt_ast, resolve_ctx);
    let res = optimize_ast_node(vars, &mut node_w, is);
    vars.stmt_ctx_mut().in_prepared_plan_building = false;
    let (p, names) = res?;

    // check whether this plan is cacheable.
    if vars.stmt_ctx().plan_cache_tracker.use_cache() {
        let (cacheable, reason) =
            is_plan_cacheable(vars, &p, param_types, stmt.limits, stmt.has_subquery)?;
        if !cacheable {
            vars.stmt_ctx()
                .plan_cache_tracker
                .set_skip_plan_cache(&reason);
        }
    }

    // put this plan into the plan cache.
    if vars.stmt_ctx().plan_cache_tracker.use_cache() {
        // cached := NewPlanCacheValue(...); stmt.NormalizedPlan, stmt.PlanDigest =
        // NormalizePlan(p); stmtCtx.SetPlan(p); stmtCtx.SetPlanDigest(...); Put(...)
        // glue: stub of the plan cache Put in generateNewPlan, blocked-by:D13.1 (the cache is the plan-cache exception A3: nothing is cached on the new stack)
    }
    vars.found_in_plan_cache = false;
    Ok((p, names))
}

/// The cache-hit path of `GetPlanFromPlanCache`: the cache is the plan-cache exception A3,
/// whose spec is not confirmed (see the module doc); nothing on the new stack reaches these.
// glue: stub of adjustCachedPlan, blocked-by:D13.1 (the cache-hit path: the plan-cache exception A3)
// glue: stub of instancePlanCacheEnabled, blocked-by:D13.1 (the instance plan cache: the plan-cache exception A3)
pub fn adjust_cached_plan() -> Result<(), Error> {
    Err(tidb_base::blocked::blocked(
        "D13.1",
        "the plan cache hit path (A3)",
    ))
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

/// checkPreparedPriv checks the privilege of the prepared statement: no privilege manager
/// (D04.1, the PoC's single-user model) and no table locks (`CheckTableLock`, the write
/// path), as Go with neither.
// go: checkPreparedPriv L2 (no privilege manager: D04.1; CheckTableLock: write path)
pub fn check_prepared_priv(stmt: &PlanCacheStmt) -> Result<(), Error> {
    let _ = stmt;
    Ok(())
}
