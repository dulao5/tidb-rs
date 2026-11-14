//! Router tests (VS1-02): the routing order with one case per step-0 condition, the AST
//! pre-check, fallback with state restore (§4.6), `strict`, the previous-statement state in
//! both orders (§4.5), the prepared-statement combinations and stmt-id routing (§4.7), and
//! the session-state sync (§4.1).
//!
//! The PoC is a stand-in ([`FakePoc`]: no cluster); the new stack is either the real phase-0
//! [`NewSession`] or a scripted one ([`FakeNew`]) around a real `tidb_session::Session`, so
//! that the paths the phase-0 session never takes (answers, Go-ported errors, `Blocked` after
//! writing session state) are covered too.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::executor::block_on;
use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_base::errors;
use tidb_metrics::StmtType;
use tidb_mysql_protocol::constants::cursor_type;
use tidb_mysql_protocol::{ColumnDef, ColumnType, OkInfo, Param, PreparedInfo, Response};
use tidb_mysql_protocol::{ResultSet, SqlError, Value};
use tidb_parser::ast;
use tidb_session::{Dialect, Session};
use tidb_sessionctx::stmtctx::new_stmt_ctx;
use tidb_sessionctx::variable::session_state::TxnReadTS;
use tidb_sql::ir::SetAssignment;
use tidb_sql::{Lowered, Statement};

use super::newstack::{DefaultGlobals, NewSession};
use super::*;

// ---------------------------------------------------------------------------------------------
// The PoC stand-in.

/// What the stand-in PoC did.
#[derive(Default)]
struct FakePoc {
    values: PocValues,
    schema_version: Option<i64>,
    in_txn: bool,
    /// Rows a SELECT returns.
    rows: usize,
    /// Column definitions of its results (and of its PREPARE).
    columns: Vec<ColumnDef>,
    prepare_fails: bool,
    prepared: HashMap<u32, Lowered>,
    log: Vec<String>,
}

fn col(name: &str) -> ColumnDef {
    ColumnDef::new(name, ColumnType::LongLong)
}

fn poc() -> FakePoc {
    FakePoc {
        values: PocValues {
            current_db: None,
            charset: "utf8mb4".into(),
            collation: "utf8mb4_bin".into(),
            results_charset: Some("utf8mb4".into()),
        },
        schema_version: Some(1),
        rows: 3,
        columns: vec![col("c")],
        ..FakePoc::default()
    }
}

impl FakePoc {
    fn result_set(&self) -> Response {
        Response::ResultSet(ResultSet {
            columns: self.columns.clone(),
            rows: vec![vec![Value::Int(1)]; self.rows],
            warnings: 0,
        })
    }
}

impl PocEngine for FakePoc {
    fn schema_version(&self) -> Option<i64> {
        self.schema_version
    }
    fn in_txn(&self) -> bool {
        self.in_txn
    }
    fn values(&self) -> PocValues {
        self.values.clone()
    }
    fn begin_command(&mut self) {}
    fn start_statement(&mut self, kind: StmtType, _parse: Duration) {
        self.log.push(format!("start {kind:?}"));
    }
    fn run<'a>(&'a mut self, lowered: &'a Lowered) -> BoxFut<'a, Result<Response, SqlError>> {
        Box::pin(async move {
            match &lowered.stmt {
                Statement::Select(_) => {
                    self.log.push("run select".into());
                    Ok(self.result_set())
                }
                Statement::Set(assignments) => {
                    self.log.push("run set".into());
                    for a in assignments {
                        if let SetAssignment::Names { charset, collation } = a {
                            let cs = charset.clone().unwrap_or_else(|| "utf8mb4".into());
                            self.values.collation =
                                collation.clone().unwrap_or_else(|| format!("{cs}_bin"));
                            self.values.results_charset = Some(cs.clone());
                            self.values.charset = cs;
                        }
                    }
                    Ok(Response::Ok(OkInfo::default()))
                }
                Statement::Use(db) => {
                    self.log.push("run use".into());
                    self.values.current_db = Some(db.clone());
                    Ok(Response::Ok(OkInfo::default()))
                }
                _ => Err(SqlError::not_supported("this statement")),
            }
        })
    }
    fn prepare(
        &mut self,
        stmt_id: u32,
        lowered: Lowered,
    ) -> BoxFut<'_, Result<PreparedInfo, SqlError>> {
        Box::pin(async move {
            self.log.push(format!("prepare {stmt_id}"));
            if self.prepare_fails {
                return Err(SqlError::not_supported("this statement on the PoC"));
            }
            let param_count = lowered.param_count as u16;
            self.prepared.insert(stmt_id, lowered);
            Ok(PreparedInfo {
                param_count,
                columns: self.columns.clone(),
            })
        })
    }
    fn execute(
        &mut self,
        stmt_id: u32,
        _params: Vec<Param>,
    ) -> BoxFut<'_, Result<Response, SqlError>> {
        Box::pin(async move {
            self.log.push(format!("execute {stmt_id}"));
            if !self.prepared.contains_key(&stmt_id) {
                return Err(SqlError::unknown_stmt(stmt_id, "stmt_execute"));
            }
            Ok(self.result_set())
        })
    }
    fn close(&mut self, stmt_id: u32) {
        self.log.push(format!("close {stmt_id}"));
        self.prepared.remove(&stmt_id);
    }
}

