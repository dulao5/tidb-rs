//! The "previous statement" state of VS1 plan §4.5. The next statement may run on the other
//! engine, so whichever engine ran a statement writes all of this, and the router copies it
//! to the other side: into the new session's `SessionVars`, or into the PoC's copy (the PoC
//! keeps no such state of its own and answers none of these reads yet; they need D08.9
//! functions or system variables it rejects).
//!
//! The values are what the *next* statement observes. Go computes them when that statement
//! starts (`ResetContextOfStmt`, `pkg/executor/select.go:1211-1251`, and `recordSet.Next`
//! for `LastFoundRows`, `pkg/executor/adapter.go:180`); [`LastStmtInfo::from_new`] mirrors
//! that computation on the finished statement.
//!
//! The warning and error *texts* are not copied: SHOW WARNINGS / SHOW ERRORS go to the engine
//! that ran the previous statement (§4.5, first bullet).
//!
//! TODO(VS1-14): once `ResetContextOfStmt` is ported, [`LastStmtInfo::apply_to`] writes its
//! inputs (the finished statement context) rather than its outputs, and the PoC's start TS
//! fills `tidb_last_query_info` (the PoC does not report it: 0 until then).

use tidb_mysql_protocol::{Response, SqlError};
use tidb_sessionctx::sessionstates::QueryInfo as LastQueryInfo;
use tidb_sessionctx::variable::session::SessionVars;
use tidb_sessionctx::variable::session_state::QueryInfo;

/// Which engine ran a statement.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Engine {
    /// The ported stack (`tidb-session`).
    New,
    /// The PoC (`tidb-sql` + `poc-executor`).
    #[default]
    Poc,
}

impl Engine {
    /// The `engine` label of `tidbrs_route_total`.
    pub fn label(self) -> &'static str {
        match self {
            Engine::New => "new",
            Engine::Poc => "poc",
        }
    }
}

/// The previous statement's state, one field per row of the table in VS1 plan §4.5.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LastStmtInfo {
    /// The engine that ran it (SHOW WARNINGS goes there).
    pub engine: Engine,
    /// `@@warning_count` (`SessionVars.SysWarningCount`; errors count as warnings).
    pub warning_count: i64,
    /// `@@error_count` (`SessionVars.SysErrorCount`).
    pub error_count: u16,
    /// `ROW_COUNT()` (`StmtCtx.PrevAffectedRows`: -1 after a SELECT).
    pub affected_rows: i64,
    /// `FOUND_ROWS()` (`SessionVars.LastFoundRows`).
    pub found_rows: u64,
    /// `LAST_INSERT_ID()` (`StmtCtx.PrevLastInsertID`; a read-only node never changes it).
    pub last_insert_id: u64,
    /// `@@last_plan_from_cache` (`SessionVars.PrevFoundInPlanCache`; always false on the PoC).
    pub plan_from_cache: bool,
    /// `@@last_plan_from_binding` (`SessionVars.PrevFoundInBinding`; false in VS1).
    pub plan_from_binding: bool,
    /// `@@last_sql_use_alloc` (`SessionVars.preUseChunkAlloc`).
    pub use_chunk_alloc: bool,
    /// `@@tidb_last_query_info` (`SessionVars.LastQueryInfo`: start_ts, for_update_ts, error).
    pub query_info: QueryInfo,
    /// `@@tidb_last_txn_info` (`SessionVars.LastTxnInfo`).
    pub txn_info: String,
}

impl LastStmtInfo {
    /// After the PoC ran a statement with `result`; `prev` is the state before it (what a
    /// statement leaves unchanged).
    pub fn from_poc(prev: &LastStmtInfo, result: &Result<Response, SqlError>) -> LastStmtInfo {
        let mut info = LastStmtInfo {
            engine: Engine::Poc,
            last_insert_id: prev.last_insert_id,
            found_rows: prev.found_rows,
            txn_info: prev.txn_info.clone(),
            ..LastStmtInfo::default()
        };
        match result {
            Ok(Response::ResultSet(rs)) => {
                info.warning_count = i64::from(rs.warnings);
                info.affected_rows = -1;
                info.found_rows = rs.rows.len() as u64;
            }
            Ok(Response::Ok(ok)) => {
                info.warning_count = i64::from(ok.warnings);
                info.affected_rows = 0;
            }
            Err(e) => {
                info.warning_count = 1;
                info.error_count = 1;
                info.query_info.err_msg = e.message.clone();
            }
        }
        info
    }

    /// After the new session ran a statement: Go's `ResetContextOfStmt` computation on the
    /// finished statement context.
    pub fn from_new(vars: &SessionVars) -> LastStmtInfo {
        let sc = vars.stmt_ctx();
        let (error_count, warning_count) = sc.num_error_warnings();
        let affected_rows = if sc.in_update_stmt
            || sc.in_delete_stmt
            || sc.in_insert_stmt
            || sc.in_set_session_states_stmt
        {
            sc.affected_rows() as i64
        } else if sc.in_select_stmt {
            -1
        } else {
            0
        };
        LastStmtInfo {
            engine: Engine::New,
            warning_count,
            error_count,
            affected_rows,
            found_rows: vars.last_found_rows,
            last_insert_id: if sc.last_insert_id() > 0 {
                sc.last_insert_id()
            } else {
                sc.prev_last_insert_id
            },
            plan_from_cache: vars.found_in_plan_cache,
            plan_from_binding: vars.found_in_binding,
            use_chunk_alloc: sc.get_use_chunk_alloc_status(),
            query_info: vars.state.last_query_info.clone(),
            txn_info: vars.last_txn_info.clone(),
        }
    }

    /// Writes the state into the new session, where its system variables and functions read
    /// it (`@@warning_count` reads `SysWarningCount`, ...).
    pub fn apply_to(&self, vars: &mut SessionVars) {
        vars.sys_warning_count = self.warning_count;
        vars.sys_error_count = self.error_count;
        vars.last_found_rows = self.found_rows;
        vars.prev_found_in_plan_cache = self.plan_from_cache;
        vars.prev_found_in_binding = self.plan_from_binding;
        vars.pre_use_chunk_alloc = self.use_chunk_alloc;
        vars.state.last_query_info = self.query_info.clone();
        vars.last_query_info = LastQueryInfo {
            txn_scope: self.query_info.txn_scope.clone(),
            start_ts: self.query_info.start_ts,
            for_update_ts: self.query_info.for_update_ts,
            ru_consumption: self.query_info.ru_consumption,
            err_msg: self.query_info.err_msg.clone(),
        };
        vars.last_txn_info = self.txn_info.clone();
        // Between statements nothing else holds the statement context.
        let sc = vars.stmt_ctx_mut();
        sc.prev_affected_rows = self.affected_rows;
        sc.prev_last_insert_id = self.last_insert_id;
    }
}
