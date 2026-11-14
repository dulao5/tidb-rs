//! The VS1 router (glue, VS1-02; `specs/VS1-vertical-slice/plan.md` §4.3–§4.7).
//!
//! With `--new-stack=vs1|strict`, every statement of a session passes here before the PoC
//! sees it. VS1-shaped `SELECT`s (text and prepared) go to the ported stack (`tidb-session`);
//! everything else, and every statement the new stack answers with a boundary stub
//! (`tidb_base::blocked`), runs on the PoC. With `--new-stack=off` (the default) no router
//! exists and the PoC runs exactly as before.
//!
//! Routing order, once per statement (§4.3):
//!
//! 0. session conditions ([`Router::session_condition`]): an explicit transaction,
//!    `autocommit = 0`, `tidb_snapshot` / `tidb_read_staleness` / `tx_read_ts` /
//!    `tidb_txn_mode` off their defaults, a variable the new session does not know
//!    (diverged in the sync, [`sync`]), a cursor; then equal InfoSchema versions;
//! 1. the statement kind (`SELECT` only) and 2. the AST pre-check ([`precheck`]);
//! 3. the new stack plans; `Blocked(block)` (before any executor opened) restores the new
//!    session ([`tidb_session::StmtCheckpoint`], §4.6) and falls back to the PoC (`vs1`) or
//!    returns the 1235 error (`strict`). Any other error of the new stack is a Go-ported error
//!    and goes to the client as it is;
//! 4. prepared statements: §4.7, by statement id ([`Owner`]).
//!
//! After each statement the engine that ran it writes the "previous statement" state
//! ([`LastStmtInfo`], §4.5) and the router copies it to the other side.
//!
//! `COM_STMT_SEND_LONG_DATA` and `COM_STMT_RESET` never reach a session: the protocol layer
//! owns long data and decodes the parameters once (ADR 0010 §4.1), and both engines get the
//! same decoded parameters. `COM_STMT_FETCH` is rejected by the protocol layer (no cursor is
//! ever opened); a cursor flag on `COM_STMT_EXECUTE` is a step-0 condition.

pub mod last_stmt;
pub mod newstack;
mod precheck;
pub mod sync;
#[cfg(test)]
mod tests;

use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use tidb_base::Error;
use tidb_base::blocked::blocked_by;
use tidb_metrics::StmtType;
use tidb_mysql_protocol::{ColumnDef, Param, PreparedInfo, Response, SqlError};
use tidb_parser::ast;
use tidb_sql::{Lowered, Statement};

pub use last_stmt::{Engine, LastStmtInfo};

/// A boxed `Send` future (the engines are trait objects).
pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// `--new-stack` / `TIDB_RS_NEW_STACK` (VS1 plan §4.3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NewStackMode {
    /// Everything on the PoC, as before. The default.
    #[default]
    Off,
    /// VS1 candidates on the new stack; `Blocked` falls back to the PoC.
    Vs1,
    /// VS1 candidates on the new stack; `Blocked` is returned as 1235 (the weekly build's
    /// measure of what the new stack runs). Other statements still run on the PoC.
    Strict,
}

impl std::str::FromStr for NewStackMode {
    type Err = String;

    fn from_str(s: &str) -> Result<NewStackMode, String> {
        match s {
            "off" => Ok(NewStackMode::Off),
            "vs1" => Ok(NewStackMode::Vs1),
            "strict" => Ok(NewStackMode::Strict),
            _ => Err(format!("--new-stack must be off, vs1 or strict, got {s:?}")),
        }
    }
}

/// Why a statement runs on the PoC (the `reason` label of `tidbrs_route_total`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reason {
    /// Step 0: a session condition.
    Session(&'static str),
    /// Step 0: the two InfoSchema versions differ.
    SchemaVersion,
    /// Step 1: not a `SELECT`.
    Kind,
    /// Step 2: the AST pre-check (the detail is for tests and logs).
    Ast(&'static str),
    /// Step 3: the new stack hit a boundary stub of this block.
    Blocked(String),
}

impl Reason {
    /// The label value.
    pub fn label(&self) -> &str {
        match self {
            Reason::Session(_) => "session",
            Reason::SchemaVersion => "schema_version",
            Reason::Kind => "kind",
            Reason::Ast(_) => "ast",
            Reason::Blocked(block) => block,
        }
    }
}

/// Where steps 0–2 send a statement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    /// The new stack.
    New,
    /// The PoC, and why.
    Poc(Reason),
}

