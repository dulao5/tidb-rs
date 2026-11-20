//! go-file: pkg/planner/optimize.go
//!
//! Go package `pkg/planner`: `Optimize`, the entry point the compiler calls. The VS1-10 part
//! keeps the driver's shape; the steps it drives are other blocks and are boundary stubs
//! until they are ported (VS1 plan §5: the non-fast path returns `Blocked` before Phase 2):
//!
//! - `Optimize`: hint extraction and the hint / binding handling first (D13.3 / D13.2), the
//!   fast plan (`TryFastPlan`, D10.8, VS1-11: `core::point_get_plan::try_fast_plan`; Go tries
//!   it only when `tidb_isolation_read_engines` holds TiKV, optimize.go:244, a check of this
//!   driver, not of `TryFastPlan`), the plan cache (D13.1, VS1-15), then `optimize`;
//! - `optimize` / `buildLogicalPlan`: `PlanBuilder.Build` (D09.1, VS1-21) and `DoOptimize`
//!   (D09.13 core part, VS1-24);
//! - `OptimizeExecStmt`: the prepared statement path (D13.1, VS1-15): the `Execute` plan and
//!   the plan cache's miss path, which ends in `OptimizeAstNode` (the statement's `Optimize`).

use std::sync::Arc;
use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_base::dbterror::plannererrors::{ErrNotHintUpdatable, ErrUnresolvedHintName};
use tidb_base::gofmt::Arg;

use tidb_parser::ast::{ExecuteStmt, Stmt};
use tidb_sessionctx::variable::session::SessionVars;
use tidb_types::field_name::NameSlice;

use crate::core::common_plans::Execute;
use crate::core::plan_cache::get_plan_from_plan_cache;
use crate::core::plan_cache_utils::{InfoSchemaLookup, get_prepared_stmt};
use crate::core::planbuilder::new_plan_builder;
use tidb_sessionctx::variable::variable::get_sys_var;

/// IsReadOnly check whether the ast.Node is a read only statement.
///
/// The `EXECUTE` branch is ported; `ast.IsReadOnly` of the parser walks the whole statement
/// (its `readOnlyChecker` looks for user variable assignments) and needs a visitor of the
/// Rust AST, which the parser does not have (D02.2).
// go: IsReadOnly L2 (ast.IsReadOnly is a boundary stub: blocked-by:D02.2)
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

/// `ast.IsReadOnly` (see [`is_read_only`]).
// glue: stub, blocked-by:D02.2 (the parser's ast.IsReadOnly: its readOnlyChecker needs a visitor of the Rust AST)
fn ast_is_read_only(node: &Stmt) -> Result<bool, Error> {
    let _ = node;
    Err(blocked("D02.2", "IsReadOnly (ast.IsReadOnly)"))
}

/// Optimize does optimization and creates a Plan.
///
/// Go handles an `EXECUTE` first (before the hints): that branch is [`optimize_exec_stmt`].
// glue: stub of Optimize, blocked-by:D13.3 (VS1-24: hint extraction comes first; then D13.2 bindings, D10.8 TryFastPlan, D13.1 plan cache)
pub fn optimize_stmt(node: &Stmt) -> Result<(), Error> {
    let _ = node;
    Err(blocked("D13.3", "Optimize (statement hints)"))
}

/// `OptimizeAstNode` (the function the plan cache's miss path calls, `generateNewPlan`):
/// `planner.Optimize` for the prepared statement's AST, a boundary stub like [`optimize_stmt`].
// glue: stub of OptimizeAstNode, blocked-by:D13.3 (planner.Optimize: hint extraction comes first, VS1-24)
pub fn optimize_ast_node(
    vars: &mut tidb_sessionctx::variable::session::SessionVars,
    node: &mut crate::core::resolve::NodeW<'_>,
    is: &dyn crate::core::plan_cache_utils::InfoSchemaLookup,
) -> Result<(crate::PhysicalPlanRef, tidb_types::field_name::NameSlice), Error> {
    let _ = (vars, is);
    optimize_stmt(node.node)?;
    Err(blocked("D13.3", "Optimize (statement hints)"))
}

/// OptimizeExecStmt to handle the "execute" statement
///
/// `buildLogicalPlan` (`PlanBuilder.Build`) returns no plan yet, so its `EXECUTE` arm
/// (`buildExecute`) is called directly; `PlanID` / `PlanColumnID` / `MapScalarSubQ` /
/// `RewritePhaseInfo`, which `buildLogicalPlan` resets first, are not in the session
/// variables yet. `GetPlanFromPlanCache` is the plan cache's miss path (nothing is cached,
/// the plan-cache exception A3); it ends in `OptimizeAstNode`, a boundary stub until
/// `planner.Optimize` is ported. `exec.Stmt` (the prepared AST) stays in `exec.prep_stmt`.
/// `domain_is` is the session's newest InfoSchema for the metadata lock of the cache's
/// preprocessing (`domain.GetDomain(sctx).InfoSchema()`).
// go: OptimizeExecStmt L2 (Build returns no plan yet: buildExecute is called directly)
pub fn optimize_exec_stmt(
    vars: &mut SessionVars,
    exec_ast: &ExecuteStmt,
    is: &dyn InfoSchemaLookup,
    domain_is: Option<&dyn InfoSchemaLookup>,
) -> Result<(Execute, NameSlice), Error> {
    let mut builder = new_plan_builder(&[]);
    builder.init(vars);

    let mut exec = builder.build_execute(vars, exec_ast)?;
    let prep_stmt = Arc::clone(&exec.prep_stmt);
    let mut prep_stmt = prep_stmt.lock();
    let (plan, names) = get_plan_from_plan_cache(
        vars,
        false,
        is,
        domain_is,
        &mut prep_stmt,
        &exec.params,
        &mut optimize_ast_node,
    )?;
    exec.plan = Some(plan);
    exec.output_names = names.clone();
    Ok((exec, names))
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
/// `MapHashCode2UniqueID4ExtendedCol` and `RewritePhaseInfo` between build rounds; the
/// statement-context state and the hint names are not ported yet.
// glue: stub of saveLogicalPlanBuildCtx, blocked-by:D09.13 (VS1-24: StmtCtx.SaveLogicalPlanBuildState)
// glue: stub of restoreLogicalPlanBuildCtx, blocked-by:D09.13 (VS1-24: StmtCtx.RestoreLogicalPlanBuildState)
pub fn save_logical_plan_build_ctx() -> Result<(), Error> {
    Err(blocked("D09.13", "saveLogicalPlanBuildCtx"))
}

/// `buildLogicalPlan` builds the logical plan of a statement (`PlanBuilder.Build`).
// glue: stub of buildLogicalPlan, blocked-by:D09.1 (VS1-21: PlanBuilder.Build of SELECT)
// glue: stub of optimize, blocked-by:D09.1 (VS1-21: buildLogicalPlan; then DoOptimize, VS1-24)
pub fn build_logical_plan() -> Result<(), Error> {
    Err(blocked("D09.1", "buildLogicalPlan"))
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

// Go's `init` stores `Optimize` / `IsReadOnly` into function variables of `core`,
// `indexadvisor` and `bindinfo` (import cycles); the crate calls them directly.
// go: init L2 (cross-package function variables; the crate calls the functions directly)
