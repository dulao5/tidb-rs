//! The VS1 router (glue, VS1-02; `specs/VS1-vertical-slice/plan.md` §4.3–§4.7).
//!
//! With `--new-stack=fallback|strict`, every statement of a session passes here before the PoC
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
//!    session ([`tidb_session::StmtCheckpoint`], §4.6) and falls back to the PoC (`fallback`) or
//!    returns the 1235 error (`strict`). Any other error of the new stack is a Go-ported error
//!    and goes to the client as it is;
//! 4. prepared statements: §4.7, by statement id ([`Owner`]). Both sides prepare a candidate;
//!    the new session's `PrepareStmt` ends `Blocked` until prepared-statement planning
//!    (VS1-15) lands, so for now the PoC owns every prepared statement with the block as the
//!    reason, and the new session is restored like after any fallback
//!    (`specs/D05.3-session/spec.md` §6).
//!
//! After each statement the engine that ran it writes the "previous statement" state
//! ([`LastStmtInfo`], §4.5) and the router copies it to the other side.
//!
//! Routing of the other statement commands (§4.7): `COM_STMT_CLOSE` goes to the owner, to
//! both when both prepared ([`Router::close`]). `COM_STMT_SEND_LONG_DATA` and
//! `COM_STMT_RESET` never reach a session: the protocol layer owns long data and decodes the
//! parameters once (ADR 0010 §4.1), so whichever engine runs an EXECUTE gets the same decoded
//! parameters, long data included — "to the owner, to both when both" holds by construction.
//! `COM_STMT_FETCH` is answered by the protocol layer as the PoC answers it (1235: no cursor
//! is ever opened); a cursor flag on `COM_STMT_EXECUTE` is a step-0 condition, so a
//! new-only statement executed with a cursor gets the 1235 of §4.7 row 3.

pub mod last_stmt;
pub mod newstack;
mod precheck;
pub mod sync;
#[cfg(test)]
mod tests;

use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
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

/// `--new-stack` / `TIDB_RS_NEW_STACK`: which statements run on the ported (new) stack
/// (vertical-slice plan §4.3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NewStackMode {
    /// Everything on the PoC, as before. The default.
    #[default]
    Off,
    /// Statements the new stack supports run there; when it reaches code that is not ported
    /// yet (`Blocked`), the statement falls back to the PoC.
    Fallback,
    /// Statements the new stack supports run there; reaching unported code (`Blocked`) is returned as 1235 (the weekly build's
    /// measure of what the new stack runs). Other statements still run on the PoC.
    Strict,
}

impl std::str::FromStr for NewStackMode {
    type Err = String;