/// A statement prepared by the new stack: its id there and its metadata (Go's column
/// definitions, which the client caches).
#[derive(Clone, Debug, PartialEq)]
pub struct NewPrepared {
    /// The new session's statement id.
    pub stmt_id: u32,
    /// Parameter count and column definitions.
    pub info: PreparedInfo,
}

/// The ported stack as the router sees it: the new session plus the glue that turns its
/// results into protocol responses (D03.1, VS1-16). [`newstack::NewSession`] is the real one.
pub trait NewStack: Send {
    /// The new session.
    fn session(&self) -> &tidb_session::Session;
    /// The new session, for writing (sync, checkpoint restore).
    fn session_mut(&mut self) -> &mut tidb_session::Session;
    /// The version of the InfoSchema the new stack would plan with, `None` while it has none.
    fn info_schema_version(&self) -> Option<i64>;
    /// `COM_QUERY`.
    fn execute<'a>(&'a mut self, stmt: &'a ast::Stmt) -> BoxFut<'a, Result<Response, Error>>;
    /// `COM_STMT_PREPARE`.
    fn prepare<'a>(&'a mut self, stmt: &'a ast::Stmt) -> BoxFut<'a, Result<NewPrepared, Error>>;
    /// `COM_STMT_EXECUTE` of statement `stmt_id` (the new session's id).
    fn execute_prepared<'a>(
        &'a mut self,
        stmt_id: u32,
        params: &'a [Param],
    ) -> BoxFut<'a, Result<Response, Error>>;
    /// `COM_STMT_CLOSE`.
    fn close(&mut self, stmt_id: u32);
}

/// The PoC session's state the router syncs from (§4.1).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PocValues {
    /// `USE`.
    pub current_db: Option<String>,
    /// `SET NAMES` charset (`character_set_client` / `connection`).
    pub charset: String,
    /// `SET NAMES` collation (`collation_connection`).
    pub collation: String,
    /// `@@character_set_results` (`None`: NULL).
    pub results_charset: Option<String>,
}

/// The PoC as the router sees it ([`crate::Session`] implements it).
pub trait PocEngine: Send {
    /// The schema version the PoC would plan with, `None` if it has none loaded.
    fn schema_version(&self) -> Option<i64>;
    /// Whether the PoC session is in an explicit transaction.
    fn in_txn(&self) -> bool;
    /// The state synced into the new session.
    fn values(&self) -> PocValues;
    /// A command starts (statement timing, metadata lock).
    fn begin_command(&mut self);
    /// The statement's type is known (statement metrics; once per command).
    fn start_statement(&mut self, kind: StmtType, parse: Duration);
    /// Runs a lowered `COM_QUERY` statement (the statement was started).
    fn run<'a>(&'a mut self, lowered: &'a Lowered) -> BoxFut<'a, Result<Response, SqlError>>;
    /// `COM_STMT_PREPARE` of a lowered statement.
    fn prepare(
        &mut self,
        stmt_id: u32,
        lowered: Lowered,
    ) -> BoxFut<'_, Result<PreparedInfo, SqlError>>;
    /// `COM_STMT_EXECUTE` (starts the statement itself).
    fn execute(
        &mut self,
        stmt_id: u32,
        params: Vec<Param>,
    ) -> BoxFut<'_, Result<Response, SqlError>>;
    /// `COM_STMT_CLOSE`.
    fn close(&mut self, stmt_id: u32);
}

