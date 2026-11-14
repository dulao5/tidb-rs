//! The session object of Go's `pkg/session/session.go`. Nothing of that file is ported yet:
//! the statement paths are boundary stubs until VS1-14.

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_parser::ast;
use tidb_sessionctx::variable::session::{GlobalVarAccessor, SessionVars};
use tidb_types::datum::Datum;

/// The SQL dialect of a connection (ADR 0010 §4.4): which syntax front end parses its
/// statements, part of the plan cache key. Set when the connection is opened, fixed for the
/// session's life; not a system variable.
// glue: ADR 0010 §4.4
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Dialect {
    /// MySQL syntax (`tidb-parser`), the only front end.
    #[default]
    MySQL,
}

/// The result of a statement that returns rows (Go `sqlexec.RecordSet`, built by
/// `executor.recordSet`).
///
/// TODO(VS1-13): the field list (`resolve.ResultField`), `Next(chk)` and `Close` of Go's
/// `recordSet` (`pkg/executor/adapter.go`); the protocol adapter encodes it (VS1-16).
pub struct RecordSet {
    _unconstructible: (),
}

/// What `PrepareStmt` returns (Go: `stmtID, paramCount, fields`).
///
/// TODO(VS1-14): the result fields (`[]*resolve.ResultField`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedStmtInfo {
    /// The session's id of the statement (Go `SessionVars.GetNextPreparedStmtID`), not the
    /// protocol's: the adapter maps between them (ADR 0010 §4.1).
    pub stmt_id: u32,
    /// Number of `?` placeholders.
    pub param_count: usize,
}

/// One client session (N2 `Session`; Go `session`). Owned by the connection's task; `Send`.
pub struct Session {
    /// The session variables: the only source of truth for session state (VS1 plan §4.1).
    vars: SessionVars,
    dialect: Dialect,
}

// N2: the session moves with its connection task between worker threads (VS1-04).
const _: () = {
    // glue: compile-time check
    const fn send<T: Send>() {}
    send::<Session>();
    send::<ast::Stmt>();
};

impl Session {
    /// A session with Go's default variables. `global` answers the global values of system
    /// variables (Go's `GlobalVarAccessor`, the domain's sysvar cache).
    // glue: stand-in for createSession / CreateSession4Test (D05.3, VS1-14)
    pub fn new(dialect: Dialect, global: Box<dyn GlobalVarAccessor>) -> Session {
        Session {
            vars: SessionVars::new(global),
            dialect,
        }
    }

    /// The connection's dialect (ADR 0010 §4.4).
    // glue: ADR 0010 §4.4
    pub fn dialect(&self) -> Dialect {
        self.dialect
    }

    /// `GetSessionVars()`.
    // glue: accessor (Go session.GetSessionVars)
    pub fn vars(&self) -> &SessionVars {
        &self.vars
    }

    /// `GetSessionVars()`, for writing.
    // glue: accessor (Go session.GetSessionVars)
    pub fn vars_mut(&mut self) -> &mut SessionVars {
        &mut self.vars
    }

    /// Whether the session is inside a transaction (ADR 0010 §4.1: state as a query; the
    /// MySQL adapter turns it into `SERVER_STATUS_IN_TRANS`).
    // glue: ADR 0010 §4.1 (reads SessionVars.InTxn)
    pub fn in_txn(&self) -> bool {
        self.vars.in_txn()
    }

    /// Whether autocommit is on.
    // glue: ADR 0010 §4.1 (reads SessionVars.IsAutocommit)
    pub fn is_autocommit(&self) -> bool {
        self.vars.is_autocommit()
    }

    /// The current database (`""` if none).
    // glue: ADR 0010 §4.1 (reads SessionVars.CurrentDB)
    pub fn current_db(&self) -> &str {
        &self.vars.current_db
    }

    /// `ExecuteStmt` executes a statement (Go `session.ExecuteStmt`, `session.go:2019`).
    // glue: stub, blocked-by:D05.3 (VS1-14)
    pub async fn execute_stmt(&mut self, stmt: &ast::Stmt) -> Result<RecordSet, Error> {
        let _ = stmt;
        Err(blocked("D05.3", "statement execution in the new session"))
    }

    /// `PrepareStmt` prepares a statement (Go `session.PrepareStmt`, `session.go:2483`).
    // glue: stub, blocked-by:D05.3 (VS1-14)
    pub async fn prepare_stmt(&mut self, stmt: &ast::Stmt) -> Result<PreparedStmtInfo, Error> {
        let _ = stmt;
        Err(blocked("D05.3", "prepared statements in the new session"))
    }

    /// `ExecutePreparedStmt` executes a prepared statement (Go
    /// `session.ExecutePreparedStmt`, `session.go:2654`).
    // glue: stub, blocked-by:D05.3 (VS1-14)
    pub async fn execute_prepared_stmt(
        &mut self,
        stmt_id: u32,
        params: Vec<Datum>,
    ) -> Result<RecordSet, Error> {
        let _ = (stmt_id, params);
        Err(blocked("D05.3", "prepared statements in the new session"))
    }

    /// `DropPreparedStmt` (COM_STMT_CLOSE).
    // glue: stub, blocked-by:D05.3 (VS1-14; nothing is prepared before it)
    pub fn drop_prepared_stmt(&mut self, stmt_id: u32) -> Result<(), Error> {
        let _ = stmt_id;
        Err(blocked("D05.3", "prepared statements in the new session"))
    }
}