// ---------------------------------------------------------------------------------------------
// The scripted new stack.

/// What the scripted new stack answers.
#[derive(Clone, Debug, PartialEq)]
enum Answer {
    /// A result set with the given column; writes session state like Go's statement path.
    Rows,
    /// A Go-ported error (1054).
    GoError,
    /// Writes session state, then hits the boundary stub of this block.
    Blocked(&'static str),
}

#[derive(Default)]
struct Script {
    answer: Option<Answer>,
    prepare: Option<Answer>,
    version: Option<i64>,
    columns: Vec<ColumnDef>,
    log: Vec<String>,
    next_id: u32,
}

struct FakeNew {
    session: Session,
    script: Arc<Mutex<Script>>,
}

/// What Go's statement path writes before it opens an executor (and a SELECT after it).
fn write_statement_state(s: &mut Session) {
    let mut sc = new_stmt_ctx();
    sc.in_select_stmt = true;
    // ResetContextOfStmt carries the previous statement's LAST_INSERT_ID() over.
    sc.prev_last_insert_id = s.vars().stmt_ctx().prev_last_insert_id;
    s.vars_mut().set_stmt_ctx(Arc::new(sc));
    let v = s.vars_mut();
    v.stmt_ctx()
        .append_warning(errors::new("new-stack warning"));
    v.stmt_ctx().append_error(errors::new("new-stack error"));
    v.stmt_ctx().set_use_chunk_alloc();
    v.found_in_plan_cache = true;
    v.found_in_binding = true;
    v.last_found_rows = 5;
    v.state.last_query_info.start_ts = 99;
    v.last_txn_info = "{\"start_ts\":99}".into();
    v.state.txn_ctx.start_ts = 99;
}

impl FakeNew {
    fn run(&mut self, answer: Option<Answer>, what: String) -> Result<Response, Error> {
        let mut script = self.script.lock().unwrap();
        script.log.push(what);
        match answer.expect("the scripted new stack was called unexpectedly") {
            Answer::Rows => {
                write_statement_state(&mut self.session);
                Ok(Response::ResultSet(ResultSet {
                    columns: script.columns.clone(),
                    rows: vec![vec![Value::Int(2)]],
                    warnings: 2,
                }))
            }
            Answer::GoError => Err(errors::normalize(
                "Unknown column 'x' in 'field list'",
                [errors::mysql_error_code(1054)],
            )),
            Answer::Blocked(block) => {
                write_statement_state(&mut self.session);
                Err(blocked(block, "something"))
            }
        }
    }
}

impl NewStack for FakeNew {
    fn session(&self) -> &Session {
        &self.session
    }
    fn session_mut(&mut self) -> &mut Session {
        &mut self.session
    }
    fn info_schema_version(&self) -> Option<i64> {
        self.script.lock().unwrap().version
    }
    fn execute<'a>(&'a mut self, _stmt: &'a ast::Stmt) -> BoxFut<'a, Result<Response, Error>> {
        let answer = self.script.lock().unwrap().answer.clone();
        let r = self.run(answer, "execute".into());
        Box::pin(async move { r })
    }
    fn prepare<'a>(&'a mut self, _stmt: &'a ast::Stmt) -> BoxFut<'a, Result<NewPrepared, Error>> {
        let answer = self.script.lock().unwrap().prepare.clone();
        let r = self.run(answer, "prepare".into()).map(|_| {
            let mut script = self.script.lock().unwrap();
            script.next_id += 1;
            NewPrepared {
                stmt_id: 100 + script.next_id,
                info: PreparedInfo {
                    param_count: 0,
                    columns: script.columns.clone(),
                },
            }
        });
        Box::pin(async move { r })
    }
    fn execute_prepared<'a>(
        &'a mut self,
        stmt_id: u32,
        _params: &'a [Param],
    ) -> BoxFut<'a, Result<Response, Error>> {
        let answer = self.script.lock().unwrap().answer.clone();
        let r = self.run(answer, format!("execute {stmt_id}"));
        Box::pin(async move { r })
    }
    fn close(&mut self, stmt_id: u32) {
        self.script
            .lock()
            .unwrap()
            .log
            .push(format!("close {stmt_id}"));
    }
}

fn scripted(mode: NewStackMode, poc: &FakePoc) -> (Router, Arc<Mutex<Script>>) {
    let script = Arc::new(Mutex::new(Script {
        columns: vec![col("c")],
        ..Script::default()
    }));
    let new = FakeNew {
        session: Session::new(Dialect::MySQL, Box::new(DefaultGlobals)),
        script: script.clone(),
    };
    (Router::new(mode, Box::new(new), &poc.values()), script)
}