    fn from_str(s: &str) -> Result<NewStackMode, String> {
        match s {
            "off" => Ok(NewStackMode::Off),
            "fallback" => Ok(NewStackMode::Fallback),
            "strict" => Ok(NewStackMode::Strict),
            _ => Err(format!(
                "--new-stack must be off, fallback or strict, got {s:?}"
            )),
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
    /// The new session was not idle when a statement was to start there (a transaction, a
    /// running statement): a bug of the router (step 0 should have caught it).
    NotIdle,
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
            Reason::NotIdle => "not_idle",
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
    fn execute<'a>(
        &'a mut self,
        stmt: Arc<ast::Stmt>,
        sql: &'a str,
    ) -> BoxFut<'a, Result<Response, Error>>;
    /// `COM_STMT_PREPARE` of `sql` (Go's `PrepareStmt` parses the text itself).
    fn prepare<'a>(&'a mut self, sql: &'a str) -> BoxFut<'a, Result<NewPrepared, Error>>;
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

/// Why the new stack did not answer a statement.
#[derive(Debug)]
enum NotRun {
    /// Blocked by a boundary stub of this block before any executor opened; the new session
    /// is restored (§4.6).
    Blocked(String),
    /// The new session was not idle, so it was not tried (a router bug; the PoC runs it).
    NotIdle,
    /// `Blocked`, but the checkpoint could not be restored (a bug): the error for the client.
    Unrestorable(SqlError),
}

/// VS1 plan §4.3: a fallback only happens before any executor opened. `before` / `after` are
/// the new session's executed-statement counts around the attempt.
fn fallback_after_open(before: u64, after: u64) -> bool {
    after != before
}

/// A statement for the new stack.
enum NewOp<'a> {
    /// `COM_QUERY`: the statement and its text.
    Query(Arc<ast::Stmt>, &'a str),
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

/// The `engine` label of a statement that ran nowhere (1235 in `strict` mode, or a
/// new-only prepared statement the new stack cannot run now).
const NOWHERE: &str = "none";

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
    /// before any executor opened, the new session restored (§4.6). `fallback`: the PoC will
    /// run the statement after a `Blocked` (the metric's engine is `poc`; otherwise `none`:
    /// the statement gets 1235 and runs nowhere).
    async fn try_new(
        &mut self,
        op: NewOp<'_>,
        fallback: bool,
    ) -> Result<Result<Response, SqlError>, NotRun> {
        // A result dropped before it was closed is finished first: the checkpoint must not
        // undo that finish (spec §2.3).
        self.new.session_mut().finish_unfinished_stmt().await;
        // The checkpoint can only restore an idle session (§4.6): check before the attempt
        // writes anything. Step 0 should make this impossible, so it is a router bug.
        if !self.new.session().is_idle() {
            log::error!(
                "VS1 router bug: the new session is not idle at the start of a statement; the PoC runs it"
            );
            let label = Reason::NotIdle.label();
            if fallback {
                count(Engine::Poc, label);
            } else {
                tidb_metrics::route(NOWHERE, label);
            }
            return Err(NotRun::NotIdle);
        }
        let cp = self.new.session().checkpoint();
        let executed = self.new.session().executed_stmts();
        let r = match op {
            NewOp::Query(stmt, sql) => self.new.execute(stmt, sql).await,
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
                    if fallback_after_open(executed, self.new.session().executed_stmts()) {
                        // Plan §4.3: `Blocked` only before an executor opened. The new stack
                        // gave up after running the statement (newstack.rs until VS1-16).
                        log::error!(
                            "VS1 router bug: blocked-by:{block} after the new stack's executors opened"
                        );
                        tidb_metrics::route(Engine::New.label(), "bug_blocked_after_open");
                    }
                    if let Err(e) = self.new.session_mut().restore(cp) {
                        // Checked idle above: a bug of the checkpoint.
                        count(Engine::New, "error");
                        return Err(NotRun::Unrestorable(SqlError::unknown(e.to_string())));
                    }
                    if fallback {
                        count(Engine::Poc, &block);
                    } else {
                        tidb_metrics::route(NOWHERE, &block);
                    }
                    Err(NotRun::Blocked(block))
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

    /// `COM_STMT_PREPARE` on the new stack, guarded like [`Router::try_new`]: an unfinished
    /// statement is finished first, a busy session is not tried (a router bug), and a
    /// `Blocked` restores the session to its state before the PREPARE (§4.6; after it the new
    /// session equals one that never saw the PREPARE). `Ok(Err(e))`: a Go-ported error.
    async fn try_prepare_new(&mut self, sql: &str) -> Result<Result<NewPrepared, Error>, NotRun> {
        self.new.session_mut().finish_unfinished_stmt().await;
        if !self.new.session().is_idle() {
            log::error!(
                "VS1 router bug: the new session is not idle at a PREPARE; only the PoC prepares it"
            );
            return Err(NotRun::NotIdle);
        }
        let cp = self.new.session().checkpoint();
        let r = self.new.prepare(sql).await;
        match r {
            Err(e) => match blocked_by(&e) {
                Some(block) => {
                    let block = block.to_owned();
                    if let Err(e) = self.new.session_mut().restore(cp) {
                        log::error!("VS1 router bug: checkpoint of PREPARE not restorable: {e}");
                        return Err(NotRun::Unrestorable(SqlError::unknown(e.to_string())));
                    }
                    Err(NotRun::Blocked(block))
                }
                None => Ok(Err(e)),
            },
            Ok(n) => Ok(Ok(n)),
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
        let stmt = Arc::new(tidb_sql::ir::parse_one(sql)?);
        let parse = start.elapsed();
        tidb_metrics::parse_duration(parse);
        let mut started = false;
        match self.route(&stmt, poc, false) {
            Route::New => {
                poc.start_statement(StmtType::Select, parse);
                started = true;
                let fallback = self.mode != NewStackMode::Strict;
                match self
                    .try_new(NewOp::Query(Arc::clone(&stmt), sql), fallback)
                    .await
                {
                    Ok(answered) => return answered,
                    Err(NotRun::Unrestorable(e)) => return Err(e),
                    Err(NotRun::NotIdle) => {}
                    Err(NotRun::Blocked(block)) => {
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
        // Not on the new stack when the InfoSchema versions differ: its column definitions
        // would come from another version, and a later fallback would fail check_columns.
        let candidate = Self::candidate(&stmt, true).and_then(|()| {
            if self.schema_versions_equal(poc) {
                Ok(())
            } else {
                Err(Reason::SchemaVersion)
            }
        });
        let new = match candidate {
            Ok(()) => match self.try_prepare_new(sql).await {
                Ok(r) => Ok(r),
                Err(NotRun::NotIdle) => Err(Reason::NotIdle),
                Err(NotRun::Blocked(block)) => Err(Reason::Blocked(block)),
                Err(NotRun::Unrestorable(e)) => return Err(e),
            },
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
            // A Go-ported error: returned as it is (§4.3 step 3).
            (Ok(Err(e)), old) => {
                if old.is_ok() {
                    poc.close(stmt_id);
                }
                (None, Err(sql_error(&e)))
            }
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
                    tidb_metrics::route(NOWHERE, &block);
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
                    // The statement is started (metrics) only once it is known which engine
                    // answers: the PoC's EXECUTE path starts it itself.
                    poc.begin_command();
                    let fallback = self.mode != NewStackMode::Strict;
                    match self
                        .try_new(NewOp::Execute(new_id, &params), fallback)
                        .await
                    {
                        Ok(answered) => {
                            poc.start_statement(StmtType::Select, Duration::ZERO);
                            self.cursor = false;
                            return answered;
                        }
                        Err(NotRun::Unrestorable(e)) => {
                            poc.start_statement(StmtType::Select, Duration::ZERO);
                            self.cursor = false;
                            return Err(e);
                        }
                        Err(NotRun::Blocked(block)) if self.mode == NewStackMode::Strict => {
                            poc.start_statement(StmtType::Select, Duration::ZERO);
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
                    tidb_metrics::route(NOWHERE, "session");
                    return self.fail(nothing_to_fall_back(&why));
                }
                poc.begin_command();
                let r = self.try_new(NewOp::Execute(new_id, &params), false).await;
                poc.start_statement(StmtType::Select, Duration::ZERO);
                return match r {
                    Ok(answered) => answered,
                    Err(NotRun::Blocked(block)) => {
                        self.fail(nothing_to_fall_back(&format!("blocked-by:{block}")))
                    }
                    Err(NotRun::NotIdle) => {
                        self.fail(nothing_to_fall_back("the new session is not idle"))
                    }
                    Err(NotRun::Unrestorable(e)) => Err(e),
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
