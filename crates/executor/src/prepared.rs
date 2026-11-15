//! go-file: pkg/executor/prepared.go
//!
//! `PrepareExec` (block D12.12, the prepare executor; the part prepared-statement planning
//! needs, VS1-15): parses the statement of a binary-protocol PREPARE, builds its
//! `PlanCacheStmt` (D13.1) and adds it to the session. `ExecuteExec` and `DeallocateExec`
//! serve the text-protocol `EXECUTE` / `DEALLOCATE`, which are not in the Rust AST (D02.2).
//!
//! Go's `PrepareExec` is an executor whose `Next` does the work and returns no rows; nothing
//! drives it but the session, so here it is a plain struct with `next` (no `BaseExecutor`:
//! no children, no chunk).

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_base::dbterror::exeerrors::ErrPrepareMulti;
use tidb_parser::{ParseOptions, parse};
use tidb_planner::core::plan_cache_utils::generate_plan_cache_stmt_with_ast;

use crate::adapter::{col_names2_result_fields, is_no_result_plan};
use crate::glue::{PlanCacheStmtRef, ResultField, SessionContext};
use crate::select::reset_context_of_stmt;

/// PrepareExec represents a PREPARE executor.
pub struct PrepareExec {
    name: String,
    sql_text: String,

    pub id: u32,
    pub param_count: usize,
    pub fields: Vec<ResultField>,
    pub stmt: Option<PlanCacheStmtRef>,

    /// If it's generated from executing "prepare stmt from '...'", the process is parse -> plan -> executor
    /// If it's generated from the prepare protocol, the process is session.PrepareStmt -> NewPrepareExec
    /// They both generate a PrepareExec struct, but the second case needs to reset the statement context while the first already do that.
    /// Also, the second case need charset_client param since SQL is directly passed from clients.
    /// While the text-prepare already transformed charset by parser.
    need_reset: bool,
}

/// NewPrepareExec creates a new PrepareExec.
// go: NewPrepareExec L2 (no BaseExecutor, see the module doc)
pub fn new_prepare_exec(sql_txt: &str) -> PrepareExec {
    PrepareExec {
        name: String::new(),
        sql_text: sql_txt.to_owned(),
        id: 0,
        param_count: 0,
        fields: Vec::new(),
        stmt: None,
        need_reset: true,
    }
}

impl PrepareExec {
    /// Next implements the Executor Next interface.
    ///
    /// Differences (spec): the Rust parser takes no charset parameters (the protocol layer
    /// hands the session UTF-8 text) and its syntax errors and warnings do not have Go's
    /// text, so both are `blocked-by:D02.1` and the PoC answers; the statement's text is the
    /// whole SQL (one statement); Top SQL registration is not ported (D14.3) and
    /// `MapHashCode2UniqueID4ExtendedCol` is not on the Rust session.
    // go: PrepareExec.Next L2 (parse errors / warnings: blocked-by:D02.1; Top SQL: D14.3)
    pub fn next(&mut self, sctx: &mut dyn SessionContext) -> Result<(), Error> {
        if self.id != 0 {
            // Must be the case when we retry a prepare.
            // Make sure it is idempotent.
            if sctx
                .get_session_vars()
                .state
                .prepared_stmts
                .contains_key(&self.id)
            {
                return Ok(());
            }
        }
        // params = vars.GetParseParams() (needReset) or the connection's charset and
        // collation: the Rust parser has no charset parameters.
        let _ = self.need_reset;
        let out = match parse(&self.sql_text, &ParseOptions::default()) {
            Ok(out) => out,
            // return util.SyntaxError(err)
            // glue: blocked-by:D02.1 (the parser's error text differs from Go's)
            Err(e) => {
                log::debug!("prepare: the Rust parser rejects the statement: {e:?}");
                return Err(blocked("D02.1", "syntax error text"));
            }
        };
        if !out.warnings.is_empty() {
            // e.Ctx().GetSessionVars().StmtCtx.AppendWarning(util.SyntaxWarn(warn))
            // glue: blocked-by:D02.1 (the parser's warning text differs from Go's)
            return Err(blocked("D02.1", "parser warnings"));
        }
        let mut stmts = out.stmts;
        if stmts.len() != 1 {
            return Err(ErrPrepareMulti.clone());
        }
        let stmt0 = stmts.remove(0);
        if self.need_reset {
            reset_context_of_stmt(sctx, &stmt0, &self.sql_text)?;
        }
        let Some(is) = sctx.get_txn_info_schema() else {
            // sessiontxn.GetTxnManager(e.Ctx()).GetTxnInfoSchema() is set by PrepareStmt.
            return Err(blocked("D05.4", "the transaction's InfoSchema"));
        };
        let (stmt, p, param_cnt) = generate_plan_cache_stmt_with_ast(
            sctx.get_session_vars_mut(),
            true,
            &self.sql_text,
            stmt0,
            is.as_ref(),
        )?;
        // topsqlstate.TopSQLEnabled(): Top SQL is not ported (D14.3).

        let vars = sctx.get_session_vars_mut();
        vars.state
            .plan_id
            .0
            .store(0, std::sync::atomic::Ordering::SeqCst);
        vars.state
            .plan_column_id
            .0
            .store(0, std::sync::atomic::Ordering::SeqCst);
        // In MySQL prepare protocol, the server need to tell the client how many column the prepared statement would return when executing it.
        // For a query with on result, e.g. an insert statement, there will be no result, so 'e.Fields' is not set.
        // Usually, p.Schema().Len() == 0 means no result. A special case is the 'do' statement, it looks like 'select' but discard the result.
        if let Some(p) = &p
            && !is_no_result_plan(p)
        {
            let names: Vec<_> = p.output_names().iter().map(|n| (**n).clone()).collect();
            self.fields = col_names2_result_fields(p.schema(), &names, &vars.current_db);
        }
        if self.id == 0 {
            self.id = vars.get_next_prepared_stmt_id();
        }
        if !self.name.is_empty() {
            vars.state
                .prepared_stmt_name_to_id
                .insert(self.name.clone(), self.id);
        }

        self.param_count = param_cnt;
        let stmt: PlanCacheStmtRef = Arc::new(parking_lot::Mutex::new(stmt));
        self.stmt = Some(Arc::clone(&stmt));
        vars.add_prepared_stmt(self.id, stmt)
    }
}
