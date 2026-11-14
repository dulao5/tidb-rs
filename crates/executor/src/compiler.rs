//! go-file: pkg/executor/compiler.go
//!
//! `Compiler.Compile`: an AST statement to an `ExecStmt` (preprocess, optimize, the
//! transaction's MaxTS / TSO decision). The preprocessor and `planner.Optimize` are VS1-10
//! (D09.0 / D09.13) and the prepared statement cache VS1-15 (D13.1): they are boundary stubs
//! here, so `Compile` fails with `blocked-by:D09.13` until VS1-10 lands, and the session falls
//! back to the PoC (VS1 plan §4.3).

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_base::mysql::NoPriority;
use tidb_parser::ast;
use tidb_types::field_name::FieldName;

use crate::adapter::{ExecStmt, TelemetryInfo};
use crate::glue::{InfoSchemaRef, Plan, PlanCacheStmtRef, SessionContext};

/// Compiler compiles an ast.StmtNode to a physical plan.
///
/// Go keeps the session in the compiler; here `compile` takes it.
#[derive(Default)]
pub struct Compiler;

/// `plannercore.IsReadOnly(stmtNode, vars)`.
// glue: stub, blocked-by:D09.13 (plannercore.IsReadOnly)
fn planner_is_read_only(stmt_node: &ast::Stmt) -> Result<bool, Error> {
    let _ = stmt_node;
    Err(blocked("D09.13", "planner.IsReadOnly"))
}

/// `plannercore.Preprocess(ctx, sctx, nodeW, WithPreprocessorReturn(ret),
/// InitTxnContextProvider)`.
// glue: stub, blocked-by:D09.0 (plannercore.Preprocess, VS1-10)
fn preprocess(sctx: &mut dyn SessionContext, stmt_node: &ast::Stmt) -> Result<(), Error> {
    let _ = (sctx, stmt_node);
    Err(blocked("D09.0", "the preprocessor"))
}

/// `plannercore.GetPreparedStmt(execStmt, sessVars)` for an `EXECUTE` statement.
// glue: stub, blocked-by:D13.1 (plannercore.GetPreparedStmt, VS1-15)
fn get_prepared_stmt(stmt_node: &ast::Stmt) -> Result<Option<PlanCacheStmtRef>, Error> {
    let _ = stmt_node;
    Ok(None)
}

/// `planner.Optimize(ctx, sctx, nodeW, is)`: the final plan and its output names.
// glue: stub, blocked-by:D09.13 (planner.Optimize, VS1-10)
fn planner_optimize(
    sctx: &mut dyn SessionContext,
    stmt_node: &ast::Stmt,
    is: Option<&InfoSchemaRef>,
) -> Result<(Plan, Vec<FieldName>), Error> {
    let _ = (sctx, stmt_node, is);
    Err(blocked("D09.13", "planner.Optimize"))
}

impl Compiler {
    /// Compile compiles an ast.StmtNode to a physical plan.
    ///
    /// `text` is `stmtNode.Text()` (the parser AST does not keep the text, the session
    /// passes it).
    // go: Compiler.Compile L2 (panics at the task boundary, M-PANIC; the statement text is passed in)
    pub fn compile(
        &self,
        sctx: &mut dyn SessionContext,
        stmt_node: Arc<ast::Stmt>,
        text: &str,
    ) -> Result<ExecStmt, Error> {
        let is_read_only = planner_is_read_only(&stmt_node)?;
        sctx.get_session_vars_mut().stmt_ctx_mut().is_read_only = is_read_only;

        // Do preprocess and validate.
        preprocess(sctx, &stmt_node)?;

        let is = sctx.get_txn_info_schema();
        // handle the execute statement
        let prepared_obj = get_prepared_stmt(&stmt_node)?;
        // Build the final physical plan.
        let (final_plan, names) = planner_optimize(sctx, &stmt_node, is.as_ref())?;

        let (in_restricted_sql, resource_group) = {
            let vars = sctx.get_session_vars();
            (
                vars.in_restricted_sql,
                vars.stmt_ctx().resource_group_name.clone(),
            )
        };
        count_stmt_node(&stmt_node, in_restricted_sql, &resource_group);
        let mut lower_priority = false;
        if sctx.get_session_vars().stmt_ctx().priority == NoPriority {
            lower_priority = need_lower_priority(&final_plan);
        }
        let plan_any: Arc<dyn std::any::Any + Send + Sync> = Arc::new(final_plan.clone());
        sctx.get_session_vars_mut()
            .stmt_ctx_mut()
            .set_plan(Some(plan_any));
        let mut stmt = ExecStmt::new(text.to_owned(), Some(final_plan), is);
        stmt.lower_priority = lower_priority;
        stmt.stmt_node = Some(stmt_node);
        stmt.output_names = names;
        stmt.ti = Arc::new(TelemetryInfo::default());
        // Use cached plan if possible: the `Execute` plan of a prepared point get (D13.1).
        let _ = prepared_obj;

        // Perform optimization and initialization related to the transaction level.
        sctx.advise_optimize_with_plan_and_then_warm_up(stmt.plan.as_ref().expect("plan"))?;

        Ok(stmt)
    }
}

/// needLowerPriority checks whether it's needed to lower the execution priority of a query.
/// If the estimated output row count of any operator in the physical plan tree is greater
/// than the specific threshold, we'll set it to lowPriority when sending it to the
/// coprocessor.
///
/// The new stack's plans are physical (no `Execute`, `Insert`, `Delete`, `Update`).
// go: needLowerPriority L1
fn need_lower_priority(p: &Plan) -> bool {
    is_physical_plan_need_lower_priority(p)
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

    #[test]
    fn compile_is_blocked_by_the_planner() {
        let mut s = MockSession::new(u64::MAX);
        let stmt = tidb_parser::parse("select 1", &Default::default())
            .expect("parse")
            .stmts
            .remove(0);
        let err = Compiler
            .compile(&mut s, Arc::new(stmt), "select 1")
            .err()
            .unwrap();
        assert_eq!(tidb_base::blocked::blocked_by(&err), Some("D09.13"));
    }
}