/// Who owns a prepared statement (§4.7, the four combinations).
#[derive(Clone, Debug, PartialEq)]
enum Owner {
    /// Prepared on both sides: executes on the new stack, falls back once per `Blocked`.
    Both {
        new_id: u32,
        /// The column definitions sent at PREPARE (the new stack's).
        columns: Vec<ColumnDef>,
    },
    /// Only the PoC: always the PoC (the reason, a block if the new stack was `Blocked`).
    Poc(Reason),
    /// Only the new stack (the PoC cannot prepare it): nothing to fall back to.
    New { new_id: u32 },
}

/// A statement for the new stack.
enum NewOp<'a> {
    /// `COM_QUERY`.
    Query(&'a ast::Stmt),
    /// `COM_STMT_EXECUTE` of the new session's statement.
    Execute(u32, &'a [Param]),
}

/// The routing state of one session.
pub struct Router {
    mode: NewStackMode,
    new: Box<dyn NewStack>,
    owners: HashMap<u32, Owner>,
    /// The previous statement's state (§4.5): the PoC's copy, and what was last written.
    last: LastStmtInfo,
    /// Synced variables the new session rejected (step 0).
    diverged: BTreeSet<String>,
    /// `tidb_txn_mode` when the session opened (step 0 compares with it).
    initial_txn_mode: String,
    /// `COM_STMT_EXECUTE` of the current command asked for a cursor.
    cursor: bool,
}

/// A new-stack error as the client sees it (Go's code and message).
fn sql_error(e: &Error) -> SqlError {
    let code = u16::try_from(e.code()).unwrap_or(1105);
    SqlError::new(code, String::from_utf8_lossy(&e.get_msg()).into_owned())
}

/// The 1235 error of a statement only the new stack can run when it cannot (§4.7, row 3).
fn nothing_to_fall_back(why: &str) -> SqlError {
    SqlError::new(
        1235,
        format!(
            "This version of TiDB doesn't yet support 'this prepared statement on the PoC, \
             and the new stack cannot run it now ({why})'"
        ),
    )
}

/// `tidbrs_route_total`.
fn count(engine: Engine, reason: &str) {
    tidb_metrics::route(engine.label(), reason);
}

/// §4.7: a fallback's result set must carry exactly the column definitions sent at PREPARE.
fn check_columns(prepared: &[ColumnDef], resp: &Response) -> Result<(), SqlError> {
    let Response::ResultSet(rs) = resp else {
        return Ok(());
    };
    if rs.columns.len() != prepared.len() {
        return Err(SqlError::unknown(format!(
            "VS1 column metadata mismatch: {} columns prepared, {} returned by the PoC",
            prepared.len(),
            rs.columns.len()
        )));
    }
    for (i, (p, e)) in prepared.iter().zip(&rs.columns).enumerate() {
        if p != e {
            return Err(SqlError::unknown(format!(
                "VS1 column metadata mismatch: column {i}: prepared {p:?}, PoC {e:?}"
            )));
        }
    }
    Ok(())
}

impl Router {
    /// A router for a session whose PoC state is `poc` (the handshake's charset).
    pub fn new(mode: NewStackMode, new: Box<dyn NewStack>, poc: &PocValues) -> Router {
        let initial_txn_mode = new.session().vars().txn_mode.clone();
        let mut r = Router {
            mode,
            new,
            owners: HashMap::new(),
            last: LastStmtInfo::default(),
            diverged: BTreeSet::new(),
            initial_txn_mode,
            cursor: false,
        };
        let names = [tidb_sql::ir::SetAssignment::Names {
            charset: None,
            collation: None,
        }];
        let vars = r.new.session_mut().vars_mut();
        sync::sync_set(vars, &mut r.diverged, &names, poc);
        sync::sync_db(vars, poc);
        r
    }

    /// The mode.
    pub fn mode(&self) -> NewStackMode {
        self.mode
    }

    /// The previous statement's state as last recorded (§4.5).
    pub fn last_stmt(&self) -> &LastStmtInfo {
        &self.last
    }

    /// The new stack.
    pub fn new_stack(&self) -> &dyn NewStack {
        &*self.new
    }

    /// Step 0: the session condition that keeps statements on the PoC, if any.
    pub fn session_condition(&self, poc: &dyn PocEngine) -> Option<&'static str> {
        let vars = self.new.session().vars();
        if poc.in_txn() || vars.in_txn() {
            return Some("explicit transaction");
        }
        if !vars.is_autocommit() {
            return Some("autocommit=0");
        }
        if vars.snapshot_ts != 0 {
            return Some("tidb_snapshot");
        }
        if vars.read_staleness != 0 {
            return Some("tidb_read_staleness");
        }
        if vars.state.txn_read_ts.is_some() {
            return Some("tx_read_ts");
        }
        if vars.txn_mode != self.initial_txn_mode {
            return Some("tidb_txn_mode");
        }
        if !self.diverged.is_empty() {
            return Some("variable unknown to the new session");
        }
        if self.cursor {
            return Some("cursor");
        }
        None
    }

    /// Step 0: the InfoSchema versions are equal. The new stack gets its InfoSchema from
    /// the PoC domain's sync loop (VS1-17); until then it has none and its planner returns
    /// `Blocked`, so the check passes.
    fn schema_versions_equal(&self, poc: &dyn PocEngine) -> bool {
        match self.new.info_schema_version() {
            None => true,
            Some(v) => poc.schema_version() == Some(v),
        }
    }

    /// Steps 1–2.
    fn candidate(stmt: &ast::Stmt, allow_params: bool) -> Result<(), Reason> {
        let sel = precheck::select_of(stmt).ok_or(Reason::Kind)?;
        precheck::ast_precheck(sel, allow_params).map_err(Reason::Ast)
    }

    /// Steps 0–2 for a statement.
    pub fn route(&self, stmt: &ast::Stmt, poc: &dyn PocEngine, allow_params: bool) -> Route {
        if let Some(c) = self.session_condition(poc) {
            return Route::Poc(Reason::Session(c));
        }
        if !self.schema_versions_equal(poc) {
            return Route::Poc(Reason::SchemaVersion);
        }
        match Self::candidate(stmt, allow_params) {
            Ok(()) => Route::New,
            Err(r) => Route::Poc(r),
        }
    }

    /// The new stack finished a statement: record its state and copy it to the PoC side.
    fn finish_new(&mut self) {
        self.last = LastStmtInfo::from_new(self.new.session().vars());
    }

    /// The PoC finished a statement with `result`: record its state and copy it into the new
    /// session.
    fn finish_poc(&mut self, result: &Result<Response, SqlError>) {
        self.last = LastStmtInfo::from_poc(&self.last, result);
        self.last.apply_to(self.new.session_mut().vars_mut());
    }

    /// Runs `op` on the new stack with a checkpoint. `Ok(Ok(resp))` / `Ok(Err(e))`: the new
    /// stack answered (`e` is a Go-ported error, returned as it is); `Err(block)`: blocked
    /// before any executor opened, the new session restored (§4.6).
    async fn try_new(&mut self, op: NewOp<'_>) -> Result<Result<Response, SqlError>, String> {
        let cp = self.new.session().checkpoint();
        let r = match op {
            NewOp::Query(stmt) => self.new.execute(stmt).await,
            NewOp::Execute(id, params) => self.new.execute_prepared(id, params).await,
        };
        match r {
            Ok(resp) => {
                drop(cp);
                self.finish_new();
                count(Engine::New, "ok");
                Ok(Ok(resp))
            }
            Err(e) => match blocked_by(&e) {
                Some(block) => {
                    let block = block.to_owned();
                    self.new.session_mut().restore(cp);
                    count(Engine::Poc, &block);
                    Err(block)
                }
                None => {
                    drop(cp);
                    self.finish_new();
                    count(Engine::New, "error");
                    Ok(Err(sql_error(&e)))
                }
            },
        }
    }

    /// An error the router itself returns for a statement (1235 in `strict` mode, nothing
    /// to fall back to): recorded as the previous statement's state like the PoC's errors.
    fn fail(&mut self, e: SqlError) -> Result<Response, SqlError> {
        let r = Err(e);
        self.finish_poc(&r);
        r
    }

    /// `COM_QUERY`.
    pub async fn query(
        &mut self,
        poc: &mut dyn PocEngine,
        sql: &str,
    ) -> Result<Response, SqlError> {
        self.cursor = false;
        poc.begin_command();
        let start = Instant::now();
        let stmt = tidb_sql::ir::parse_one(sql)?;
        let parse = start.elapsed();
        tidb_metrics::parse_duration(parse);
        let mut started = false;
        match self.route(&stmt, poc, false) {
            Route::New => {
                poc.start_statement(StmtType::Select, parse);
                started = true;
                match self.try_new(NewOp::Query(&stmt)).await {
                    Ok(answered) => return answered,
                    Err(block) => {
                        if self.mode == NewStackMode::Strict {
                            return self.fail(sql_error(&tidb_base::blocked::blocked(
                                &block,
                                "this statement on the new stack",
                            )));
                        }
                    }
                }
            }
            Route::Poc(reason) => count(Engine::Poc, reason.label()),
        }
        let lowered = match tidb_sql::ir::lower_statement(&stmt, sql) {
            Ok(l) => l,
            Err(e) => return self.fail(e),
        };
        if !started {
            poc.start_statement(crate::stmt_type(&lowered.stmt), parse);
        }
        if lowered.param_count > 0 {
            return self.fail(crate::param_in_query());
        }
        let result = poc.run(&lowered).await;
        if result.is_ok() {
            let vars = self.new.session_mut().vars_mut();
            match &lowered.stmt {
                Statement::Set(assignments) => {
                    sync::sync_set(vars, &mut self.diverged, assignments, &poc.values())
                }
                Statement::Use(_) => sync::sync_db(vars, &poc.values()),
                _ => {}
            }
        }
        self.finish_poc(&result);
        result
    }

    /// `COM_INIT_DB` / the handshake's database, after the PoC accepted it.
    pub fn use_db(&mut self, poc: &dyn PocEngine) {
        sync::sync_db(self.new.session_mut().vars_mut(), &poc.values());
    }

    /// `COM_STMT_PREPARE` (§4.7): both sides prepare; the table decides the owner.
    pub async fn prepare(
        &mut self,
        poc: &mut dyn PocEngine,
        stmt_id: u32,
        sql: &str,
    ) -> Result<PreparedInfo, SqlError> {
        let stmt = tidb_sql::ir::parse_one(sql)?;
        let new = match Self::candidate(&stmt, true) {
            Ok(()) => {
                let cp = self.new.session().checkpoint();
                let r = self.new.prepare(&stmt).await;
                match &r {
                    Err(e) if blocked_by(e).is_some() => self.new.session_mut().restore(cp),
                    _ => drop(cp),
                }
                Ok(r)
            }
            Err(reason) => Err(reason),
        };
        let old = match tidb_sql::ir::lower_statement(&stmt, sql) {
            Ok(lowered) => poc.prepare(stmt_id, lowered).await,
            Err(e) => Err(e),
        };
        let (owner, result) = match (new, old) {
            (Ok(Ok(n)), Ok(_)) => (
                Some(Owner::Both {
                    new_id: n.stmt_id,
                    columns: n.info.columns.clone(),
                }),
                Ok(n.info),
            ),
            (Ok(Ok(n)), Err(_)) => (Some(Owner::New { new_id: n.stmt_id }), Ok(n.info)),
            (Ok(Err(e)), old) => match (blocked_by(&e), old) {
                (Some(block), Ok(info)) => (
                    Some(Owner::Poc(Reason::Blocked(block.to_owned()))),
                    Ok(info),
                ),
                (Some(_), Err(pe)) => (None, Err(pe)),
                // A Go-ported error: returned as it is (§4.3 step 3).
                (None, old) => {
                    if old.is_ok() {
                        poc.close(stmt_id);
                    }
                    (None, Err(sql_error(&e)))
                }
            },
            (Err(reason), Ok(info)) => (Some(Owner::Poc(reason)), Ok(info)),
            (Err(_), Err(pe)) => (None, Err(pe)),
        };
        if let Some(owner) = owner {
            self.owners.insert(stmt_id, owner);
        }
        result
    }

    /// `COM_STMT_EXECUTE` (§4.7).
    pub async fn execute(
        &mut self,
        poc: &mut dyn PocEngine,
        stmt_id: u32,
        params: Vec<Param>,
    ) -> Result<Response, SqlError> {
        let owner = self.owners.get(&stmt_id).cloned();
        let result = match owner {
            // Unknown here: the PoC reports Go's error for an unknown statement.
            None => poc.execute(stmt_id, params).await,
            Some(Owner::Poc(reason)) => match reason {
                Reason::Blocked(block) if self.mode == NewStackMode::Strict => {
                    count(Engine::Poc, &block);
                    self.cursor = false;
                    return self.fail(sql_error(&tidb_base::blocked::blocked(
                        &block,
                        "this prepared statement on the new stack",
                    )));
                }
                reason => {
                    count(Engine::Poc, reason.label());
                    poc.execute(stmt_id, params).await
                }
            },
            Some(Owner::Both { new_id, columns }) => {
                if let Some(c) = self.session_condition(poc) {
                    count(Engine::Poc, Reason::Session(c).label());
                    poc.execute(stmt_id, params).await
                } else if !self.schema_versions_equal(poc) {
                    count(Engine::Poc, Reason::SchemaVersion.label());
                    poc.execute(stmt_id, params).await
                } else {
                    poc.begin_command();
                    poc.start_statement(StmtType::Select, Duration::ZERO);
                    match self.try_new(NewOp::Execute(new_id, &params)).await {
                        Ok(answered) => {
                            self.cursor = false;
                            return answered;
                        }
                        Err(block) if self.mode == NewStackMode::Strict => {
                            self.cursor = false;
                            return self.fail(sql_error(&tidb_base::blocked::blocked(
                                &block,
                                "this prepared statement on the new stack",
                            )));
                        }
                        Err(_) => {
                            let r = poc.execute(stmt_id, params).await;
                            match r {
                                Ok(resp) => check_columns(&columns, &resp).map(|()| resp),
                                Err(e) => Err(e),
                            }
                        }
                    }
                }
            }
            Some(Owner::New { new_id }) => {
                let why = match self.session_condition(poc) {
                    Some(c) => Some(format!("session condition: {c}")),
                    None if !self.schema_versions_equal(poc) => {
                        Some("InfoSchema versions differ".to_owned())
                    }
                    None => None,
                };
                self.cursor = false;
                if let Some(why) = why {
                    count(Engine::Poc, "session");
                    return self.fail(nothing_to_fall_back(&why));
                }
                poc.begin_command();
                poc.start_statement(StmtType::Select, Duration::ZERO);
                return match self.try_new(NewOp::Execute(new_id, &params)).await {
                    Ok(answered) => answered,
                    Err(block) => self.fail(nothing_to_fall_back(&format!("blocked-by:{block}"))),
                };
            }
        };
        self.cursor = false;
        self.finish_poc(&result);
        result
    }

    /// `COM_STMT_CLOSE`: to the owner, to both when both prepared it.
    pub fn close(&mut self, poc: &mut dyn PocEngine, stmt_id: u32) {
        match self.owners.remove(&stmt_id) {
            Some(Owner::Both { new_id, .. }) => {
                self.new.close(new_id);
                poc.close(stmt_id);
            }
            Some(Owner::New { new_id }) => self.new.close(new_id),
            Some(Owner::Poc(_)) | None => poc.close(stmt_id),
        }
    }

    /// The flags of the coming `COM_STMT_EXECUTE` (`CURSOR_TYPE_READ_ONLY` is step 0).
    pub fn set_execute_flags(&mut self, flags: u8) {
        self.cursor = flags & tidb_mysql_protocol::constants::cursor_type::READ_ONLY != 0;
    }

    /// The owner of a prepared statement, for tests.
    #[cfg(test)]
    fn owner(&self, stmt_id: u32) -> Option<&Owner> {
        self.owners.get(&stmt_id)
    }
}
