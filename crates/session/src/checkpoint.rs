//! The statement checkpoint of VS1 plan §4.6: a statement that falls back to the PoC after
//! the new session has started on it (`ExecuteStmt` → `Compile` → `Optimize`, before any
//! executor is opened) must leave the session as if it had never run here.
//!
//! glue: VS1-02 (no Go counterpart: Go never abandons a statement half-way to re-run it
//! elsewhere).
//!
//! The checkpoint covers the parts of `SessionVars` that the statement path writes:
//!
//! - the statement context: the `Arc` itself (Go's `ResetContextOfStmt` installs a new one),
//!   plus its warnings and extra warnings (appended through the shared reference);
//! - the scalar fields (`LastFoundRows`, `FoundInPlanCache`, `PrevFoundInPlanCache`, the
//!   statement counters, `preparedStmtID`, ...) and the non-scalar session state (`TxnCtx` and
//!   the transaction status flags, `PlanCacheParams`, `PlanID` / `PlanColumnID`,
//!   `LastQueryInfo`, the prepared statements);
//! - `SessionVars.LastQueryInfo`.
//!
//! What it does not restore: state changed *through* shared `Arc`s (the interior of
//! `user_vars`, any cache shared with other sessions). That is correct only while such state
//! changes after executors open (`SELECT @a := 1` assigns during execution; `set_var` is never
//! folded at planning time), i.e. never before a `Blocked` can occur. A planner-time cache in
//! shared state would escape the restore: it must not be added without extending this.
//!
//! Cost (Lucky, #212 review): `checkpoint()` + drop about 330 ns per statement, with
//! `restore` about 390 ns (release build), against a ~100 µs point select. It stays cheap
//! because the cloned parts hold `Arc`s (`prepared_stmts`, `user_vars`): O(#prepared)
//! refcount bumps, no deep copies. Keep it so when the field list is narrowed.
//!
//! TODO(VS1-14): narrow this to the field list of the VS1-14 plan (what the ported statement
//! path actually writes), and hand an already-sent TSO request over to the PoC (§4.6 item 2).
//! The new statement path must install a fresh statement context before writing its plain
//! fields: `stmt_ctx_mut()` on the checkpointed context panics because the checkpoint shares
//! it.

use std::sync::Arc;

use tidb_base::contextutil::SQLWarn;
use tidb_sessionctx::sessionstates::QueryInfo;
use tidb_sessionctx::stmtctx::StatementContext;
use tidb_sessionctx::variable::session::SessionState;
use tidb_sessionctx::variable::session_fields::SessionVarsFields;

use crate::session::Session;

/// The session state before a statement started on the new stack.
pub struct StmtCheckpoint {
    stmt_ctx: Arc<StatementContext>,
    warnings: Vec<SQLWarn>,
    extra_warnings: Vec<SQLWarn>,
    fields: Box<SessionVarsFields>,
    state: Box<SessionState>,
    last_query_info: QueryInfo,
}

impl Session {
    /// The state to restore if the statement falls back (VS1 plan §4.6).
    // glue: VS1-02
    pub fn checkpoint(&self) -> StmtCheckpoint {
        let vars = self.vars();
        let sc = vars.stmt_ctx_arc();
        StmtCheckpoint {
            warnings: sc.get_warnings(),
            extra_warnings: sc.get_extra_warnings(),
            stmt_ctx: sc,
            fields: Box::new(vars.fields.clone()),
            state: Box::new(vars.state.clone()),
            last_query_info: vars.last_query_info.clone(),
        }
    }

    /// Puts the session back to `cp`.
    // glue: VS1-02
    pub fn restore(&mut self, cp: StmtCheckpoint) {
        let vars = self.vars_mut();
        cp.stmt_ctx.set_warnings(cp.warnings);
        cp.stmt_ctx.set_extra_warnings(cp.extra_warnings);
        vars.set_stmt_ctx(cp.stmt_ctx);
        vars.fields = *cp.fields;
        vars.state = *cp.state;
        vars.last_query_info = cp.last_query_info;
    }

    /// A serialized snapshot of the session state the router keeps in sync and restores:
    /// every scalar field, the non-scalar state, `LastQueryInfo` and the statement context's
    /// warnings and previous-statement fields. Two snapshots are equal iff the state is
    /// (VS1 plan §4.6 item 3, the snapshot tests).
    // glue: VS1-02 (test support)
    pub fn state_snapshot(&self) -> String {
        use std::fmt::Write;
        let vars = self.vars();
        let mut out = String::new();
        for (k, v) in vars.fields.scalar_strings() {
            let _ = writeln!(out, "{k}={v}");
        }
        let sc = vars.stmt_ctx();
        let _ = writeln!(
            out,
            "StmtCtx.warnings={:?}\nStmtCtx.extraWarnings={:?}\nStmtCtx.PrevAffectedRows={}\n\
             StmtCtx.PrevLastInsertID={}",
            sc.get_warnings(),
            sc.get_extra_warnings(),
            sc.prev_affected_rows,
            sc.prev_last_insert_id,
        );
        let _ = writeln!(out, "LastQueryInfo={:?}", vars.last_query_info);
        let _ = writeln!(out, "state={:?}", vars.state);
        out
    }
}