fn real(mode: NewStackMode, poc: &FakePoc) -> Router {
    Router::new(mode, Box::new(NewSession::new()), &poc.values())
}

fn parse(sql: &str) -> ast::Stmt {
    tidb_sql::ir::parse_one(sql).unwrap()
}

fn rows(r: &Result<Response, SqlError>) -> Option<(usize, Vec<ColumnDef>)> {
    match r {
        Ok(Response::ResultSet(rs)) => Some((rs.rows.len(), rs.columns.clone())),
        _ => None,
    }
}

fn get(router: &mut Router, var: &str) -> String {
    router
        .new
        .session_mut()
        .vars_mut()
        .get_session_or_global_system_var(var)
        .unwrap()
}

const POINT: &str = "SELECT c FROM sbtest1 WHERE id = 1";

/// The new session's snapshot, comparable across two sessions: without the random seed of
/// `RAND()` that every session draws when it opens.
fn snap(r: &Router) -> String {
    let s = r.new.session().state_snapshot();
    let Some(i) = s.find("rng: MysqlRng {") else {
        return s;
    };
    let j = i + s[i..].find('}').expect("closing brace") + 1;
    format!("{}{}", &s[..i], &s[j..])
}

// ---------------------------------------------------------------------------------------------
// Mode, steps 1 and 2.

#[test]
fn mode_parsing() {
    assert_eq!("off".parse(), Ok(NewStackMode::Off));
    assert_eq!("vs1".parse(), Ok(NewStackMode::Vs1));
    assert_eq!("strict".parse(), Ok(NewStackMode::Strict));
    assert!("on".parse::<NewStackMode>().is_err());
    assert_eq!(NewStackMode::default(), NewStackMode::Off);
}

#[test]
fn kind_and_ast_precheck() {
    let p = poc();
    let r = real(NewStackMode::Vs1, &p);
    let route = |sql: &str| r.route(&parse(sql), &p, false);
    for ok in [
        POINT,
        "SELECT 1",
        "SELECT c FROM sbtest1 WHERE id BETWEEN 1 AND 10 ORDER BY c LIMIT 10",
        "SELECT id + 1, c FROM test.t WHERE id > 5 AND pad LIKE 'a%'",
        "SELECT * FROM t WHERE 1 = 0",
        "SELECT c FROM t WHERE id IN (1, 2, 3)",
        "SELECT coalesce(c, 1) FROM t",
    ] {
        assert_eq!(route(ok), Route::New, "{ok}");
    }
    for (sql, want) in [
        ("SET NAMES utf8mb4", Reason::Kind),
        ("USE test", Reason::Kind),
        ("SELECT 1 UNION SELECT 2", Reason::Kind),
        ("SELECT c FROM t JOIN u ON t.id = u.id", Reason::Ast("join")),
        ("SELECT c FROM t, u", Reason::Ast("join")),
        (
            "SELECT c FROM (SELECT c FROM t) x",
            Reason::Ast("derived table"),
        ),
        (
            "SELECT c FROM t WHERE id IN (SELECT id FROM u)",
            Reason::Ast("subquery"),
        ),
        (
            "SELECT c FROM t WHERE EXISTS (SELECT 1 FROM u)",
            Reason::Ast("subquery"),
        ),
        ("SELECT (SELECT 1) FROM t", Reason::Ast("subquery")),
        ("SELECT count(*) FROM t", Reason::Ast("aggregate")),
        (
            "SELECT sum(k) FROM t WHERE id < 5",
            Reason::Ast("aggregate"),
        ),
        (
            "SELECT c FROM t GROUP BY c",
            Reason::Ast("GROUP BY / HAVING"),
        ),
        ("SELECT DISTINCT c FROM t", Reason::Ast("DISTINCT")),
        ("SELECT c FROM t FOR UPDATE", Reason::Ast("locking clause")),
        (
            "SELECT c FROM t AS OF TIMESTAMP NOW() WHERE id = 1",
            Reason::Ast("AS OF"),
        ),
        ("WITH x AS (SELECT 1) SELECT * FROM x", Reason::Ast("CTE")),
        ("SELECT row_number() OVER () FROM t", Reason::Ast("window")),
        (
            "SELECT no_such_function(c) FROM t",
            Reason::Ast("function not ported"),
        ),
        (
            "SELECT c FROM t WHERE id = ?",
            Reason::Ast("parameter marker"),
        ),
        (
            "SELECT @@max_allowed_packet, @@wait_timeout",
            Reason::Ast("variable"),
        ),
        ("SELECT c FROM t WHERE id = @x", Reason::Ast("variable")),
    ] {
        assert_eq!(route(sql), Route::Poc(want), "{sql}");
    }
    // Prepared statements may have `?`.
    assert_eq!(
        r.route(&parse("SELECT c FROM t WHERE id = ?"), &p, true),
        Route::New
    );
}

