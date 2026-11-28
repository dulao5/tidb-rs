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
//!    (diverged in the sync, [`sync`]), a cursor; then an admissible schema lease and equal
//!    InfoSchema versions ([`Router::schema_check`]);
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
pub mod trace;

use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use tidb_base::Error;
use tidb_base::blocked::blocked_by;
use tidb_metrics::StmtType;
use tidb_mysql_protocol::{ColumnDef, OkInfo, Param, PreparedInfo, RawParams, Response, SqlError};
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

impl NewStackMode {
    /// Whether the schema sync builds the ported domain the new sessions plan with: only
    /// when the new stack is on, so `off` leaves the PoC's sync loop as it was.
    pub fn uses_ported_domain(self) -> bool {
        self != NewStackMode::Off
    }
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
    /// Step 0: the schema sync does not admit statements on the new stack: the schema lease
    /// has expired, the node is not registered, or the ported domain's last reload failed
    /// ([`PocEngine::admissible_schema_version`] is `None`).
    SchemaLease,
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
            Reason::SchemaLease => "schema_lease",
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
    /// A command starts: take the domain's newest InfoSchema. The version compared in step 0
    /// is then the one the statement plans with (§4.1: compared once, fixed for the
    /// statement).
    fn refresh_info_schema(&mut self) {}
    /// The cluster's global variables can be loaded into the new session: the loader's
    /// sysvar cache was built (D06.5 spec §8.3). Until then the router does not load them
    /// (Go's load would block on a rebuild of the cache; the read-only node does not block
    /// a session on TiKV) and keeps the session's statements on the PoC.
    fn globals_ready(&self) -> bool {
        true
    }
    /// `COM_QUERY`. The statement is handed over: the new session's `Compile` writes to it
    /// (D05.3 spec §9.2).
    fn execute<'a>(
        &'a mut self,
        stmt: ast::Stmt,
        sql: &'a str,
    ) -> BoxFut<'a, Result<Response, Error>>;
    /// `COM_STMT_PREPARE` of `sql` (Go's `PrepareStmt` parses the text itself).
    fn prepare<'a>(&'a mut self, sql: &'a str) -> BoxFut<'a, Result<NewPrepared, Error>>;
    /// `COM_STMT_EXECUTE` of statement `stmt_id` (the new session's id). `raw` is the
    /// packet's parameter section, which the new stack decodes itself (Go's
    /// `parseBinaryParams`); `params` is the protocol layer's decoding of it.
    fn execute_prepared<'a>(
        &'a mut self,
        stmt_id: u32,
        params: &'a [Param],
        raw: &'a RawParams,
    ) -> BoxFut<'a, Result<Response, Error>>;
    /// The new session's count of statements whose executors were built and opened (plan
    /// §4.3: a `Blocked` after that is a bug).
    fn executed_stmts(&self) -> u64 {
        self.session().executed_stmts()
    }
    /// `COM_STMT_CLOSE` (Go's `DropPreparedStmt`: the statement is only marked closed).
    fn close(&mut self, stmt_id: u32);
    /// A transaction ended on the PoC (§4.7, closed statements): the new session removes the
    /// statements closed in it, as Go's `cleanRetryInfo` at the end of that statement would.
    fn txn_ended_elsewhere(&mut self) {
        self.session_mut().clean_closed_prepared_stmts();
    }
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
    /// The version of the synced schema while the schema sync admits statements on the new
    /// stack; `None` when it does not (the schema lease has expired, the node is not
    /// registered, the ported domain's last reload failed) or there is no schema sync. Not
    /// the version of a schema the PoC would only use on its ADR 0003 path.
    fn admissible_schema_version(&self) -> Option<i64>;
    /// Whether the PoC session is in an explicit transaction.
    fn in_txn(&self) -> bool;
    /// The state synced into the new session.
    fn values(&self) -> PocValues;
    /// How many assignments of the `SET` it just ran the PoC applied: all of them when the
    /// statement succeeded; when it failed, the ones before the failing assignment, which stay
    /// applied as in Go's `SetExecutor.Next`.
    fn set_applied(&self) -> usize;
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
    /// A statement started that the PoC did not run (the router answered it itself, or the
    /// PoC could not lower it): the PoC's warning list belongs to an older statement and
    /// goes (#512).
    fn clear_warnings(&mut self) {}
    /// The plan-cache decision of a statement the PoC prepared (#512): keep its reason for
    /// every EXECUTE and add the PREPARE's warnings.
    fn set_cacheability(&mut self, stmt_id: u32, c: crate::plan_cache::PrepareCacheability) {
        let _ = (stmt_id, c);
    }
    /// The previous statement's state (VS1 plan §4.5: the PoC writes it into its own
    /// fields), before the PoC runs a statement that may read it.
    fn set_prev_stmt(&mut self, last: &LastStmtInfo) {
        let _ = last;
    }
}

