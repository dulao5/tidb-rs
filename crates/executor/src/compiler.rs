//! go-file: pkg/executor/compiler.go
//!
//! `Compiler.Compile`: an AST statement to an `ExecStmt` (preprocess, optimize, the
//! transaction's MaxTS / TSO decision). `Compile` runs the planner's `IsReadOnly`,
//! `Preprocess` and `planner.Optimize` over the session's planning context
//! (`SessionContext::get_plan_ctx`, whose expression context is the session's
//! `sessionexpr`), D05.3 spec §9. Where the plan is not ready yet (physical optimization
//! stops at the statistics derivation / FindBestTask boundaries, D10.6 / D10.2), `Optimize`
//! returns the boundary error and the session falls back to the PoC before anything ran
//! (VS1 plan §4.3).

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_base::mysql::NoPriority;
use tidb_parser::ast;
use tidb_planner::core::common_plans::Execute;
use tidb_planner::core::plan_cache_utils::InfoSchemaLookup;
use tidb_planner::core::preprocess::{
    InfoSchemaHandle, PreprocessOpt, PreprocessorReturn, init_txn_context_provider,
    with_domain_info_schema, with_preprocessor_return,
};
use tidb_planner::core::resolve::{NodeW, new_node_w};
use tidb_planner::optimize::{OptimizedPlan, optimize_stmt};
use tidb_types::field_name::FieldName;

use crate::adapter::{ExecStmt, TelemetryInfo};
use crate::glue::{InfoSchemaRef, Plan, PlanCacheStmtRef, SessionContext};

/// Compiler compiles an ast.StmtNode to a physical plan.
///
/// Go keeps the session in the compiler; here `compile` takes it.
#[derive(Default)]
pub struct Compiler;

/// `EXECUTE ... USING @v` (the text protocol, which the router never sends to the new stack)
/// stops where `buildExecute` would (the expression rewriter of the `USING` variables, D09.2),
/// before the preprocessor (D05.3 spec §9.1).
// glue: the USING variables of a text-protocol EXECUTE (D09.2 boundary)
fn check_execute_using(stmt_node: &ast::Stmt) -> Result<(), Error> {
    if let ast::Stmt::Execute(e) = stmt_node
        && !e.using_vars.is_empty()
    {
        // glue: blocked-by:D09.2 (the expression rewriter, b.rewrite of EXECUTE ... USING)
        return Err(blocked("D09.2", "b.rewrite of EXECUTE ... USING"));
    }
    Ok(())
}

/// `plannercore.Preprocess(ctx, sctx, nodeW, WithPreprocessorReturn(ret),
/// InitTxnContextProvider)`, then the session's `SetReadonlyUserVarMap` that Go's
/// `Preprocess` makes.
///
/// Go's `ensureInfoSchema` reads the transaction manager's InfoSchema when the return value
/// has none; the Rust preprocessor takes it in the return value (as PREPARE hands it in), so
/// the transaction's InfoSchema is set there. The domain's InfoSchema is for the metadata
/// lock (`with_domain_info_schema`).
// glue: Compile's plannercore.Preprocess call (the InfoSchemas are handed in, M-CTX)
fn preprocess(
    sctx: &mut dyn SessionContext,
    node_w: &mut NodeW<'_>,
    txn_is: Option<&InfoSchemaRef>,
    domain_is: Option<&InfoSchemaRef>,
) -> Result<(), Error> {
    let ret = PreprocessorReturn {
        info_schema: txn_is.map(|is| InfoSchemaHandle(is.snapshot())),
        ..Default::default()
    };
    let mut opts: Vec<PreprocessOpt> = vec![
        with_preprocessor_return(ret),
        Box::new(init_txn_context_provider),
    ];
    if let Some(d) = domain_is {
        opts.push(with_domain_info_schema(d.snapshot()));
    }
    let (ret, res) =
        tidb_planner::core::preprocess::preprocess(sctx.get_session_vars_mut(), node_w, opts);
    sctx.set_readonly_user_var_map(ret.readonly_user_vars);
    res
}

/// `plannercore.GetPreparedStmt(execStmt, sessVars)` for an `EXECUTE` statement.
// glue: Compile's `if execStmt, ok := stmtNode.(*ast.ExecuteStmt)` around plannercore.GetPreparedStmt
fn get_prepared_stmt(
    sctx: &dyn SessionContext,
    stmt_node: &ast::Stmt,
) -> Result<Option<PlanCacheStmtRef>, Error> {
    match stmt_node {
        ast::Stmt::Execute(exec_stmt) => Ok(Some(
            tidb_planner::core::plan_cache_utils::get_prepared_stmt(
                exec_stmt,
                sctx.get_session_vars(),
            )?,
        )),
        _ => Ok(None),
    }
}