// ---------------------------------------------------------------------------------------------
// Step 0: one case per condition.

/// Each condition sends a candidate to the PoC (`session`), and the new stack is not called.
#[test]
fn step0_each_session_condition() {
    type Setup = Box<dyn Fn(&mut Router, &mut FakePoc)>;
    let cases: Vec<(&str, Setup)> = vec![
        (
            "explicit transaction",
            Box::new(|_r: &mut Router, p: &mut FakePoc| p.in_txn = true),
        ),
        (
            "explicit transaction",
            Box::new(|r: &mut Router, _: &mut FakePoc| {
                r.new
                    .session_mut()
                    .vars_mut()
                    .set_status_flag(tidb_base::mysql::ServerStatusInTrans, true)
            }),
        ),
        (
            "autocommit=0",
            Box::new(|r: &mut Router, _: &mut FakePoc| {
                r.new
                    .session_mut()
                    .vars_mut()
                    .set_system_var("autocommit", "0")
                    .unwrap()
            }),
        ),
        (
            "tidb_snapshot",
            Box::new(|r: &mut Router, _: &mut FakePoc| {
                r.new.session_mut().vars_mut().snapshot_ts = 1
            }),
        ),
        (
            "tidb_read_staleness",
            Box::new(|r: &mut Router, _: &mut FakePoc| {
                r.new.session_mut().vars_mut().read_staleness = -5
            }),
        ),
        (
            "tx_read_ts",
            Box::new(|r: &mut Router, _: &mut FakePoc| {
                r.new.session_mut().vars_mut().state.txn_read_ts = Some(TxnReadTS::new(7))
            }),
        ),
        (
            "tidb_txn_mode",
            Box::new(|r: &mut Router, _: &mut FakePoc| {
                r.new.session_mut().vars_mut().txn_mode = "optimistic".into()
            }),
        ),
        (
            "variable unknown to the new session",
            Box::new(|r: &mut Router, p: &mut FakePoc| {
                // The PoC accepts any time zone text; Go rejects this one.
                let res = block_on(r.query(p, "SET time_zone = 'not a zone'"));
                assert!(res.is_ok());
            }),
        ),
        (
            "cursor",
            Box::new(|r: &mut Router, _: &mut FakePoc| r.set_execute_flags(cursor_type::READ_ONLY)),
        ),
    ];
    for (want, setup) in cases {
        let mut p = poc();
        let (mut r, script) = scripted(NewStackMode::Strict, &p);
        script.lock().unwrap().answer = Some(Answer::Rows);
        setup(&mut r, &mut p);
        assert_eq!(
            r.route(&parse(POINT), &p, false),
            Route::Poc(Reason::Session(want)),
            "{want}"
        );
        if want != "cursor" {
            // Even in strict mode the statement runs on the PoC.
            let res = block_on(r.query(&mut p, POINT));
            assert_eq!(rows(&res).map(|r| r.0), Some(3), "{want}");
            assert!(script.lock().unwrap().log.is_empty(), "{want}");
        }
    }
}

/// Step 0: the InfoSchema versions must be equal; lagging and leading both go to the PoC.
#[test]
fn step0_schema_versions_must_be_equal() {
    let p = poc();
    let (r, script) = scripted(NewStackMode::Vs1, &p);
    for (v, want) in [
        (None, Route::New),
        (Some(1), Route::New),
        (Some(0), Route::Poc(Reason::SchemaVersion)),
        (Some(2), Route::Poc(Reason::SchemaVersion)),
    ] {
        script.lock().unwrap().version = v;
        assert_eq!(r.route(&parse(POINT), &p, false), want, "{v:?}");
    }
    let mut p = poc();
    p.schema_version = None;
    script.lock().unwrap().version = Some(1);
    assert_eq!(
        r.route(&parse(POINT), &p, false),
        Route::Poc(Reason::SchemaVersion)
    );
}

/// Step 0 comes before the kind and the AST pre-check.
#[test]
fn step0_comes_first() {
    let mut p = poc();
    p.in_txn = true;
    let r = real(NewStackMode::Vs1, &p);
    assert_eq!(
        r.route(&parse("SELECT count(*) FROM t"), &p, false),
        Route::Poc(Reason::Session("explicit transaction"))
    );
}

// ---------------------------------------------------------------------------------------------
// Step 3: answers, Go-ported errors, fallback (§4.6), strict.

#[test]
fn new_stack_answer_is_returned() {
    let mut p = poc();
    let (mut r, script) = scripted(NewStackMode::Vs1, &p);
    script.lock().unwrap().answer = Some(Answer::Rows);
    let res = block_on(r.query(&mut p, POINT));
    assert_eq!(rows(&res).map(|r| r.0), Some(1));
    assert!(!p.log.iter().any(|l| l.starts_with("run")));
    assert_eq!(p.log, ["start Select"]);
    assert_eq!(r.last_stmt().engine, Engine::New);
}