/// The scope of `ADMIN FLUSH <scope> PLAN_CACHE` (`AdminStmt#30` of the grammar; the Rust AST
/// has no ADMIN statement yet, D02.2), `None` for any other statement.
// glue: the ADMIN AST is D02.2 (the grammar alternative and its StatementScope child)
fn admin_flush_plan_cache_scope(stmt: &ast::Stmt) -> Option<tidb_executor::simple::StatementScope> {
    use tidb_executor::simple::StatementScope;
    let ast::Stmt::Unported(node) = stmt else {
        return None;
    };
    if node.kind != "AdminStmt#30" {
        return None;
    }
    let scope = node
        .children
        .first()?
        .downcast_ref::<tidb_parser::gen_types::StatementScope>()?;
    Some(match scope.0.as_ref().map(|n| n.kind) {
        Some("StatementScope#1") => StatementScope::Global,
        Some("StatementScope#2") => StatementScope::Instance,
        // StatementScope#0 (no keyword) and #3 (SESSION): ast.StatementScopeSession
        _ => StatementScope::Session,
    })
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
    /// `COM_QUERY`: the statement (handed over, D05.3 spec §9.2) and its text.
    Query(ast::Stmt, &'a str),
    /// `COM_STMT_EXECUTE` of the new session's statement.
    Execute(u32, &'a [Param], &'a RawParams),
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
    /// The new session's global variables were loaded from the cluster's
    /// `mysql.global_variables` (step 0 keeps the session's statements on the PoC until then:
    /// the fail-safe of D06.5 spec §8.3).
    globals_loaded: bool,
    /// Loading them failed (a SetSession hook rejected a value of the table): Go's load runs
    /// once per session, so the session stays on the PoC.
    globals_failed: bool,
    /// The input of the PoC's plan-cache decision while the global variables are not loaded:
    /// a session with the compiled defaults (made at the first such PREPARE).
    default_vars: Option<Box<tidb_session::Session>>,
    /// `COM_STMT_EXECUTE` of the current command asked for a cursor.
    cursor: bool,
    /// The raw parameter section of the current `COM_STMT_EXECUTE`.
    raw_params: RawParams,
    /// The statements closed (`COM_STMT_CLOSE`) in the current transaction, in order: Go
    /// keeps a closed statement executable until the transaction ends (`DropPreparedStmt` /
    /// `cleanRetryInfo`), so the router keeps its owner and closes the PoC's copy then.
    closed: Vec<u32>,
    /// A statement ran (on either engine) since the last transaction-end check.
    stmt_ended: bool,
    /// The router answered a statement itself ([`Router::fail`]) after the PoC's last one:
    /// the PoC's warning list (#512) is cleared at the end of the command, so a SHOW
    /// WARNINGS that goes to the PoC does not show an older statement's warnings.
    poc_warnings_stale: bool,
    /// The connection's id, for the route trace ([`trace`]).
    conn_id: u64,
}

/// A new-stack error as the client sees it: the code, SQLSTATE and message Go's
/// `clientConn.writeError` (pkg/server/conn.go:1580) puts in the ERR packet, while preserving the
/// original `tidb_base::Error` so `command dispatched failed` emits `[component:code]` and stacks.
/// A TiDB error goes through `terror.ToSQLError`: its MySQL code if its class registered the code,
/// else `ErrUnknown` (1105), with its message; any other error is `ErrUnknown` with its text. The
/// SQLSTATE follows from the code (`mysql.NewErrf`, Go's `MySQLState` table), so a coprocessor
/// error TiKV returns (`ClassTiKV.Synthesize`, distsql's select result) such as 1690 gets
/// `22003`. A boundary stub's error ([`tidb_base::blocked::blocked`]) is tidb-rs's own: it
/// keeps its 1235 (Go has no such error; its class is not in Go's registry).
// glue: the error-to-packet-fields step of clientConn.writeError (the function itself is the server block's part 2, D03.1, with the connection loop)
pub(crate) fn sql_error(e: &Error) -> SqlError {
    if blocked_by(e).is_some() {
        return SqlError::new(
            tidb_base::mysql::ErrNotSupportedYet,
            String::from_utf8_lossy(&e.get_msg()).into_owned(),
        )
        .with_source_err(e.clone());
    }
    let m = if e.is_normalized() {
        tidb_base::terror::to_sql_error(e)
    } else {
        tidb_base::mysql::new_errf(
            tidb_base::mysql::ErrUnknown,
            "%s",
            &[],
            vec![tidb_base::gofmt::Arg::go_string(e.error())],
        )
    };
    let err = SqlError::new(m.code, String::from_utf8_lossy(&m.message).into_owned())
        .with_source_err(e.clone());
    debug_assert_eq!(err.state, m.state, "the SQLSTATE of code {}", m.code);
    err
}

/// Emit Go `session.go:1590` `WARN` `"parse SQL failed"` (preserving raw `\n`, applying
/// `redact::string`) and attach a `[parser:1064]` `source_err` on `SqlError` for
/// `command dispatched failed`.
pub(crate) fn log_and_wrap_parse_error(
    conn_id: u64,
    session_alias: &str,
    redact_mode: &str,
    sql: &str,
    err: SqlError,
) -> SqlError {
    let src = err.source_err.clone().unwrap_or_else(|| {
        // `err.message` is already formatted by the parser; use "%s" instead of ErrParse's
        // "%s %s" template so we do not append an extra trailing space.
        tidb_base::terror::ClassParser
            .new_std(tidb_base::errno::ErrParse)
            .fast_gen("%s", tidb_base::go_args![&err.message])
    });
    let sql_log = tidb_base::redact::string(redact_mode, sql);
    tidb_base::log_warn!(
        "parse SQL failed",
        "conn" => conn_id,
        "session_alias" => session_alias,
        "error" => &src,
        "SQL" => &sql_log,
    );
    err.with_source_err(src)
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

/// A SetSession hook rejected a value of `mysql.global_variables` while a new session loaded
/// it: logged once per process for each error, with the variables whose value is rejected
/// (found by setting each value of the session map on a session with the compiled
/// defaults), so that an operator sees why sessions stay on the PoC.
// glue: the router's log of a failed loadCommonGlobalVariablesIfNeeded (D06.5 spec §8.3)
fn log_rejected_globals(session: &tidb_session::Session, err: &Error) {
    static LOGGED: std::sync::LazyLock<parking_lot::Mutex<BTreeSet<String>>> =
        std::sync::LazyLock::new(Default::default);
    let key = err.to_string();
    {
        let mut logged = LOGGED.lock();
        if logged.len() >= 64 || !logged.insert(key) {
            return;
        }
    }
    let mut rejected = Vec::new();
    if let Some(Ok(cache)) = session.vars().global_vars_accessor.session_cache()
        && let Ok(mut probe) = crate::plan_cache::compiled_defaults_session()
    {
        let mut names: Vec<_> = cache.into_iter().collect();
        names.sort();
        for (name, value) in names {
            if let Err(e) = probe
                .vars_mut()
                .set_system_var_with_relaxed_validation(&name, &value)
                && !tidb_sessionctx::variable::error::ErrUnknownSystemVar.equal(&e)
            {
                rejected.push(name);
            }
        }
    }
    let rejected = rejected.join(",");
    tidb_base::log_warn!(
        "VS1 router: a value of mysql.global_variables is rejected by its hook, sessions stay on the PoC (logged once)",
        "variables" => rejected.as_str(),
        "error" => err,
    );
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
            globals_loaded: false,
            globals_failed: false,
            default_vars: None,
            cursor: false,
            raw_params: RawParams::default(),
            closed: Vec::new(),
            stmt_ended: false,
            poc_warnings_stale: false,
            conn_id: 0,
        };
        r.ensure_globals();
        let names = [tidb_sql::ir::SetAssignment::Names {
            charset: None,
            collation: None,
        }];
        let vars = r.new.session_mut().vars_mut();
        sync::sync_set(vars, &mut r.diverged, &names, poc);
        sync::sync_db(vars, poc);
        r
    }

    /// Go's session loads the global values of its variables at its first statement
    /// (`loadCommonGlobalVariablesIfNeeded`); the router loads them when the session opens
    /// and, while the loader's sysvar cache is not built yet, retries at the start of each
    /// command until it is (Go's load would block on `rebuildSysVarCacheIfNeeded`; D06.5
    /// spec §8.3). Until then step 0 keeps the session's statements on the PoC. Go's load
    /// skips the variables already set: the synced ones are set before or after it and win
    /// the same way. Once loaded, step 0 compares `tidb_txn_mode` with the value the session
    /// has then (the cluster's global value; the router syncs no `tidb_txn_mode`). If a
    /// SetSession hook rejects a value of the table, the session's variables are not the
    /// cluster's, so the session stays on the PoC for its lifetime (Go's load does not run
    /// twice either).
    // glue: when the router runs loadCommonGlobalVariablesIfNeeded (D06.5 spec §8.3)
    fn ensure_globals(&mut self) {
        if self.globals_loaded || self.globals_failed || !self.new.globals_ready() {
            return;
        }
        match self
            .new
            .session_mut()
            .load_common_global_variables_if_needed()
        {
            Ok(()) => {
                self.globals_loaded = true;
                self.initial_txn_mode = self.new.session().vars().txn_mode.clone();
            }
            Err(e) => {
                self.globals_failed = true;
                log_rejected_globals(self.new.session(), &e);
            }
        }
    }

    /// The connection's id (the route trace and new-stack SessionVars record under it).
    pub fn set_connection_id(&mut self, id: u64) {
        self.conn_id = id;
        self.new.session_mut().vars_mut().connection_id = id;
    }

    /// A routing decision: `tidbrs_route_total{engine, reason}` and the route trace.
    fn note(&self, engine: &'static str, reason: &str) {
        tidb_metrics::route(engine, reason);
        trace::record(self.conn_id, engine, reason);
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
        if !self.globals_loaded {
            return Some("global variables not loaded");
        }
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
        // Go's `tx_read_ts` hook keeps a `TxnReadTS` with ts 0 when the variable is empty
        // (its default, set when the global values are loaded): only a read ts counts.
        if vars
            .state
            .txn_read_ts
            .as_ref()
            .is_some_and(|t| t.peak_txn_read_ts() != 0)
        {
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

    /// Parser options for the active session (`sql_mode` and parser config).
    fn parse_options(&self) -> tidb_parser::ParseOptions {
        let vars = self.new.session().vars();
        tidb_parser::ParseOptions {
            window_functions: vars.enable_window_function,
            sql_mode: tidb_base::mysql::SQLMode(vars.sql_mode),
        }
    }

    /// Step 0 for a PREPARE: the new session's global variables are loaded (the other session
    /// conditions do not apply to a PREPARE, §4.7: both sides prepare it).
    fn globals_check(&self) -> Result<(), Reason> {
        if self.globals_loaded {
            Ok(())
        } else {
            Err(Reason::Session("global variables not loaded"))
        }
    }

    /// Step 0: the new stack may plan with its InfoSchema. The new stack gets it from the
    /// ported domain that the PoC domain's sync loop reloads (§4.1); a session without a
    /// domain (no schema sync) has none, and its planner returns `Blocked`, so the check
    /// passes. Otherwise the schema sync must admit statements on the new stack
    /// ([`Reason::SchemaLease`]): the new stack has no schema validator and no statement
    /// guard, so past the schema lease, after deregistering, or after a failed reload of the
    /// ported domain only the PoC's paths are safe. And the versions must be equal
    /// ([`Reason::SchemaVersion`]).
    fn schema_check(&self, poc: &dyn PocEngine) -> Result<(), Reason> {
        let Some(v) = self.new.info_schema_version() else {
            return Ok(());
        };
        match poc.admissible_schema_version() {
            None => Err(Reason::SchemaLease),
            Some(pv) if pv == v => Ok(()),
            Some(_) => Err(Reason::SchemaVersion),
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
        if let Err(reason) = self.schema_check(poc) {
            return Route::Poc(reason);
        }
        match Self::candidate(stmt, allow_params) {
            Ok(()) => Route::New,
            Err(r) => Route::Poc(r),
        }
    }

    /// The new stack finished a statement: record its state and copy it to the PoC side.
    fn finish_new(&mut self) {
        self.last = LastStmtInfo::from_new(self.new.session().vars());
        self.stmt_ended = true;
    }

    /// After a command: if a statement ran and no transaction is open on either side, the
    /// transaction (an explicit one, or the autocommit statement itself) has ended, and the
    /// statements closed in it are removed now (§4.7): the PoC's copies are closed, the new
    /// session removes its own (`cleanRetryInfo`; a no-op when the new stack ran the
    /// statement and has already done it), and the router forgets the owners.
    fn end_of_command(&mut self, poc: &mut dyn PocEngine) {
        if !std::mem::take(&mut self.stmt_ended) || self.closed.is_empty() {
            return;
        }
        if poc.in_txn() || self.new.session().in_txn() {
            return;
        }
        self.new.txn_ended_elsewhere();
        for stmt_id in std::mem::take(&mut self.closed) {
            match self.owners.remove(&stmt_id) {
                Some(Owner::Both { .. }) | Some(Owner::Poc(_)) => poc.close(stmt_id),
                Some(Owner::New { .. }) | None => {}
            }
        }
    }

    /// The PoC finished a statement with `result`: record its state and copy it into the new
    /// session.
    fn finish_poc(&mut self, result: &Result<Response, SqlError>) {
        self.last = LastStmtInfo::from_poc(&self.last, result);
        self.last.apply_to(self.new.session_mut().vars_mut());
        self.stmt_ended = true;
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
            tidb_base::log_error!(
                "VS1 router bug: the new session is not idle at the start of a statement; the PoC runs it",
            );
            let label = Reason::NotIdle.label();
            if fallback {
                self.note(Engine::Poc.label(), label);
            } else {
                self.note(NOWHERE, label);
            }
            return Err(NotRun::NotIdle);
        }
        let cp = self.new.session().checkpoint();
        let executed = self.new.executed_stmts();
        let r = match op {
            NewOp::Query(stmt, sql) => self.new.execute(stmt, sql).await,
            NewOp::Execute(id, params, raw) => self.new.execute_prepared(id, params, raw).await,
        };
        match r {
            Ok(resp) => {
                drop(cp);
                self.finish_new();
                self.note(Engine::New.label(), "ok");
                Ok(Ok(resp))
            }
            Err(e) => match blocked_by(&e) {
                Some(block) if fallback_after_open(executed, self.new.executed_stmts()) => {
                    // Plan §4.3: a fallback only before an executor opened; after that the
                    // statement may have read data and changed the session, and running it
                    // again on the PoC is not allowed. A boundary stub there is a bug of the
                    // new stack: the client gets its 1235 (constitution article 5), the
                    // statement ends on the new stack.
                    tidb_base::log_error!(
                        "VS1 router bug: blocked after the new stack's executors opened",
                        "block" => block,
                    );
                    self.note(Engine::New.label(), "bug_blocked_after_open");
                    drop(cp);
                    self.finish_new();
                    Ok(Err(sql_error(&e)))
                }
                Some(block) => {
                    let block = block.to_owned();
                    if let Err(e) = self.new.session_mut().restore(cp) {
                        // Checked idle above: a bug of the checkpoint.
                        self.note(Engine::New.label(), "error");
                        return Err(NotRun::Unrestorable(SqlError::unknown(e.to_string())));
                    }
                    if fallback {
                        self.note(Engine::Poc.label(), &block);
                    } else {
                        self.note(NOWHERE, &block);
                    }
                    Err(NotRun::Blocked(block))
                }
                None => {
                    drop(cp);
                    self.finish_new();
                    self.note(Engine::New.label(), "error");
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
            tidb_base::log_error!(
                "VS1 router bug: the new session is not idle at a PREPARE; only the PoC prepares it",
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
                        let err_str = e.to_string();
                        tidb_base::log_error!(
                            "VS1 router bug: checkpoint of PREPARE not restorable",
                            "error" => err_str.as_str(),
                        );
                        return Err(NotRun::Unrestorable(SqlError::unknown(err_str)));
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
        self.poc_warnings_stale = true;
        r
    }

    /// The end of a command: drop the PoC's warnings when the router answered the statement
    /// itself (see `poc_warnings_stale`).
    fn flush_poc_warnings(&mut self, poc: &mut dyn PocEngine) {
        if std::mem::take(&mut self.poc_warnings_stale) {
            poc.clear_warnings();
        }
    }

    /// `COM_QUERY`.
    pub async fn query(
        &mut self,
        poc: &mut dyn PocEngine,
        sql: &str,
    ) -> Result<Response, SqlError> {
        self.new.refresh_info_schema();
        self.ensure_globals();
        let r = self.query_inner(poc, sql).await;
        self.flush_poc_warnings(poc);
        self.end_of_command(poc);
        r
    }

    /// [`Router::query`] before the transaction-end check.
    async fn query_inner(
        &mut self,
        poc: &mut dyn PocEngine,
        sql: &str,
    ) -> Result<Response, SqlError> {
        self.cursor = false;
        poc.begin_command();
        let opts = self.parse_options();
        let start = Instant::now();
        let stmt = match tidb_sql::ir::parse_one_with(sql, &opts) {
            Ok(s) => s,
            Err(e) => {
                let vars = self.new.session().vars();
                if !vars.in_restricted_sql {
                    return Err(log_and_wrap_parse_error(
                        self.conn_id,
                        &vars.session_alias,
                        &vars.enable_redact_log,
                        sql,
                        e,
                    ));
                }
                return Err(e);
            }
        };
        let parse = start.elapsed();
        tidb_metrics::parse_duration(parse);
        if let Some(scope) = admin_flush_plan_cache_scope(&stmt) {
            // The plan cache lives in the new stack (the plan-cache exception A3, decision 8):
            // the new session runs the statement in every mode (the prototype has no plan
            // cache and fences ADMIN statements).
            poc.start_statement(StmtType::General, parse);
            self.note(Engine::New.label(), "ok");
            let r = self
                .new
                .session_mut()
                .admin_flush_plan_cache(&stmt, sql, scope);
            self.finish_new();
            return match r {
                Ok(()) => Ok(Response::Ok(OkInfo {
                    warnings: u16::try_from(self.last.warning_count).unwrap_or(u16::MAX),
                    ..OkInfo::default()
                })),
                // Go's `writeError`: an `errors.New` error is ErrUnknown (1105).
                Err(e) => Err(sql_error(&e)),
            };
        }
        let mut started = false;
        let stmt = match self.route(&stmt, poc, false) {
            Route::New => {
                poc.start_statement(StmtType::Select, parse);
                started = true;
                let fallback = self.mode != NewStackMode::Strict;
                match self.try_new(NewOp::Query(stmt, sql), fallback).await {
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
                // The new stack took the statement (its `Compile` writes to it, D05.3 spec
                // §9.2): the PoC lowers a new parse of the same text. Only statements that
                // fall back are parsed twice.
                tidb_sql::ir::parse_one_with(sql, &opts)?
            }
            Route::Poc(reason) => {
                self.note(Engine::Poc.label(), reason.label());
                stmt
            }
        };
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
        // §4.5: SHOW WARNINGS / SHOW ERRORS go to the engine that ran the previous
        // statement. Go's SHOW WARNINGS keeps the previous statement's warnings (its
        // `ResetContextOfStmt` copies them), so the previous statement stays the new stack's.
        if let Statement::ShowWarnings { errors } = &lowered.stmt
            && self.last.engine == Engine::New
        {
            return Ok(Response::ResultSet(self.new_stack_warnings(*errors)));
        }
        poc.set_prev_stmt(&self.last);
        let result = poc.run(&lowered).await;
        let vars = self.new.session_mut().vars_mut();
        match &lowered.stmt {
            // A failing SET keeps the assignments before the failing one (Go's
            // `SetExecutor.Next`), so the new session gets those too.
            Statement::Set(assignments) => {
                let applied = if result.is_ok() {
                    assignments.len()
                } else {
                    poc.set_applied().min(assignments.len())
                };
                sync::sync_set(
                    vars,
                    &mut self.diverged,
                    &assignments[..applied],
                    &poc.values(),
                )
            }
            Statement::Use(_) if result.is_ok() => sync::sync_db(vars, &poc.values()),
            _ => {}
        }
        self.finish_poc(&result);
        result
    }

    /// The rows of `SHOW WARNINGS` (`errors`: `SHOW ERRORS`) for a previous statement the
    /// new stack ran, from its statement context, as Go's `ShowExec.fetchShowWarnings` makes
    /// them: a TiDB error is shown with its MySQL code and message (`terror.ToSQLError`),
    /// any other error with `ErrUnknown` and its text. The columns are the PoC's (the same
    /// definitions as Go's SHOW WARNINGS).
    // glue: VS1 plan §4.5 (SHOW WARNINGS answered by the engine that ran the previous statement)
    fn new_stack_warnings(&self, errors: bool) -> tidb_mysql_protocol::ResultSet {
        crate::warnings_result_set(
            self.new
                .session()
                .vars()
                .stmt_ctx()
                .get_warnings()
                .as_slice(),
            errors,
        )
    }

    /// `COM_INIT_DB` / the handshake's database, after the PoC accepted it.
    pub fn use_db(&mut self, poc: &dyn PocEngine) {
        sync::sync_db(self.new.session_mut().vars_mut(), &poc.values());
    }

    /// `COM_STMT_PREPARE` (§4.7): both sides prepare; the table decides the owner.
    ///
    /// A PREPARE ends a statement like a query: Go's `PrepareStmt` calls `rollbackOnError`
    /// after the prepare executor whether it succeeded or not (the parse is inside it), which
    /// outside a transaction is `RollbackTxn` → `cleanRetryInfo`. So the transaction-end
    /// check runs after it too.
    pub async fn prepare(
        &mut self,
        poc: &mut dyn PocEngine,
        stmt_id: u32,
        sql: &str,
    ) -> Result<PreparedInfo, SqlError> {
        self.new.refresh_info_schema();
        self.ensure_globals();
        let r = self.prepare_inner(poc, stmt_id, sql).await;
        self.stmt_ended = true;
        self.end_of_command(poc);
        r
    }

    /// [`Router::prepare`] before the transaction-end check.
    async fn prepare_inner(
        &mut self,
        poc: &mut dyn PocEngine,
        stmt_id: u32,
        sql: &str,
    ) -> Result<PreparedInfo, SqlError> {
        let opts = self.parse_options();
        let stmt = tidb_sql::ir::parse_one_with(sql, &opts)?;
        // Not on the new stack when the InfoSchema versions differ: its column definitions
        // would come from another version, and a later fallback would fail check_columns.
        // Nor when the schema lease is not admissible (step 0). Nor before the session's global
        // variables are loaded (step 0): its variables, `sql_mode` included, are not the
        // cluster's.
        let candidate = Self::candidate(&stmt, true)
            .and_then(|()| self.globals_check())
            .and_then(|()| self.schema_check(poc));
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
            Err(e) => {
                // Go would have started the PREPARE: the PoC's older warnings go.
                poc.clear_warnings();
                Err(e)
            }
        };
        // SHOW WARNINGS after the PREPARE goes to the engine whose statement context holds
        // its warnings: the new session's when it prepared the statement (or returned a
        // Go-ported error), else the PoC's.
        self.last.engine = match &new {
            Ok(_) => Engine::New,
            Err(_) => Engine::Poc,
        };
        // The PoC keeps the statement: take Go's prepare-time plan-cache decision for its
        // EXECUTEs (#512).
        let old = match (old, &new) {
            (Ok(info), Ok(Err(_))) => Ok(info),
            (Ok(info), _) => match self.prepare_cacheability(poc, &stmt).await {
                Ok(c) => {
                    poc.set_cacheability(stmt_id, c);
                    Ok(info)
                }
                // A boundary the decision reaches: the PoC's copy cannot answer as Go
                // would, so it goes. The new stack keeps the statement when it prepared it
                // (owner `New`); otherwise the PREPARE fails with the boundary's 1235.
                Err(e) => {
                    poc.close(stmt_id);
                    Err(e)
                }
            },
            (Err(e), _) => Err(e),
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

    /// Go's prepare-time plan-cache decision for a statement the PoC prepared (#512): with
    /// the new session's synced variables, and the new session's InfoSchema when it is the
    /// one the PoC's schema agrees with (step 0's version check); otherwise no InfoSchema,
    /// which leaves out the table checks (a known difference, plan cache spec §5).
    // glue: the router's input to the prototype's plan-cache decision (#512)
    async fn prepare_cacheability(
        &mut self,
        poc: &mut dyn PocEngine,
        stmt: &ast::Stmt,
    ) -> Result<crate::plan_cache::PrepareCacheability, SqlError> {
        let with_is = self.new.info_schema_version().is_some() && self.schema_check(poc).is_ok();
        if with_is {
            // The decision is synchronous: load the statement's tables first, as the new
            // session does before planning.
            let load = tidb_session::session::load_stmt_tables(self.new.session().sctx(), stmt);
            load.await;
        }
        self.decide_cacheability(with_is, stmt)
    }

    /// [`Router::prepare_cacheability`] after the table loads. Before the session's global
    /// variables are loaded, the variables are the compiled defaults' (as with
    /// `--new-stack=off` before the sysvar cache is built), not the new session's unloaded
    /// ones.
    fn decide_cacheability(
        &mut self,
        with_is: bool,
        stmt: &ast::Stmt,
    ) -> Result<crate::plan_cache::PrepareCacheability, SqlError> {
        if !self.globals_loaded {
            if self.default_vars.is_none() {
                self.default_vars = Some(Box::new(crate::plan_cache::compiled_defaults_session()?));
            }
            let vars = self.default_vars.as_ref().expect("set above").vars();
            return crate::plan_cache::prepare_time_cacheability(vars, stmt, None);
        }
        let session = self.new.session();
        if !with_is {
            return crate::plan_cache::prepare_time_cacheability(session.vars(), stmt, None);
        }
        let is = session.sctx().get_info_schema();
        crate::plan_cache::prepare_time_cacheability(
            session.vars(),
            stmt,
            Some(is.as_ref() as &dyn tidb_planner::core::plan_cache_utils::InfoSchemaLookup),
        )
    }

    /// `COM_STMT_EXECUTE` (§4.7).
    pub async fn execute(
        &mut self,
        poc: &mut dyn PocEngine,
        stmt_id: u32,
        params: Vec<Param>,
    ) -> Result<Response, SqlError> {
        self.new.refresh_info_schema();
        self.ensure_globals();
        let r = self.execute_inner(poc, stmt_id, params).await;
        self.flush_poc_warnings(poc);
        self.end_of_command(poc);
        r
    }

    /// [`Router::execute`] before the transaction-end check.
    async fn execute_inner(
        &mut self,
        poc: &mut dyn PocEngine,
        stmt_id: u32,
        params: Vec<Param>,
    ) -> Result<Response, SqlError> {
        let raw = std::mem::take(&mut self.raw_params);
        let owner = self.owners.get(&stmt_id).cloned();
        let result = match owner {
            // Unknown here: the PoC reports Go's error for an unknown statement.
            None => poc.execute(stmt_id, params).await,
            Some(Owner::Poc(reason)) => match reason {
                Reason::Blocked(block) if self.mode == NewStackMode::Strict => {
                    self.note(NOWHERE, &block);
                    self.cursor = false;
                    return self.fail(sql_error(&tidb_base::blocked::blocked(
                        &block,
                        "this prepared statement on the new stack",
                    )));
                }
                reason => {
                    self.note(Engine::Poc.label(), reason.label());
                    poc.execute(stmt_id, params).await
                }
            },
            Some(Owner::Both { new_id, columns }) => {
                if let Some(c) = self.session_condition(poc) {
                    self.note(Engine::Poc.label(), Reason::Session(c).label());
                    poc.execute(stmt_id, params).await
                } else if let Err(reason) = self.schema_check(poc) {
                    self.note(Engine::Poc.label(), reason.label());
                    poc.execute(stmt_id, params).await
                } else {
                    // The statement is started (metrics) only once it is known which engine
                    // answers: the PoC's EXECUTE path starts it itself.
                    poc.begin_command();
                    let fallback = self.mode != NewStackMode::Strict;
                    match self
                        .try_new(NewOp::Execute(new_id, &params, &raw), fallback)
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
                // The metric's reason: the schema lease's like its other routes; any other
                // step-0 condition is counted as `session`, as before.
                let why = match self.session_condition(poc) {
                    Some(c) => Some((Reason::Session(c), format!("session condition: {c}"))),
                    None => match self.schema_check(poc) {
                        Err(Reason::SchemaLease) => Some((
                            Reason::SchemaLease,
                            "the schema lease is not admissible".to_owned(),
                        )),
                        Err(_) => Some((
                            Reason::Session("schema version"),
                            "InfoSchema versions differ".to_owned(),
                        )),
                        Ok(()) => None,
                    },
                };
                self.cursor = false;
                if let Some((reason, why)) = why {
                    self.note(NOWHERE, reason.label());
                    return self.fail(nothing_to_fall_back(&why));
                }
                poc.begin_command();
                let r = self
                    .try_new(NewOp::Execute(new_id, &params, &raw), false)
                    .await;
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

    /// `COM_STMT_CLOSE`: to the owner, to both when both prepared it. As Go
    /// (`DropPreparedStmt`), the statement stays executable until the transaction ends: the
    /// new session only marks it closed, and the PoC's copy is closed and the owner
    /// forgotten at the end of the next statement that ends a transaction
    /// ([`Router::end_of_command`]). A statement the router does not know goes to the PoC at
    /// once (it reports Go's error).
    pub fn close(&mut self, poc: &mut dyn PocEngine, stmt_id: u32) {
        match self.owners.get(&stmt_id) {
            Some(Owner::Both { new_id, .. }) | Some(Owner::New { new_id }) => {
                self.new.close(*new_id)
            }
            Some(Owner::Poc(_)) => {}
            None => {
                poc.close(stmt_id);
                return;
            }
        }
        if !self.closed.contains(&stmt_id) {
            self.closed.push(stmt_id);
        }
    }

    /// The flags of the coming `COM_STMT_EXECUTE` (`CURSOR_TYPE_READ_ONLY` is step 0).
    pub fn set_execute_flags(&mut self, flags: u8) {
        self.cursor = flags & tidb_mysql_protocol::constants::cursor_type::READ_ONLY != 0;
    }

    /// The raw parameters of the coming `COM_STMT_EXECUTE` (for the new stack's
    /// `parseBinaryParams`).
    pub fn set_execute_raw_params(&mut self, raw: RawParams) {
        self.raw_params = raw;
    }

    /// The owner of a prepared statement, for tests.
    #[cfg(test)]
    fn owner(&self, stmt_id: u32) -> Option<&Owner> {
        self.owners.get(&stmt_id)
    }
}