/// `planner.Optimize(ctx, c.Ctx, nodeW, is)` over the session's planning context
/// (`GetPlanCtx`). The domain's InfoSchema is for the plan cache's metadata lock. The
/// transaction's InfoSchema is always there in Go; a session without one stops at the
/// transaction manager's boundary.
// glue: Compile's planner.Optimize call (the session's planning context, N2)
fn planner_optimize(
    sctx: &mut dyn SessionContext,
    node_w: &mut NodeW<'_>,
    is: Option<&InfoSchemaRef>,
    domain_is: Option<&InfoSchemaRef>,
) -> Result<(OptimizedPlan, Vec<FieldName>), Error> {
    let Some(is) = is else {
        // glue: blocked-by:D05.4 (sessiontxn GetTxnInfoSchema)
        return Err(blocked("D05.4", "the transaction's InfoSchema"));
    };
    let parts = sctx.get_plan_ctx();
    let mut ctx = parts.plan_ctx(sctx.get_session_vars_mut());
    let (p, names) = optimize_stmt(
        &mut ctx,
        node_w,
        is.as_ref(),
        domain_is.map(|d| d.as_ref() as &dyn InfoSchemaLookup),
    )?;
    Ok((p, names.iter().map(|n| (**n).clone()).collect()))
}

impl Compiler {
    /// Compile compiles an ast.StmtNode to a physical plan.
    ///
    /// `text` is `stmtNode.Text()` (the parser AST does not keep the text, the session
    /// passes it). The statement is handed over (D05.3 spec §9.2): Go's preprocessor and
    /// optimizer write to the node they share by pointer; here they write to the statement
    /// `Compile` owns, which is then frozen into the `Arc` of `ExecStmt.StmtNode` (the session
    /// hands the same `Arc` to the transaction manager). An `Execute` plan takes Go's path
    /// where `IsSafeToReusePointGetExecutor` answers false (the reuse is the plan cache's,
    /// A3; D05.3 spec §10).
    // go: Compiler.Compile L2 (panics at the task boundary, M-PANIC; the statement text is passed in; IsSafeToReusePointGetExecutor is the plan cache's, A3)
    pub fn compile(
        &self,
        sctx: &mut dyn SessionContext,
        mut stmt_node: ast::Stmt,
        text: &str,
    ) -> Result<ExecStmt, Error> {
        let is_read_only =
            tidb_planner::optimize::is_read_only(&stmt_node, sctx.get_session_vars())?;
        sctx.get_session_vars_mut().stmt_ctx_mut().is_read_only = is_read_only;

        // Do preprocess and validate.
        check_execute_using(&stmt_node)?;
        let mut node_w = new_node_w(&mut stmt_node);
        let txn_is = sctx.get_txn_info_schema();
        let domain_is = sctx.get_domain_info_schema();
        preprocess(sctx, &mut node_w, txn_is.as_ref(), domain_is.as_ref())?;

        let is = sctx.get_txn_info_schema();
        // handle the execute statement
        let prepared_obj = get_prepared_stmt(sctx, node_w.node)?;
        // Build the final physical plan.
        let (final_plan, names) =
            planner_optimize(sctx, &mut node_w, is.as_ref(), domain_is.as_ref())?;
        drop(node_w);
        // The planner is done writing: from here on the statement is shared (ExecStmt, the
        // transaction manager), D05.3 spec §9.2.
        let stmt_node = Arc::new(stmt_node);

        let (in_restricted_sql, resource_group) = {
            let vars = sctx.get_session_vars();
            (
                vars.in_restricted_sql,
                vars.stmt_ctx().resource_group_name.clone(),
            )
        };
        if let Some(prepared_obj) = &prepared_obj {
            count_stmt_node(
                &prepared_obj.lock().prepared_ast.stmt,
                in_restricted_sql,
                &resource_group,
            );
        } else {
            count_stmt_node(&stmt_node, in_restricted_sql, &resource_group);
        }
        let mut lower_priority = false;
        if sctx.get_session_vars().stmt_ctx().priority == NoPriority {
            lower_priority = need_lower_priority(&final_plan);
        }
        let mut stmt = ExecStmt::new(text.to_owned(), None, is.clone());
        let plan_any: Arc<dyn std::any::Any + Send + Sync> = match final_plan {
            OptimizedPlan::Physical(p) => {
                stmt.plan = Some(Arc::clone(&p));
                Arc::new(p)
            }
            OptimizedPlan::Execute(exec) => {
                // One Execute plan, shared by the statement context and the statement (Go
                // stores the same pointer in both).
                let exec: Arc<Execute> = Arc::from(exec);
                stmt.execute_plan = Some(Arc::clone(&exec));
                exec
            }
        };
        sctx.get_session_vars_mut()
            .stmt_ctx_mut()
            .set_plan(Some(plan_any));
        stmt.lower_priority = lower_priority;
        stmt.stmt_node = Some(stmt_node);
        stmt.output_names = names;
        stmt.ti = Arc::new(TelemetryInfo::default());
        // Use cached plan if possible: Go asks `IsSafeToReusePointGetExecutor` whether the
        // prepared statement's point get may replace an Execute plan (`stmt.PsStmt`, the
        // point plan short path that reuses the cached executor). That reuse belongs to the
        // plan cache (the plan-cache exception A3, not confirmed; the tracker lists the
        // function as not ported), so the statement always takes Go's path where it answers
        // false: `PsStmt` stays nil and `Exec` builds a new executor from the Execute plan
        // (`inheritContextFromExecuteStmt`). The result is the same; only the reuse is
        // missing (spec D05.3 §10).
        let _ = &prepared_obj;

        // `AdviseOptimizeWithPlan` of every transaction context provider looks through an
        // Execute plan to the plan inside it (`if execute, ok := plan.(*plannercore.Execute)`),
        // which is what the session's hook takes.
        let advised = match (&stmt.plan, &stmt.execute_plan) {
            (Some(p), _) => Some(p),
            (None, Some(e)) => e.plan.as_ref(),
            (None, None) => None,
        };
        if let Some(p) = advised {
            sctx.advise_optimize_with_plan_and_then_warm_up(p)?;
        }

        Ok(stmt)
    }
}