/// Any error of the new stack other than `Blocked` is Go's error: no fallback.
#[test]
fn go_ported_error_is_not_a_fallback() {
    let mut p = poc();
    let (mut r, script) = scripted(NewStackMode::Vs1, &p);
    script.lock().unwrap().answer = Some(Answer::GoError);
    let e = block_on(r.query(&mut p, POINT)).unwrap_err();
    assert_eq!(e.code, 1054);
    assert!(!p.log.iter().any(|l| l.starts_with("run")));
}

/// §4.6: after `Blocked`, the new session looks as if the statement never ran there. A
/// control router sends the same statement straight to the PoC (unequal schema versions,
/// which change no session state); both end with the same serialized snapshot, and only the
/// PoC's warnings (none) are left.
#[test]
fn fallback_restores_the_new_session() {
    for block in ["D05.3", "D10.2", "D09.12", "D03.1"] {
        let mut p = poc();
        let (mut r, script) = scripted(NewStackMode::Vs1, &p);
        script.lock().unwrap().answer = Some(Answer::Blocked(block));
        let mut pc = poc();
        let (mut control, cscript) = scripted(NewStackMode::Vs1, &pc);
        cscript.lock().unwrap().version = Some(-1);
        for sql in [POINT, "SET NAMES latin1", POINT] {
            let a = block_on(r.query(&mut p, sql));
            let b = block_on(control.query(&mut pc, sql));
            assert_eq!(a, b, "{block}: {sql}");
            assert_eq!(snap(&r), snap(&control), "{block}: {sql}");
        }
        assert_eq!(
            rows(&block_on(r.query(&mut p, POINT))).map(|r| r.0),
            Some(3)
        );
        assert_eq!(r.new.session().vars().stmt_ctx().warning_count(), 0);
        assert_eq!(script.lock().unwrap().log.len(), 3, "{block}");
        assert!(cscript.lock().unwrap().log.is_empty());
    }
}

/// The real phase-0 session: every candidate is `blocked-by:D05.3`; `vs1` falls back with
/// the session unchanged, `strict` returns the 1235 error.
#[test]
fn phase0_session_falls_back_or_reports_1235() {
    let mut p = poc();
    let mut r = real(NewStackMode::Vs1, &p);
    let before = r.new.session().state_snapshot();
    let res = block_on(r.query(&mut p, POINT));
    assert_eq!(rows(&res).map(|r| r.0), Some(3));
    // Only the PoC's previous-statement state was written.
    let mut expected = real(NewStackMode::Vs1, &p);
    LastStmtInfo::from_poc(&LastStmtInfo::default(), &res)
        .apply_to(expected.new.session_mut().vars_mut());
    assert_eq!(snap(&r), snap(&expected));
    assert_ne!(r.new.session().state_snapshot(), before);

    let mut p = poc();
    let mut r = real(NewStackMode::Strict, &p);
    let e = block_on(r.query(&mut p, POINT)).unwrap_err();
    assert_eq!(e.code, 1235);
    assert!(e.message.contains("blocked-by:D05.3"), "{}", e.message);
    assert!(!p.log.iter().any(|l| l.starts_with("run")));
    // Not a candidate: still the PoC in strict mode.
    assert!(block_on(r.query(&mut p, "SET NAMES utf8mb4")).is_ok());
    assert!(block_on(r.query(&mut p, "SELECT count(*) FROM t")).is_ok());
}

// ---------------------------------------------------------------------------------------------
// §4.5: the previous-statement state, every field in both orders.

/// New stack, then PoC: what the new stack's statement left is recorded for the PoC side,
/// field by field (the PoC answers none of these reads itself yet).
#[test]
fn last_stmt_new_then_poc() {
    let mut p = poc();
    let (mut r, script) = scripted(NewStackMode::Vs1, &p);
    r.new
        .session_mut()
        .vars_mut()
        .stmt_ctx_mut()
        .prev_last_insert_id = 7;
    script.lock().unwrap().answer = Some(Answer::Rows);
    block_on(r.query(&mut p, POINT)).unwrap();
    let last = r.last_stmt().clone();
    assert_eq!(last.engine, Engine::New);
    assert_eq!(last.warning_count, 2, "@@warning_count");
    assert_eq!(last.error_count, 1, "@@error_count");
    assert_eq!(last.affected_rows, -1, "ROW_COUNT()");
    assert_eq!(last.found_rows, 5, "FOUND_ROWS()");
    assert_eq!(last.last_insert_id, 7, "LAST_INSERT_ID()");
    assert!(last.plan_from_cache, "@@last_plan_from_cache");
    assert!(last.plan_from_binding, "@@last_plan_from_binding");
    assert!(last.use_chunk_alloc, "@@last_sql_use_alloc");
    assert_eq!(last.query_info.start_ts, 99, "@@tidb_last_query_info");
    assert_eq!(last.txn_info, "{\"start_ts\":99}", "@@tidb_last_txn_info");
    // A PoC statement next: it sees nothing new; a SET keeps FOUND_ROWS and LAST_INSERT_ID.
    block_on(r.query(&mut p, "SET NAMES utf8mb4")).unwrap();
    let last = r.last_stmt().clone();
    assert_eq!(last.engine, Engine::Poc);
    assert_eq!((last.found_rows, last.last_insert_id), (5, 7));
    assert_eq!(last.affected_rows, 0);
    assert!(!last.plan_from_cache && !last.plan_from_binding && !last.use_chunk_alloc);
}

