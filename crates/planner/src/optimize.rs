//! go-file: pkg/planner/optimize.go
//!
//! Go package `pkg/planner`: `Optimize`, the entry point the compiler calls. The VS1-10 part
//! keeps the driver's shape; the steps it drives are other blocks and are boundary stubs
//! until they are ported (VS1 plan §5: the non-fast path returns `Blocked` before Phase 2):
//!
//! - `Optimize`: hint extraction and the hint / binding handling first (D13.3 / D13.2), the
//!   fast plan (`TryFastPlan`, D10.8, VS1-11), the plan cache (D13.1, VS1-15), then
//!   `optimize`;
//! - `optimize` / `buildLogicalPlan`: `PlanBuilder.Build` (D09.1, VS1-21) and `DoOptimize`
//!   (D09.13 core part, VS1-24);
//! - `OptimizeExecStmt`: the prepared statement path (D13.1, VS1-15).

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_base::dbterror::plannererrors::{ErrNotHintUpdatable, ErrUnresolvedHintName};
use tidb_base::gofmt::Arg;
use tidb_parser::ast::Stmt;
use tidb_sessionctx::variable::variable::get_sys_var;

/// IsReadOnly check whether the ast.Node is a read only statement.
///
/// The EXECUTE branch reads the prepared statement (D13.1) and the rest is `ast.IsReadOnly`
/// of the parser (D02.2).
// glue: stub of IsReadOnly, blocked-by:D02.2 (ast.IsReadOnly; EXECUTE: D13.1)
pub fn is_read_only(node: &Stmt) -> Result<bool, Error> {
    let _ = node;
    Err(blocked("D02.2", "IsReadOnly (ast.IsReadOnly)"))
}

/// Optimize does optimization and creates a Plan.
// glue: stub of Optimize, blocked-by:D13.3 (VS1-24: hint extraction comes first; then D13.2 bindings, D10.8 TryFastPlan, D13.1 plan cache)
pub fn optimize_stmt(node: &Stmt) -> Result<(), Error> {
    let _ = node;
    Err(blocked("D13.3", "Optimize (statement hints)"))
}

/// OptimizeExecStmt to handle the "execute" statement
// glue: stub of OptimizeExecStmt, blocked-by:D13.1 (VS1-15: PlanCacheStmt, GetPlanFromPlanCache)
pub fn optimize_exec_stmt() -> Result<(), Error> {
    Err(blocked("D13.1", "OptimizeExecStmt (plan cache)"))
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