/// needLowerPriority checks whether it's needed to lower the execution priority of a query.
/// If the estimated output row count of any operator in the physical plan tree is greater
/// than the specific threshold, we'll set it to lowPriority when sending it to the
/// coprocessor.
///
/// The new stack's plans are physical or `Execute` (no `Insert`, `Delete`, `Update`: the
/// write path).
// go: needLowerPriority L1
fn need_lower_priority(p: &OptimizedPlan) -> bool {
    match p {
        OptimizedPlan::Physical(x) => is_physical_plan_need_lower_priority(x),
        OptimizedPlan::Execute(x) => match &x.plan {
            Some(p) => need_lower_priority(&OptimizedPlan::Physical(Arc::clone(p))),
            None => false,
        },
    }
}

/// `p.StatsCount()` comes with the physical plan base (D10.1, VS1-19): until then no plan is
/// estimated above the threshold.
// go: isPhysicalPlanNeedLowerPriority L2 (StatsCount comes with D10.1: no plan is lowered yet)
fn is_physical_plan_need_lower_priority(p: &Plan) -> bool {
    let expensive_threshold = tidb_base::config::get_global_config()
        .log
        .expensive_threshold as i64;
    // glue: stub, blocked-by:D10.1 (PhysicalPlan.StatsCount)
    let stats_count: i64 = 0;
    if stats_count > expensive_threshold {
        return true;
    }

    for child in p.children() {
        if is_physical_plan_need_lower_priority(child) {
            return true;
        }
    }

    false
}

/// CountStmtNode records the number of statements with the same type.
///
/// The counter (`metrics.StmtNodeCounter`) is D14.1's and the statement label
/// (`stmtctx.GetStmtLabel`) D00.1's: nothing is counted yet.
// go: CountStmtNode L2 (the counter is D14.1)
pub fn count_stmt_node(stmt_node: &ast::Stmt, in_restricted_sql: bool, resource_group: &str) {
    if in_restricted_sql {
        return;
    }
    let _ = (stmt_node, resource_group);
    // glue: stub, blocked-by:D14.1 (metrics.StmtNodeCounter)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::tests::MockSession;

    fn compile_err(sql: &str) -> Error {
        let mut s = MockSession::new(u64::MAX);
        let stmt = tidb_parser::parse(sql, &Default::default())
            .expect("parse")
            .stmts
            .remove(0);
        Compiler.compile(&mut s, stmt, sql).err().unwrap()
    }

    /// A plain statement is handed over and planned (D05.3 spec §9.2): the mock session has
    /// no transaction InfoSchema, so `Optimize` stops at the transaction manager's boundary;
    /// an EXECUTE of an unknown statement is Go's `ErrStmtNotFound` from `planner.IsReadOnly`.
    #[test]
    fn compile_plans_a_plain_statement() {
        let block = |e: &Error| tidb_base::blocked::blocked_by(e).map(str::to_owned);
        assert_eq!(block(&compile_err("select 1")).as_deref(), Some("D05.4"));
        let e = compile_err("execute s");
        assert_eq!(e.code(), 8111, "{e}");
    }
}