/// PoC, then new stack: the PoC's statement is written into the new session, where Go's
/// system variables read it.
#[test]
fn last_stmt_poc_then_new() {
    let mut p = poc();
    let (mut r, script) = scripted(NewStackMode::Vs1, &p);
    // The new stack ran a statement before (plan from cache, warnings, ...).
    script.lock().unwrap().answer = Some(Answer::Rows);
    block_on(r.query(&mut p, POINT)).unwrap();
    r.finish_new();
    r.last.apply_to(r.new.session_mut().vars_mut());
    assert_eq!(get(&mut r, "last_plan_from_cache"), "ON");
    // Now a PoC statement (a SELECT returning 3 rows).
    script.lock().unwrap().version = Some(-1);
    block_on(r.query(&mut p, POINT)).unwrap();
    assert_eq!(get(&mut r, "warning_count"), "0");
    assert_eq!(get(&mut r, "error_count"), "0");
    assert_eq!(r.new.session().vars().stmt_ctx().prev_affected_rows, -1);
    assert_eq!(r.new.session().vars().last_found_rows, 3);
    assert_eq!(get(&mut r, "last_plan_from_cache"), "OFF");
    assert_eq!(get(&mut r, "last_plan_from_binding"), "OFF");
    assert_eq!(get(&mut r, "last_sql_use_alloc"), "OFF");
    assert!(get(&mut r, "tidb_last_query_info").contains("\"start_ts\":0"));
    // Not changed by a read: LAST_INSERT_ID() and @@tidb_last_txn_info.
    assert_eq!(get(&mut r, "tidb_last_txn_info"), "{\"start_ts\":99}");
    assert_eq!(get(&mut r, "last_insert_id"), "0");
    // A PoC error: one error, counted as a warning too.
    assert!(block_on(r.query(&mut p, "SHOW WARNINGS")).is_err());
    assert_eq!(get(&mut r, "error_count"), "1");
    assert_eq!(get(&mut r, "warning_count"), "1");
    // A SET: ROW_COUNT() 0, FOUND_ROWS() kept.
    block_on(r.query(&mut p, "SET NAMES utf8mb4")).unwrap();
    assert_eq!(r.new.session().vars().stmt_ctx().prev_affected_rows, 0);
    assert_eq!(r.new.session().vars().last_found_rows, 3);
}

// ---------------------------------------------------------------------------------------------
// §4.1: the session-state sync.

#[test]
fn session_state_sync() {
    let mut p = poc();
    let (mut r, _) = scripted(NewStackMode::Vs1, &p);
    block_on(r.query(&mut p, "SET NAMES latin1")).unwrap();
    assert_eq!(get(&mut r, "character_set_client"), "latin1");
    assert_eq!(get(&mut r, "character_set_connection"), "latin1");
    assert_eq!(get(&mut r, "character_set_results"), "latin1");
    assert_eq!(get(&mut r, "collation_connection"), "latin1_bin");
    block_on(r.query(&mut p, "SET time_zone = '+08:00'")).unwrap();
    assert_eq!(get(&mut r, "time_zone"), "+08:00");
    block_on(r.query(&mut p, "SET sql_mode = 'ANSI_QUOTES'")).unwrap();
    assert_eq!(get(&mut r, "sql_mode"), "ANSI_QUOTES");
    block_on(r.query(&mut p, "USE test")).unwrap();
    assert_eq!(r.new.session().current_db(), "test");
    // COM_INIT_DB.
    p.values.current_db = Some("other".into());
    r.use_db(&p);
    assert_eq!(r.new.session().current_db(), "other");
    // A value the new session rejects diverges, until it is set back.
    block_on(r.query(&mut p, "SET time_zone = 'not a zone'")).unwrap();
    assert_eq!(
        r.session_condition(&p),
        Some("variable unknown to the new session")
    );
    block_on(r.query(&mut p, "SET time_zone = 'SYSTEM'")).unwrap();
    assert_eq!(r.session_condition(&p), None);
    // The handshake's charset reached the new session when the router was created.
    let p = FakePoc {
        values: PocValues {
            charset: "utf8".into(),
            collation: "utf8_general_ci".into(),
            ..poc().values
        },
        ..poc()
    };
    let mut r = real(NewStackMode::Vs1, &p);
    assert_eq!(get(&mut r, "collation_connection"), "utf8_general_ci");
}

// ---------------------------------------------------------------------------------------------
// §4.7: prepared statements.

#[test]
fn prepared_both() {
    let mut p = poc();
    let (mut r, script) = scripted(NewStackMode::Vs1, &p);
    script.lock().unwrap().prepare = Some(Answer::Rows);
    script.lock().unwrap().columns = vec![col("c")];
    let info = block_on(r.prepare(&mut p, 1, "SELECT c FROM t WHERE id = ?")).unwrap();
    assert_eq!(info.columns, vec![col("c")]);
    assert!(matches!(r.owner(1), Some(Owner::Both { new_id: 101, .. })));
    // EXECUTE on the new stack.
    script.lock().unwrap().answer = Some(Answer::Rows);
    let res = block_on(r.execute(&mut p, 1, vec![]));
    assert_eq!(rows(&res).map(|r| r.0), Some(1));
    assert!(!p.log.contains(&"execute 1".to_owned()));
    // Blocked this time: this execution only falls back, with matching metadata.
    script.lock().unwrap().answer = Some(Answer::Blocked("D10.2"));
    let res = block_on(r.execute(&mut p, 1, vec![]));
    assert_eq!(rows(&res).map(|r| r.0), Some(3));
    assert!(matches!(r.owner(1), Some(Owner::Both { .. })));
    // A step-0 condition: this execution on the PoC.
    script.lock().unwrap().answer = None;
    r.set_execute_flags(cursor_type::READ_ONLY);
    assert_eq!(
        rows(&block_on(r.execute(&mut p, 1, vec![]))).map(|r| r.0),
        Some(3)
    );
    // strict: Blocked is an error.
    let (mut s, sscript) = scripted(NewStackMode::Strict, &p);
    sscript.lock().unwrap().prepare = Some(Answer::Rows);
    block_on(s.prepare(&mut p, 2, POINT)).unwrap();
    sscript.lock().unwrap().answer = Some(Answer::Blocked("D10.2"));
    let e = block_on(s.execute(&mut p, 2, vec![])).unwrap_err();
    assert_eq!(e.code, 1235);
}

/// Counter-proof of the metadata check: the PoC's column definition differs from the one
/// sent at PREPARE, and the fallback must not send it.
#[test]
fn prepared_fallback_metadata_mismatch() {
    let mut p = poc();
    let (mut r, script) = scripted(NewStackMode::Vs1, &p);
    script.lock().unwrap().prepare = Some(Answer::Rows);
    block_on(r.prepare(&mut p, 1, POINT)).unwrap();
    script.lock().unwrap().answer = Some(Answer::Blocked("D10.2"));
    p.columns = vec![ColumnDef {
        column_length: 11,
        ..col("c")
    }];
    let e = block_on(r.execute(&mut p, 1, vec![])).unwrap_err();
    assert_eq!(e.code, 1105);
    assert!(
        e.message.contains("VS1 column metadata mismatch"),
        "{}",
        e.message
    );
    p.columns = vec![col("c"), col("d")];
    let e = block_on(r.execute(&mut p, 1, vec![])).unwrap_err();
    assert!(
        e.message.contains("VS1 column metadata mismatch"),
        "{}",
        e.message
    );
}

#[test]
fn prepared_poc_only() {
    // New stack Blocked at PREPARE: the PoC owns it for good.
    let mut p = poc();
    let (mut r, script) = scripted(NewStackMode::Vs1, &p);
    script.lock().unwrap().prepare = Some(Answer::Blocked("D13.1"));
    let info = block_on(r.prepare(&mut p, 1, POINT)).unwrap();
    assert_eq!(info.columns, p.columns);
    assert_eq!(
        r.owner(1),
        Some(&Owner::Poc(Reason::Blocked("D13.1".into())))
    );
    script.lock().unwrap().answer = Some(Answer::Rows);
    assert_eq!(
        rows(&block_on(r.execute(&mut p, 1, vec![]))).map(|r| r.0),
        Some(3)
    );
    assert_eq!(script.lock().unwrap().log, ["prepare"]);
    // Not a candidate: the new stack is not asked.
    block_on(r.prepare(&mut p, 2, "SELECT count(*) FROM t")).unwrap();
    assert_eq!(r.owner(2), Some(&Owner::Poc(Reason::Ast("aggregate"))));
    // strict: a statement blocked at PREPARE reports 1235 at EXECUTE.
    let (mut s, sscript) = scripted(NewStackMode::Strict, &p);
    sscript.lock().unwrap().prepare = Some(Answer::Blocked("D13.1"));
    block_on(s.prepare(&mut p, 3, POINT)).unwrap();
    let e = block_on(s.execute(&mut p, 3, vec![])).unwrap_err();
    assert!(e.code == 1235 && e.message.contains("blocked-by:D13.1"));
    // ... but not one that was never a candidate.
    block_on(s.prepare(&mut p, 4, "SELECT count(*) FROM t")).unwrap();
    assert!(block_on(s.execute(&mut p, 4, vec![])).is_ok());
}

/// The PoC cannot prepare it, the new stack can: nothing to fall back to.
#[test]
fn prepared_new_only() {
    let mut p = poc();
    p.prepare_fails = true;
    let (mut r, script) = scripted(NewStackMode::Vs1, &p);
    script.lock().unwrap().prepare = Some(Answer::Rows);
    block_on(r.prepare(&mut p, 1, POINT)).unwrap();
    assert_eq!(r.owner(1), Some(&Owner::New { new_id: 101 }));
    script.lock().unwrap().answer = Some(Answer::Rows);
    assert_eq!(
        rows(&block_on(r.execute(&mut p, 1, vec![]))).map(|r| r.0),
        Some(1)
    );
    // Blocked at EXECUTE: 1235, naming the block; same in vs1 and strict.
    script.lock().unwrap().answer = Some(Answer::Blocked("D10.4"));
    let e = block_on(r.execute(&mut p, 1, vec![])).unwrap_err();
    assert!(
        e.code == 1235 && e.message.contains("blocked-by:D10.4"),
        "{}",
        e.message
    );
    // A step-0 condition: 1235, naming it.
    script.lock().unwrap().answer = Some(Answer::Rows);
    r.set_execute_flags(cursor_type::READ_ONLY);
    let e = block_on(r.execute(&mut p, 1, vec![])).unwrap_err();
    assert!(
        e.code == 1235 && e.message.contains("cursor"),
        "{}",
        e.message
    );
    assert!(!p.log.iter().any(|l| l.starts_with("execute")));
}

#[test]
fn prepared_both_fail() {
    // Blocked + PoC error: the PoC's error.
    let mut p = poc();
    p.prepare_fails = true;
    let (mut r, script) = scripted(NewStackMode::Vs1, &p);
    script.lock().unwrap().prepare = Some(Answer::Blocked("D13.1"));
    let e = block_on(r.prepare(&mut p, 1, POINT)).unwrap_err();
    assert_eq!(e.code, 1235);
    assert!(e.message.contains("on the PoC"), "{}", e.message);
    assert_eq!(r.owner(1), None);
    // A Go-ported error + PoC error: the new stack's error.
    script.lock().unwrap().prepare = Some(Answer::GoError);
    let e = block_on(r.prepare(&mut p, 2, POINT)).unwrap_err();
    assert_eq!(e.code, 1054);
    // A Go-ported error + PoC success: still the new stack's error, and the PoC's statement
    // is closed again.
    p.prepare_fails = false;
    let e = block_on(r.prepare(&mut p, 3, POINT)).unwrap_err();
    assert_eq!(e.code, 1054);
    assert!(p.log.contains(&"close 3".to_owned()));
    assert!(!p.prepared.contains_key(&3));
}

/// COM_STMT_CLOSE goes to the owner, to both when both prepared the statement.
#[test]
fn close_by_owner() {
    let mut p = poc();
    let (mut r, script) = scripted(NewStackMode::Vs1, &p);
    script.lock().unwrap().prepare = Some(Answer::Rows);
    block_on(r.prepare(&mut p, 1, POINT)).unwrap();
    script.lock().unwrap().prepare = Some(Answer::Blocked("D13.1"));
    block_on(r.prepare(&mut p, 2, POINT)).unwrap();
    p.prepare_fails = true;
    script.lock().unwrap().prepare = Some(Answer::Rows);
    block_on(r.prepare(&mut p, 3, POINT)).unwrap();
    for id in [1, 2, 3] {
        r.close(&mut p, id);
        assert_eq!(r.owner(id), None);
    }
    let new_log = script.lock().unwrap().log.clone();
    assert!(new_log.contains(&"close 101".to_owned()));
    assert!(new_log.contains(&"close 102".to_owned()));
    assert!(p.log.contains(&"close 1".to_owned()));
    assert!(p.log.contains(&"close 2".to_owned()));
    assert!(!p.log.contains(&"close 3".to_owned()));
}

/// The real phase-0 session cannot prepare: every candidate is owned by the PoC.
#[test]
fn phase0_prepare_is_poc_owned() {
    let mut p = poc();
    let mut r = real(NewStackMode::Vs1, &p);
    block_on(r.prepare(&mut p, 1, POINT)).unwrap();
    assert_eq!(
        r.owner(1),
        Some(&Owner::Poc(Reason::Blocked("D05.3".into())))
    );
    assert_eq!(
        rows(&block_on(r.execute(&mut p, 1, vec![]))).map(|r| r.0),
        Some(3)
    );
}
