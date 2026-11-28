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
use tidb_mysql_protocol::{RawParams, ResultSet, SqlError, Value};
use tidb_parser::ast;
use tidb_session::{Dialect, Session};
use tidb_sessionctx::stmtctx::new_stmt_ctx;
use tidb_sessionctx::variable::session_state::TxnReadTS;
use tidb_sql::ir::{SetAssignment, SetValue};
use tidb_sql::{Lowered, Statement};

use super::newstack::{ClusterGlobals, NewSession, compiled_defaults_cache};
use super::*;

// ---------------------------------------------------------------------------------------------
// The PoC stand-in.

/// What the stand-in PoC did.
#[derive(Default)]
struct FakePoc {
    values: PocValues,
    /// Assignments the last SET applied.
    set_applied: usize,
    schema_version: Option<i64>,
    in_txn: bool,
    /// Rows a SELECT returns.
    rows: usize,
    /// Column definitions of its results (and of its PREPARE).
    columns: Vec<ColumnDef>,
    prepare_fails: bool,
    prepared: HashMap<u32, Lowered>,
    log: Vec<String>,
    /// The last SELECT lowered & passed into `FakePoc::run`.
    last_run: Option<Lowered>,
    /// The schema sync's registry: when set, the schema version is the real PoC's
    /// computation over it, and `schema_version` is ignored.
    registry: Option<Arc<poc_domain::Registry>>,
    /// The plan-cache decisions handed over at PREPARE (#512), by statement id.
    cacheability: HashMap<u32, crate::plan_cache::PrepareCacheability>,
    /// How often the router cleared the PoC's warnings.
    warnings_cleared: usize,
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
    fn admissible_schema_version(&self) -> Option<i64> {
        match &self.registry {
            Some(r) => crate::router_schema_version(Some(r), std::time::Instant::now()),
            None => self.schema_version,
        }
    }
    fn in_txn(&self) -> bool {
        self.in_txn
    }
    fn values(&self) -> PocValues {
        self.values.clone()
    }
    fn set_applied(&self) -> usize {
        self.set_applied
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
                    self.last_run = Some(lowered.clone());
                    Ok(self.result_set())
                }
                Statement::Set(assignments) => {
                    self.log.push("run set".into());
                    self.set_applied = 0;
                    for a in assignments {
                        // Like the PoC: an assignment it rejects ends the statement, and the
                        // ones before it stay applied (it accepts only the default
                        // sql_select_limit).
                        if let SetAssignment::SysVar { name, value, .. } = a
                            && name == "sql_select_limit"
                            && *value != SetValue::Default
                        {
                            return Err(SqlError::not_supported("setting system variable"));
                        }
                        self.set_applied += 1;
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
                ..Default::default()
            })
        })
    }
    fn execute(
        &mut self,
        stmt_id: u32,
        params: Vec<Param>,
    ) -> BoxFut<'_, Result<Response, SqlError>> {
        Box::pin(async move {
            // As the real `execute_inner`: it starts the statement itself.
            self.log.push("start Select".into());
            self.log.push(format!("execute {stmt_id}"));
            if !params.is_empty() {
                self.log.push(format!("params {params:?}"));
            }
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
    fn clear_warnings(&mut self) {
        self.warnings_cleared += 1;
    }
    fn set_cacheability(&mut self, stmt_id: u32, c: crate::plan_cache::PrepareCacheability) {
        self.cacheability.insert(stmt_id, c);
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
    /// The error distsql's select result makes of a coprocessor response whose
    /// `SelectResponse.error` TiKV set: `ClassTiKV.Synthesize(code, msg)`.
    CopError(i32, &'static str),
    /// A plain Go error (`errors.New`), not a `*terror.Error`.
    PlainError(&'static str),
    /// The 1235 of `tidb_rc_read_check_ts` (sessiontxn, N11 D11): TiDB's "not supported",
    /// not a boundary stub.
    RcReadCheckTsNotSupported,
    /// Writes session state, then hits the boundary stub of this block.
    Blocked(&'static str),
    /// Opens the executors (the executed-statement count moves), then a boundary stub: a
    /// bug of the new stack (plan §4.3).
    BlockedAfterOpen(&'static str),
}

#[derive(Default)]
struct Script {
    answer: Option<Answer>,
    prepare: Option<Answer>,
    version: Option<i64>,
    columns: Vec<ColumnDef>,
    log: Vec<String>,
    next_id: u32,
    /// The new session's executed-statement count as the router sees it.
    executed: u64,
}

struct FakeNew {
    session: Session,
    script: Arc<Mutex<Script>>,
    /// The sysvar cache behind the session's accessor (`None`: always ready).
    globals: Option<Arc<tidb_systable::sysvar_cache::SysVarCache>>,
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
    v.last_query_info.start_ts = 99;
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
            Answer::GoError => Err(tidb_base::dbterror::plannererrors::ErrUnknownColumn
                .gen_with_stack_by_args(vec!["x".into(), "field list".into()])),
            Answer::CopError(code, msg) => {
                Err(tidb_base::dbterror::ClassTiKV.synthesize(code, msg))
            }
            Answer::PlainError(msg) => Err(errors::new(msg)),
            Answer::RcReadCheckTsNotSupported => {
                Err(tidb_sessiontxn::isolation::err_rc_read_check_ts_not_supported())
            }
            Answer::Blocked(block) => {
                write_statement_state(&mut self.session);
                Err(blocked(block, "something"))
            }
            Answer::BlockedAfterOpen(block) => {
                write_statement_state(&mut self.session);
                script.executed += 1;
                Err(blocked(block, "something after the executors opened"))
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
    fn globals_ready(&self) -> bool {
        self.globals.as_ref().is_none_or(|c| !c.needs_rebuild())
    }
    fn executed_stmts(&self) -> u64 {
        self.script.lock().unwrap().executed
    }
    fn execute<'a>(
        &'a mut self,
        _stmt: ast::Stmt,
        _sql: &'a str,
    ) -> BoxFut<'a, Result<Response, Error>> {
        let answer = self.script.lock().unwrap().answer.clone();
        let r = self.run(answer, "execute".into());
        Box::pin(async move { r })
    }
    fn prepare<'a>(&'a mut self, _sql: &'a str) -> BoxFut<'a, Result<NewPrepared, Error>> {
        let answer = self.script.lock().unwrap().prepare.clone();
        let r = self.run(answer, "prepare".into()).map(|_| {
            let mut script = self.script.lock().unwrap();
            script.next_id += 1;
            NewPrepared {
                stmt_id: 100 + script.next_id,
                info: PreparedInfo {
                    param_count: 0,
                    columns: script.columns.clone(),
                    ..Default::default()
                },
            }
        });
        Box::pin(async move { r })
    }
    fn execute_prepared<'a>(
        &'a mut self,
        stmt_id: u32,
        params: &'a [Param],
        _raw: &'a RawParams,
    ) -> BoxFut<'a, Result<Response, Error>> {
        if !params.is_empty() {
            self.script
                .lock()
                .unwrap()
                .log
                .push(format!("params {params:?}"));
        }
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
        session: Session::new(
            Dialect::MySQL,
            Box::new(ClusterGlobals::compiled_defaults()),
        ),
        script: script.clone(),
        globals: None,
    };
    let r = Router::new(mode, Box::new(new), &poc.values());
    (r, script)
}

/// An InfoSchema with `test.sbtest1 (id BIGINT PRIMARY KEY, c CHAR(120))`, built by a full
/// load as the domain builds it.
fn sbtest_info_schema() -> Arc<tidb_infoschema::InfoSchemaV2> {
    use tidb_model::{ColumnInfo, DBInfo, StatePublic, TableInfo};
    use tidb_parser::model::CIStr;
    use tidb_parser::types::FieldType;
    let col = |id: i64, name: &str, tp: u8| {
        Arc::new(ColumnInfo {
            id,
            name: CIStr::new(name),
            offset: (id - 1) as isize,
            field_type: FieldType::new(tp),
            state: StatePublic,
            ..Default::default()
        })
    };
    let t = TableInfo {
        id: 110,
        // The database's id, as the InfoSchema loader sets it.
        dbid: 2,
        name: CIStr::new("sbtest1"),
        columns: Some(vec![
            col(1, "id", tidb_base::mysql::TypeLonglong),
            col(2, "c", tidb_base::mysql::TypeString),
        ]),
        pk_is_handle: true,
        state: StatePublic,
        version: 5,
        max_column_id: 2,
        ..Default::default()
    };
    let mut db = DBInfo {
        id: 2,
        name: CIStr::new("test"),
        state: StatePublic,
        ..Default::default()
    };
    db.deprecated.tables = Some(vec![Arc::new(t)]);
    let mut b = tidb_infoschema::new_builder(None, None, tidb_infoschema::new_data());
    block_on(b.init_with_db_infos(vec![db], vec![], vec![], 1)).unwrap();
    Arc::new(block_on(b.build(1)))
}

/// The real new session, with an InfoSchema that has `test.sbtest1`.
fn real(mode: NewStackMode, poc: &FakePoc) -> Router {
    // `tidb_enable_metadata_lock` on, as on a cluster (Go's default).
    tidb_session::session::init_mdl_variable();
    let mut new = NewSession::new(None, None, None, compiled_defaults_cache());
    new.session_mut()
        .set_domain_info_schema(sbtest_info_schema());
    Router::new(mode, Box::new(new), &poc.values())
}

fn parse(sql: &str) -> ast::Stmt {
    tidb_sql::ir::parse_one(sql).unwrap()
}

fn parse_with(sql: &str, r: &Router) -> ast::Stmt {
    tidb_sql::ir::parse_one_with(sql, &r.parse_options()).unwrap()
}

/// Decodes one MySQL text-protocol result row from [`EncodedResultSet`] into column byte slices.
fn decode_text_row(mut packet: &[u8], ncols: usize) -> Vec<Option<Vec<u8>>> {
    let mut out = Vec::with_capacity(ncols);
    for _ in 0..ncols {
        let (&first, rest) = packet.split_first().expect("truncated text row");
        if first == 0xfb {
            out.push(None);
            packet = rest;
            continue;
        }
        let (len, rest) = match first {
            0..=250 => (usize::from(first), rest),
            0xfc => {
                let (b, r) = rest.split_at(2);
                (usize::from(u16::from_le_bytes([b[0], b[1]])), r)
            }
            other => panic!("unexpected length-encoded prefix {other:#x}"),
        };
        let (val, next) = rest.split_at(len);
        out.push(Some(val.to_vec()));
        packet = next;
    }
    assert!(packet.is_empty(), "trailing bytes in row: {packet:?}");
    out
}

fn encoded_cell(res: &Result<Response, SqlError>) -> Vec<u8> {
    let Ok(Response::Encoded(enc)) = res else {
        panic!("expected encoded response, got {res:?}");
    };
    assert_eq!(enc.rows.len(), 1, "expected 1 row, got {:?}", enc.rows);
    decode_text_row(&enc.rows[0], enc.columns.len())
        .into_iter()
        .next()
        .flatten()
        .expect("non-NULL cell")
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
    assert_eq!("fallback".parse(), Ok(NewStackMode::Fallback));
    assert_eq!("strict".parse(), Ok(NewStackMode::Strict));
    assert!("on".parse::<NewStackMode>().is_err());
    assert_eq!(NewStackMode::default(), NewStackMode::Off);
}

#[test]
fn kind_and_ast_precheck() {
    let p = poc();
    let r = real(NewStackMode::Fallback, &p);
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
    let (r, script) = scripted(NewStackMode::Fallback, &p);
    for (v, want) in [
        (None, Route::New),
        (Some(1), Route::New),
        (Some(0), Route::Poc(Reason::SchemaVersion)),
        (Some(2), Route::Poc(Reason::SchemaVersion)),
    ] {
        script.lock().unwrap().version = v;
        assert_eq!(r.route(&parse(POINT), &p, false), want, "{v:?}");
    }
    // No admissible schema lease: the PoC, with its own reason.
    let mut p = poc();
    p.schema_version = None;
    script.lock().unwrap().version = Some(1);
    assert_eq!(
        r.route(&parse(POINT), &p, false),
        Route::Poc(Reason::SchemaLease)
    );
}

/// Step 0: a statement goes to the new stack only while the synced schema is admissible.
/// Here the node is not registered in the cluster (never registered, or deregistered): the
/// DDL owner does not wait for it, so the new stack's InfoSchema may be two versions behind
/// the data. The ADR 0003 cache still holds the new stack's version, and the versions
/// compare equal; the statement must still run on the PoC, whose path checks the schema at
/// the statement's own timestamp. (The lease and the ported domain's reload are covered by
/// the registry's `new_stack_schema_version_needs_an_admissible_lease`.)
#[test]
fn step0_unadmissible_schema_lease_goes_to_the_poc() {
    let mut p = poc();
    p.registry = Some(poc_domain::Registry::new(poc_domain::Limits::default()));
    let (mut r, script) = scripted(NewStackMode::Strict, &p);
    script.lock().unwrap().version = Some(1);
    assert_eq!(
        r.route(&parse(POINT), &p, false),
        Route::Poc(Reason::SchemaLease)
    );
    // Even in strict mode the statement runs on the PoC.
    let res = block_on(r.query(&mut p, POINT));
    assert_eq!(rows(&res).map(|r| r.0), Some(3));
    assert!(script.lock().unwrap().log.is_empty());
}

/// Step 0 comes before the kind and the AST pre-check.
#[test]
fn step0_comes_first() {
    let mut p = poc();
    p.in_txn = true;
    let r = real(NewStackMode::Fallback, &p);
    assert_eq!(
        r.route(&parse("SELECT count(*) FROM t"), &p, false),
        Route::Poc(Reason::Session("explicit transaction"))
    );
}

/// #509 Step 2: every `sql_mode` bit Go's parser reads (alone or in combination `ANSI`) is now
/// faithfully implemented in `tidb-parser`, so none of them triggers a step-0 PoC hold.
#[test]
fn step0_sql_mode_bits_route_with_session_sql_mode() {
    let mut p = poc();
    let (mut r, _) = scripted(NewStackMode::Fallback, &p);
    for mode in [
        "ANSI_QUOTES",
        "PIPES_AS_CONCAT",
        "HIGH_NOT_PRECEDENCE",
        "NO_BACKSLASH_ESCAPES",
        "IGNORE_SPACE",
        "REAL_AS_FLOAT",
        "STRICT_TRANS_TABLES,IGNORE_SPACE",
        "ANSI",
        "",
        "TRADITIONAL",
        "ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES,NO_ZERO_IN_DATE,NO_ZERO_DATE,\
         ERROR_FOR_DIVISION_BY_ZERO,NO_AUTO_CREATE_USER,NO_ENGINE_SUBSTITUTION",
        "NO_UNSIGNED_SUBTRACTION,ALLOW_INVALID_DATES",
    ] {
        block_on(r.query(&mut p, &format!("SET sql_mode = '{mode}'"))).unwrap();
        assert_eq!(r.session_condition(&p), None, "{mode}");
        assert_eq!(
            r.route(&parse_with(POINT, &r), &p, false),
            Route::New,
            "{mode}"
        );
    }
}

/// The three W48 statements of #509 (Go's integration test `types/const`, lines 13, 141 and
/// 142) now parse with the active session `sql_mode`:
/// 1. `SELECT 'hello' || 'world'` under `PIPES_AS_CONCAT` parses as `CONCAT(...)`, so precheck
///    routes it to `Poc(Reason::Ast("function not ported"))` and it returns 1235 rather than
///    wrong `0`.
/// 2. `SELECT * FROM sbtest1 WHERE NOT id BETWEEN 2 AND 3` under `HIGH_NOT_PRECEDENCE` parses
///    as `Between(Not2, ...)` and routes `Route::New` rather than falling behind a guard.
/// 3. `SELECT NOT 1 BETWEEN -5 AND 5` executes on the real new stack and flips from `0`
///    (default mode) to `1` (`HIGH_NOT_PRECEDENCE`) and back to `0` (`SET @@sql_mode = DEFAULT`).
#[test]
fn w48_sql_mode_cases_match_go() {
    // Case 1: PIPES_AS_CONCAT
    {
        let mut p = poc();
        let mut r = real(NewStackMode::Strict, &p);
        assert_eq!(
            encoded_cell(&block_on(r.query(&mut p, "SELECT 'hello' || 'world'"))),
            b"0"
        );
        block_on(r.query(&mut p, "SET sql_mode = 'PIPES_AS_CONCAT'")).unwrap();
        let stmt = parse_with("SELECT 'hello' || 'world'", &r);
        assert_eq!(
            r.route(&stmt, &p, false),
            Route::Poc(Reason::Ast("function not ported"))
        );
        let e = block_on(r.query(&mut p, "SELECT 'hello' || 'world'")).unwrap_err();
        assert_eq!(e.code, 1235, "{}", e.message);
        block_on(r.query(&mut p, "SET @@sql_mode = DEFAULT")).unwrap();
        assert_eq!(
            encoded_cell(&block_on(r.query(&mut p, "SELECT 'hello' || 'world'"))),
            b"0"
        );
    }

    // Also verify scripted strict mode does not touch the new stack under PIPES_AS_CONCAT.
    {
        let mut p = poc();
        let (mut r, script) = scripted(NewStackMode::Strict, &p);
        block_on(r.query(&mut p, "SET sql_mode = 'PIPES_AS_CONCAT'")).unwrap();
        let e = block_on(r.query(&mut p, "SELECT 'hello' || 'world'")).unwrap_err();
        assert_eq!(e.code, 1235, "{}", e.message);
        assert!(script.lock().unwrap().log.is_empty());
    }

    // Case 2: WHERE NOT id BETWEEN 2 AND 3
    {
        let mut p = poc();
        let (mut r, _) = scripted(NewStackMode::Strict, &p);
        let sql = "SELECT * FROM sbtest1 WHERE NOT id BETWEEN 2 AND 3";
        let ast::Stmt::Select(def_sel) = parse_with(sql, &r) else {
            panic!("expected SELECT");
        };
        assert!(matches!(
            def_sel.where_.as_ref(),
            Some(ast::Expr::Unary(u))
                if u.op == ast::Op::Not && matches!(&u.v, ast::Expr::Between(_))
        ));
        assert_eq!(r.route(&ast::Stmt::Select(def_sel), &p, false), Route::New);

        block_on(r.query(&mut p, "SET sql_mode = 'high_not_precedence'")).unwrap();
        let high_stmt = parse_with(sql, &r);
        let ast::Stmt::Select(high_sel) = &high_stmt else {
            panic!("expected SELECT");
        };
        assert!(matches!(
            high_sel.where_.as_ref(),
            Some(ast::Expr::Between(b))
                if !b.not
                    && matches!(&b.expr, ast::Expr::Unary(u) if u.op == ast::Op::Not2)
        ));
        assert_eq!(r.route(&high_stmt, &p, false), Route::New);
    }

    // Case 3: SELECT NOT 1 BETWEEN -5 AND 5 executed end-to-end on real NewSession.
    {
        let mut p = poc();
        let mut r = real(NewStackMode::Strict, &p);
        assert_eq!(
            encoded_cell(&block_on(r.query(&mut p, "SELECT NOT 1 BETWEEN -5 AND 5"))),
            b"0"
        );
        block_on(r.query(&mut p, "SET sql_mode = 'high_not_precedence'")).unwrap();
        assert_eq!(
            encoded_cell(&block_on(r.query(&mut p, "SELECT NOT 1 BETWEEN -5 AND 5"))),
            b"1"
        );
        block_on(r.query(&mut p, "SET @@sql_mode = DEFAULT")).unwrap();
        assert_eq!(
            encoded_cell(&block_on(r.query(&mut p, "SELECT NOT 1 BETWEEN -5 AND 5"))),
            b"0"
        );
    }
}

/// #509 Step 2: real execution and fallback re-parse respect `ANSI_QUOTES`,
/// `NO_BACKSLASH_ESCAPES`, `IGNORE_SPACE`, `REAL_AS_FLOAT`, and `ANSI`.
#[test]
fn all_sql_mode_bits_execute_and_fallback_reparse_with_session_mode() {
    // ANSI_QUOTES: real NewSession execution (literal -> 1054 unknown column) + fallback re-parse.
    {
        let mut p = poc();
        let mut r = real(NewStackMode::Strict, &p);
        assert_eq!(
            encoded_cell(&block_on(r.query(&mut p, r#"SELECT "x""#))),
            b"x"
        );
        block_on(r.query(&mut p, "SET sql_mode = 'ANSI_QUOTES'")).unwrap();
        let e = block_on(r.query(&mut p, r#"SELECT "x""#)).unwrap_err();
        assert_eq!(
            (e.code, e.message.as_str()),
            (1054, "Unknown column 'x' in 'field list'")
        );

        // Fallback re-parse in Router::query_inner must pass opts (ANSI_QUOTES), so FakePoc
        // sees `Expr::Column("c")`, NOT `Expr::Literal(Str("c"))`.
        let mut p = poc();
        p.values.current_db = Some("test".into());
        let mut r = real(NewStackMode::Fallback, &p);
        block_on(r.query(&mut p, "SET sql_mode = 'ANSI_QUOTES'")).unwrap();
        let res = block_on(r.query(&mut p, r#"SELECT "c" FROM sbtest1 WHERE "id" = 1"#));
        assert_eq!(rows(&res).map(|r| r.0), Some(3));
        let lowered = p.last_run.take().expect("fallback ran on PoC");
        let Statement::Select(sel) = lowered.stmt else {
            panic!("expected Select");
        };
        assert!(matches!(
            &sel.fields[0],
            tidb_sql::ir::Field::Expr {
                expr: tidb_sql::ir::Expr::Column(c),
                ..
            } if c.name == "c"
        ));
    }

    // NO_BACKSLASH_ESCAPES: `'\n'` evaluates to two bytes (`\n`) vs one newline byte.
    {
        let mut p = poc();
        let mut r = real(NewStackMode::Strict, &p);
        assert_eq!(
            encoded_cell(&block_on(r.query(&mut p, r"SELECT '\n'"))),
            b"\n"
        );
        block_on(r.query(&mut p, "SET sql_mode = 'NO_BACKSLASH_ESCAPES'")).unwrap();
        assert_eq!(
            encoded_cell(&block_on(r.query(&mut p, r"SELECT '\n'"))),
            br"\n"
        );
        let e = block_on(r.query(&mut p, r"SELECT '\'hello'")).unwrap_err();
        assert_eq!(e.code, 1064, "{}", e.message);
    }

    // IGNORE_SPACE: `CAST (1 AS CHAR)` fails without the mode, succeeds with it;
    // `COUNT (id)` switches from `function not ported` to `aggregate`.
    {
        let mut p = poc();
        let mut r = real(NewStackMode::Strict, &p);
        let e = block_on(r.query(&mut p, "SELECT CAST (1 AS CHAR)")).unwrap_err();
        assert_eq!(e.code, 1064, "{}", e.message);
        assert_eq!(
            r.route(&parse_with("SELECT COUNT (id) FROM sbtest1", &r), &p, false),
            Route::Poc(Reason::Ast("function not ported"))
        );

        block_on(r.query(&mut p, "SET sql_mode = 'IGNORE_SPACE'")).unwrap();
        assert_eq!(
            encoded_cell(&block_on(r.query(&mut p, "SELECT CAST (1 AS CHAR)"))),
            b"1"
        );
        assert_eq!(
            r.route(&parse_with("SELECT COUNT (id) FROM sbtest1", &r), &p, false),
            Route::Poc(Reason::Ast("aggregate"))
        );
    }

    // REAL_AS_FLOAT: `CAST(1.25 AS REAL)` reports Float instead of Double.
    {
        let mut p = poc();
        let mut r = real(NewStackMode::Strict, &p);
        let Ok(Response::Encoded(def_rs)) = block_on(r.query(&mut p, "SELECT CAST(1.25 AS REAL)"))
        else {
            panic!("expected Encoded response");
        };
        assert_eq!(def_rs.columns[0].column_type, ColumnType::Double);

        block_on(r.query(&mut p, "SET sql_mode = 'REAL_AS_FLOAT'")).unwrap();
        let Ok(Response::Encoded(flt_rs)) = block_on(r.query(&mut p, "SELECT CAST(1.25 AS REAL)"))
        else {
            panic!("expected Encoded response");
        };
        assert_eq!(flt_rs.columns[0].column_type, ColumnType::Float);
    }

    // ANSI combination mode: REAL_AS_FLOAT + PIPES_AS_CONCAT + ANSI_QUOTES + IGNORE_SPACE.
    {
        let mut p = poc();
        let mut r = real(NewStackMode::Strict, &p);
        block_on(r.query(&mut p, "SET sql_mode = 'ANSI'")).unwrap();
        let Ok(Response::Encoded(rs)) = block_on(r.query(&mut p, "SELECT CAST (1.25 AS REAL)"))
        else {
            panic!("expected Encoded response");
        };
        assert_eq!(rs.columns[0].column_type, ColumnType::Float);
        assert_eq!(
            block_on(r.query(&mut p, r#"SELECT "x""#)).unwrap_err().code,
            1054
        );
        assert_eq!(
            block_on(r.query(&mut p, "SELECT 'a' || 'b'"))
                .unwrap_err()
                .code,
            1235
        );
    }
}

/// #509 Step 2 for prepared statements: both sides parse with the `sql_mode` active at
/// PREPARE (matching Go), and changing `sql_mode` before EXECUTE keeps the prepared AST
/// and column metadata from PREPARE time.
#[test]
fn prepared_under_sql_mode_uses_prepare_time_mode() {
    let mut p = poc();
    p.values.current_db = Some("test".into());
    let mut r = real(NewStackMode::Fallback, &p);

    // 1. ANSI_QUOTES: PREPARE succeeds on both sides with quoted identifiers, and EXECUTE
    //    remains usable even after SET sql_mode = DEFAULT.
    block_on(r.query(&mut p, "SET sql_mode = 'ANSI_QUOTES'")).unwrap();
    let info = block_on(r.prepare(&mut p, 1, r#"SELECT "c" FROM sbtest1 WHERE "id" = ?"#)).unwrap();
    assert!(matches!(r.owner(1), Some(Owner::Both { .. })));
    assert_eq!(info.columns.len(), 1);
    assert_eq!(info.columns[0].name, "c");
    p.columns = info.columns.clone();

    block_on(r.query(&mut p, "SET sql_mode = DEFAULT")).unwrap();
    r.set_execute_raw_params(RawParams {
        null_bitmap: vec![0],
        param_types: vec![tidb_base::mysql::TypeLonglong, 0],
        param_values: 1i64.to_le_bytes().to_vec(),
        bound: vec![None],
    });
    let res = block_on(r.execute(
        &mut p,
        1,
        vec![Param {
            ty: ColumnType::LongLong,
            unsigned: false,
            value: Value::Int(1),
        }],
    ));
    assert_eq!(rows(&res).map(|r| r.0), Some(3));

    // 2. REAL_AS_FLOAT: PREPARE column metadata is TypeFloat vs TypeDouble under DEFAULT,
    //    and executing after SET sql_mode = DEFAULT preserves TypeFloat. (PoC IR does not lower
    //    CAST, so the new stack owns stmt 2/3 alone: Owner::New.)
    block_on(r.query(&mut p, "SET sql_mode = 'REAL_AS_FLOAT'")).unwrap();
    let real_info = block_on(r.prepare(&mut p, 2, "SELECT CAST(? AS REAL)")).unwrap();
    assert_eq!(real_info.columns[0].column_type, ColumnType::Float);
    assert!(matches!(r.owner(2), Some(Owner::New { .. })));

    block_on(r.query(&mut p, "SET sql_mode = DEFAULT")).unwrap();
    r.set_execute_raw_params(RawParams {
        null_bitmap: vec![0],
        param_types: vec![tidb_base::mysql::TypeDouble, 0],
        param_values: 1.25f64.to_le_bytes().to_vec(),
        bound: vec![None],
    });
    let Ok(Response::Encoded(exec_rs)) = block_on(r.execute(
        &mut p,
        2,
        vec![Param {
            ty: ColumnType::Double,
            unsigned: false,
            value: Value::Float(1.25),
        }],
    )) else {
        panic!("expected new-stack encoded execute result");
    };
    assert_eq!(exec_rs.columns[0].column_type, ColumnType::Float);

    let def_info = block_on(r.prepare(&mut p, 3, "SELECT CAST(? AS REAL)")).unwrap();
    assert_eq!(def_info.columns[0].column_type, ColumnType::Double);

    // 3. PIPES_AS_CONCAT: `SELECT 'hello' || 'world'` parses as CONCAT -> PoC-owned at PREPARE.
    block_on(r.query(&mut p, "SET sql_mode = 'PIPES_AS_CONCAT'")).unwrap();
    let e = block_on(r.prepare(&mut p, 4, "SELECT 'hello' || 'world'")).unwrap_err();
    assert_eq!(e.code, 1235, "{}", e.message);
}

/// #509 / #555: `PREPARE -> EXECUTE -> SET sql_mode -> EXECUTE -> CLOSE -> re-PREPARE -> EXECUTE`
/// with the instance plan cache enabled, in both `fallback` and `strict` modes.
/// Verifies that executing a statement prepared under one `sql_mode` after `SET sql_mode = ''`
/// keeps the prepare-time AST (matching Go) without poisoning the instance plan cache entry for
/// a later `PREPARE` of the same SQL text under the new `sql_mode`.
#[test]
fn prepare_set_sql_mode_execute_reprepare_with_plan_cache() {
    let extract_str = |res: Result<Response, SqlError>| -> Vec<u8> {
        match res.expect("execute should succeed") {
            Response::Encoded(rs) => {
                // Binary protocol single VARCHAR column: 0x00 header, 1-byte null bitmap, lenenc string.
                let row = &rs.rows[0];
                let len = row[2] as usize;
                row[3..3 + len].to_vec()
            }
            Response::ResultSet(rs) => match &rs.rows[0][0] {
                Value::Bytes(b) => b.clone(),
                other => panic!("unexpected cell: {other:?}"),
            },
            other => panic!("unexpected response: {other:?}"),
        }
    };

    for mode in [NewStackMode::Fallback, NewStackMode::Strict] {
        let mut p = poc();
        p.values.current_db = Some("test".into());
        let mut r = real(mode, &p);
        let cache = Arc::new(
            tidb_planner::core::plan_cache_instance::new_instance_plan_cache(90 << 20, 100 << 20),
        );
        r.new
            .session_mut()
            .set_instance_plan_cache(Some(cache as _));
        r.new.session_mut().vars_mut().enable_prepared_plan_cache = true;

        // 1. NO_BACKSLASH_ESCAPES: `SELECT '\n'` returns 2 bytes (`\n` literal) when prepared
        //    under NO_BACKSLASH_ESCAPES, keeps returning 2 bytes when executed after
        //    `SET sql_mode = ''`, and returns 1 byte (newline) after close + re-prepare under `''`.
        block_on(r.query(&mut p, "SET sql_mode = 'NO_BACKSLASH_ESCAPES'")).unwrap();
        let sql = r"SELECT '\n'";
        let info1 = block_on(r.prepare(&mut p, 1, sql)).unwrap();
        p.columns = info1.columns.clone();
        assert_eq!(extract_str(block_on(r.execute(&mut p, 1, vec![]))), b"\\n");

        block_on(r.query(&mut p, "SET sql_mode = ''")).unwrap();
        assert_eq!(extract_str(block_on(r.execute(&mut p, 1, vec![]))), b"\\n");

        r.close(&mut p, 1);
        let info2 = block_on(r.prepare(&mut p, 2, sql)).unwrap();
        p.columns = info2.columns.clone();
        assert_eq!(
            extract_str(block_on(r.execute(&mut p, 2, vec![]))),
            b"\n",
            "mode={mode:?}: re-prepared statement must not hit stale NO_BACKSLASH_ESCAPES plan"
        );

        // 2. ANSI_QUOTES: PREPARE under ANSI_QUOTES records ModeANSIQuotes in PlanCacheStmt,
        //    whereas re-PREPARE under '' records 0, so their PlanCacheKeys do not collide even
        //    when the first statement is executed after `SET sql_mode = ''`.
        block_on(r.query(&mut p, "SET sql_mode = 'ANSI_QUOTES'")).unwrap();
        let sql_aq = r#"SELECT id FROM sbtest1 WHERE "c" = 'x'"#;
        let _ = block_on(r.prepare(&mut p, 3, sql_aq)).unwrap();
        let Owner::Both { new_id: nid3, .. } = r.owner(3).cloned().unwrap() else {
            panic!("expected Owner::Both for stmt 3");
        };
        {
            let ps_any = r
                .new
                .session()
                .vars()
                .get_prepared_stmt_by_id(nid3)
                .unwrap();
            let ps = ps_any
                .downcast_ref::<parking_lot::Mutex<tidb_planner::core::plan_cache_utils::PlanCacheStmt>>()
                .unwrap()
                .lock();
            assert_eq!(ps.parse_sql_mode, tidb_base::mysql::ModeANSIQuotes.0);
        }

        block_on(r.query(&mut p, "SET sql_mode = ''")).unwrap();
        r.close(&mut p, 3);
        let _ = block_on(r.prepare(&mut p, 4, sql_aq)).unwrap();
        let Owner::Both { new_id: nid4, .. } = r.owner(4).cloned().unwrap() else {
            panic!("expected Owner::Both for stmt 4");
        };
        {
            let ps_any = r
                .new
                .session()
                .vars()
                .get_prepared_stmt_by_id(nid4)
                .unwrap();
            let ps = ps_any
                .downcast_ref::<parking_lot::Mutex<tidb_planner::core::plan_cache_utils::PlanCacheStmt>>()
                .unwrap()
                .lock();
            assert_eq!(ps.parse_sql_mode, 0);
        }

        // 3. REAL_AS_FLOAT: PREPARE under REAL_AS_FLOAT -> EXECUTE -> SET '' -> EXECUTE ->
        //    CLOSE -> re-PREPARE -> EXECUTE returns Float then Double (no stale plan cache hit).
        block_on(r.query(&mut p, "SET sql_mode = 'REAL_AS_FLOAT'")).unwrap();
        let sql_real = "SELECT CAST(1.25 AS REAL)";
        let _ = block_on(r.prepare(&mut p, 5, sql_real)).unwrap();
        let Ok(Response::Encoded(rs1)) = block_on(r.execute(&mut p, 5, vec![])) else {
            panic!("expected encoded result");
        };
        assert_eq!(rs1.columns[0].column_type, ColumnType::Float);

        block_on(r.query(&mut p, "SET sql_mode = ''")).unwrap();
        let Ok(Response::Encoded(rs2)) = block_on(r.execute(&mut p, 5, vec![])) else {
            panic!("expected encoded result");
        };
        assert_eq!(rs2.columns[0].column_type, ColumnType::Float);

        r.close(&mut p, 5);
        let _ = block_on(r.prepare(&mut p, 6, sql_real)).unwrap();
        let Ok(Response::Encoded(rs3)) = block_on(r.execute(&mut p, 6, vec![])) else {
            panic!("expected encoded result");
        };
        assert_eq!(
            rs3.columns[0].column_type,
            ColumnType::Double,
            "mode={mode:?}: re-prepared CAST(1.25 AS REAL) under '' must be Double, not stale Float"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Step 3: answers, Go-ported errors, fallback (§4.6), strict.

#[test]
fn new_stack_answer_is_returned() {
    let mut p = poc();
    let (mut r, script) = scripted(NewStackMode::Fallback, &p);
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
    let (mut r, script) = scripted(NewStackMode::Fallback, &p);
    script.lock().unwrap().answer = Some(Answer::GoError);
    let e = block_on(r.query(&mut p, POINT)).unwrap_err();
    assert_eq!(
        e,
        SqlError {
            code: 1054,
            state: "42S22",
            message: "Unknown column 'x' in 'field list'".into(),
            source_err: None,
        }
    );
    assert!(!p.log.iter().any(|l| l.starts_with("run")));
}

/// A coprocessor error TiKV returned (a pushed-down `id + 9223372036854775807 > 0`
/// overflowing) reaches the client as Go's `clientConn.writeError` sends it: TiKV's code
/// and message, and the SQLSTATE Go's `MySQLState` table gives the code (`22003` for
/// `ErrDataOutOfRange`, not `HY000`). A code `ClassTiKV` did not register becomes
/// `ErrUnknown` (`terror.ToSQLError`), as does a plain Go error.
#[test]
fn new_stack_errors_get_go_code_sqlstate_and_message() {
    const OVERFLOW: &str = "BIGINT value is out of range in '(1 + 9223372036854775807)'";
    let cases = [
        (
            Answer::CopError(1690, OVERFLOW),
            SqlError {
                code: 1690,
                state: "22003",
                message: OVERFLOW.into(),
                source_err: None,
            },
        ),
        (
            Answer::CopError(1365, "Division by 0"),
            SqlError {
                code: 1365,
                state: "22012",
                message: "Division by 0".into(),
                source_err: None,
            },
        ),
        (
            Answer::CopError(1406, "Data Too Long, field len 3"),
            SqlError {
                code: 1105,
                state: "HY000",
                message: "Data Too Long, field len 3".into(),
                source_err: None,
            },
        ),
        (
            Answer::PlainError("other error: region unavailable"),
            SqlError {
                code: 1105,
                state: "HY000",
                message: "other error: region unavailable".into(),
                source_err: None,
            },
        ),
    ];
    for mode in [NewStackMode::Fallback, NewStackMode::Strict] {
        for (answer, want) in &cases {
            let mut p = poc();
            let (mut r, script) = scripted(mode, &p);
            script.lock().unwrap().answer = Some(answer.clone());
            let e = block_on(r.query(&mut p, POINT)).unwrap_err();
            assert_eq!(&e, want, "{mode:?} {answer:?}");
            assert_eq!(
                e.to_string(),
                format!("ERROR {} ({}): {}", want.code, want.state, want.message)
            );
            assert!(
                !p.log.iter().any(|l| l.starts_with("run")),
                "{mode:?}: no fallback"
            );
            assert_eq!(r.last_stmt().engine, Engine::New);
        }
    }
}

/// `tidb_rc_read_check_ts` is rejected by the new stack with a real 1235 (N11 D11,
/// provisional, #146): returned to the client, never a fallback (the PoC would read at the
/// reused ts without the check).
#[test]
fn rc_read_check_ts_is_not_a_fallback() {
    let mut p = poc();
    let (mut r, script) = scripted(NewStackMode::Fallback, &p);
    script.lock().unwrap().answer = Some(Answer::RcReadCheckTsNotSupported);
    let e = block_on(r.query(&mut p, POINT)).unwrap_err();
    assert_eq!(e.code, 1235);
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
        let (mut r, script) = scripted(NewStackMode::Fallback, &p);
        script.lock().unwrap().answer = Some(Answer::Blocked(block));
        let mut pc = poc();
        let (mut control, cscript) = scripted(NewStackMode::Fallback, &pc);
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

/// A SET that fails at its second assignment keeps the first one, in the PoC and in the new
/// session (Go's `SetExecutor.Next` returns at the first error without undoing the others).
#[test]
fn failing_set_keeps_the_earlier_assignments_in_the_new_session() {
    let mut p = poc();
    let mut r = real(NewStackMode::Fallback, &p);
    assert_eq!(get(&mut r, "character_set_client"), "utf8mb4");
    let res = block_on(r.query(
        &mut p,
        "SET NAMES latin1, sql_select_limit = 5, NAMES ascii",
    ));
    assert_eq!(res.unwrap_err().code, 1235);
    assert_eq!(p.set_applied, 1);
    assert_eq!(p.values.charset, "latin1");
    for v in [
        "character_set_client",
        "character_set_connection",
        "character_set_results",
    ] {
        assert_eq!(get(&mut r, v), "latin1", "{v}");
    }
    assert_eq!(get(&mut r, "collation_connection"), "latin1_bin");
    // A SET that fails at its first assignment changes nothing.
    let res = block_on(r.query(&mut p, "SET sql_select_limit = 5, NAMES ascii"));
    assert_eq!(res.unwrap_err().code, 1235);
    assert_eq!(get(&mut r, "character_set_client"), "latin1");
    // A variable only the new session models keeps its value too.
    let res = block_on(r.query(&mut p, "SET time_zone = '+01:00', sql_select_limit = 5"));
    assert_eq!(res.unwrap_err().code, 1235);
    assert_eq!(get(&mut r, "time_zone"), "+01:00");
}

/// The route trace (#260): every routing decision of the connection, with its engine and
/// reason; a statement that never reaches routing leaves it unchanged.
#[test]
fn route_trace_records_each_statement() {
    super::trace::enable();
    let conn = 7_000_001;
    let mut p = poc();
    let (mut r, script) = scripted(NewStackMode::Fallback, &p);
    r.set_connection_id(conn);
    script.lock().unwrap().answer = Some(Answer::Rows);
    block_on(r.query(&mut p, POINT)).unwrap();
    assert_eq!(
        super::trace::get(conn),
        super::trace::Last {
            seq: 1,
            engine: "new",
            reason: "ok".into()
        }
    );
    script.lock().unwrap().answer = Some(Answer::Blocked("D05.3"));
    block_on(r.query(&mut p, POINT)).unwrap();
    assert_eq!(
        super::trace::get(conn),
        super::trace::Last {
            seq: 2,
            engine: "poc",
            reason: "D05.3".into()
        }
    );
    block_on(r.query(&mut p, "SET NAMES latin1")).unwrap();
    assert_eq!(
        super::trace::get(conn),
        super::trace::Last {
            seq: 3,
            engine: "poc",
            reason: "kind".into()
        }
    );
    // A parse error never reaches routing.
    assert!(block_on(r.query(&mut p, "SELEKT 1")).is_err());
    assert_eq!(super::trace::get(conn).seq, 3);
}

/// The real session: a text-protocol candidate is handed over to `Compile` (D05.3 spec §9.2)
/// and planned; on the stand-in store (no coprocessor client) the fast plan's push-down check
/// stops (`blocked-by:D09.13`). `fallback` falls back with the session unchanged and the PoC
/// runs a new parse of the text; `strict` returns the 1235 error. Without a database the
/// preprocessor answers Go's 1046 itself (a Go error, no fallback).
#[test]
fn phase0_session_falls_back_or_reports_1235() {
    let mut p = poc();
    p.values.current_db = Some("test".into());
    let mut r = real(NewStackMode::Fallback, &p);
    let before = r.new.session().state_snapshot();
    let res = block_on(r.query(&mut p, POINT));
    assert_eq!(rows(&res).map(|r| r.0), Some(3));
    assert!(p.log.iter().any(|l| l.starts_with("run")));
    // Only the PoC's previous-statement state was written.
    let mut expected = real(NewStackMode::Fallback, &p);
    LastStmtInfo::from_poc(&LastStmtInfo::default(), &res)
        .apply_to(expected.new.session_mut().vars_mut());
    assert_eq!(snap(&r), snap(&expected));
    assert_ne!(r.new.session().state_snapshot(), before);

    let mut p = poc();
    p.values.current_db = Some("test".into());
    let mut r = real(NewStackMode::Strict, &p);
    let e = block_on(r.query(&mut p, POINT)).unwrap_err();
    assert_eq!(e.code, 1235);
    assert!(e.message.contains("blocked-by:D09.13"), "{}", e.message);
    assert!(!p.log.iter().any(|l| l.starts_with("run")));
    // Not a candidate: still the PoC in strict mode.
    assert!(block_on(r.query(&mut p, "SET NAMES utf8mb4")).is_ok());
    assert!(block_on(r.query(&mut p, "SELECT count(*) FROM t")).is_ok());

    let mut p = poc();
    let mut r = real(NewStackMode::Fallback, &p);
    let e = block_on(r.query(&mut p, POINT)).unwrap_err();
    assert_eq!((e.code, e.state), (1046, "3D000"), "{}", e.message);
    assert!(!p.log.iter().any(|l| l.starts_with("run")));
}

// ---------------------------------------------------------------------------------------------
// §4.5: the previous-statement state, every field in both orders.

/// New stack, then PoC: what the new stack's statement left is recorded for the PoC side,
/// field by field (the PoC answers none of these reads itself yet).
#[test]
fn last_stmt_new_then_poc() {
    let mut p = poc();
    let (mut r, script) = scripted(NewStackMode::Fallback, &p);
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

/// §4.5: SHOW WARNINGS / SHOW ERRORS go to the engine that ran the previous statement. After
/// a new-stack statement they are answered from the new session's statement context, with
/// the rows of Go's `fetchShowWarnings` (a plain error is `ErrUnknown` with its text), and
/// they stay the new stack's (Go's SHOW WARNINGS keeps the warnings); after a PoC statement
/// the PoC answers.
#[test]
fn show_warnings_go_to_the_engine_that_ran_the_previous_statement() {
    let mut p = poc();
    let (mut r, script) = scripted(NewStackMode::Fallback, &p);
    script.lock().unwrap().answer = Some(Answer::Rows);
    block_on(r.query(&mut p, POINT)).unwrap();
    let text = |s: &str| Value::Bytes(s.as_bytes().to_vec());
    let warning = vec![text("Warning"), Value::Int(1105), text("new-stack warning")];
    let error = vec![text("Error"), Value::Int(1105), text("new-stack error")];
    for _ in 0..2 {
        let Ok(Response::ResultSet(rs)) = block_on(r.query(&mut p, "SHOW WARNINGS")) else {
            panic!("SHOW WARNINGS");
        };
        assert_eq!(rs.rows, vec![warning.clone(), error.clone()]);
        assert_eq!(rs.columns.len(), 3);
    }
    let Ok(Response::ResultSet(rs)) = block_on(r.query(&mut p, "SHOW ERRORS")) else {
        panic!("SHOW ERRORS");
    };
    assert_eq!(rs.rows, vec![error]);
    assert_eq!(
        script.lock().unwrap().log,
        ["execute"],
        "the new stack ran only the SELECT"
    );
    // A PoC statement next: SHOW WARNINGS is the PoC's again.
    block_on(r.query(&mut p, "SET NAMES utf8mb4")).unwrap();
    assert!(block_on(r.query(&mut p, "SHOW WARNINGS")).is_err());
}

/// The router loads the new session's global variables when the session opens (Go loads them
/// at its first statement): e.g. the prepared plan cache is on, as in Go. Step 0 compares with
/// the loaded values: `tidb_txn_mode` keeps its default, and `tx_read_ts`, whose hook keeps a
/// read ts of 0 for the empty default, does not count until a read ts is set.
#[test]
fn global_variables_are_loaded_when_the_session_opens() {
    let p = poc();
    let (mut r, _script) = scripted(NewStackMode::Strict, &p);
    let vars = r.new.session().vars();
    assert!(vars.enable_prepared_plan_cache);
    assert_eq!(vars.txn_mode, "PESSIMISTIC");
    assert_eq!(
        vars.state
            .txn_read_ts
            .as_ref()
            .map(|t| t.peak_txn_read_ts()),
        Some(0)
    );
    assert_eq!(r.session_condition(&p), None);
    assert_eq!(r.route(&parse(POINT), &p, false), Route::New);
    r.new.session_mut().vars_mut().state.txn_read_ts = Some(TxnReadTS::new(7));
    assert_eq!(r.session_condition(&p), Some("tx_read_ts"));
}

/// Global values the new session cannot read (a stand-in that fails).
struct FailingGlobals;

impl tidb_sessionctx::variable::session::GlobalVarAccessor for FailingGlobals {
    fn get_global_sys_var(&self, _: &str) -> Result<String, Error> {
        Err(errors::new("no global values"))
    }
    fn set_global_sys_var_only(&mut self, _: &str, _: &str, _: bool) -> Result<(), Error> {
        Ok(())
    }
    fn get_tidb_table_value(&self, _: &str) -> Result<String, Error> {
        Ok(String::new())
    }
    fn set_tidb_table_value(&mut self, _: &str, _: &str, _: &str) -> Result<(), Error> {
        Ok(())
    }
}

/// A new session whose global variables cannot be loaded is pinned to the PoC (step 0):
/// strict mode runs the candidate on the PoC too, and the new stack is never called.
#[test]
fn failed_global_variable_load_pins_the_session_to_the_poc() {
    let mut p = poc();
    let script = Arc::new(Mutex::new(Script {
        columns: vec![col("c")],
        answer: Some(Answer::Rows),
        ..Script::default()
    }));
    let new = FakeNew {
        session: Session::new(Dialect::MySQL, Box::new(FailingGlobals)),
        script: script.clone(),
        globals: None,
    };
    let mut r = Router::new(NewStackMode::Strict, Box::new(new), &p.values());
    assert_eq!(r.session_condition(&p), Some("global variables not loaded"));
    assert_eq!(
        r.route(&parse(POINT), &p, false),
        Route::Poc(Reason::Session("global variables not loaded"))
    );
    let res = block_on(r.query(&mut p, POINT));
    assert_eq!(rows(&res).map(|r| r.0), Some(3));
    assert!(script.lock().unwrap().log.is_empty());
}

/// A sysvar cache built from these rows of `mysql.global_variables` (the loader's rebuild,
/// without SetGlobal hooks).
fn cluster_cache(rows: &[(&str, &str)]) -> Arc<tidb_systable::sysvar_cache::SysVarCache> {
    let table: std::collections::HashMap<String, String> = rows
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    let cache = tidb_systable::sysvar_cache::SysVarCache::new();
    cache.rebuild(None, &mut || Ok(table.clone())).unwrap();
    Arc::new(cache)
}

/// A scripted router whose new session reads its global values from `cache`.
fn scripted_over(
    mode: NewStackMode,
    poc: &FakePoc,
    cache: Arc<tidb_systable::sysvar_cache::SysVarCache>,
) -> (Router, Arc<Mutex<Script>>) {
    let script = Arc::new(Mutex::new(Script {
        columns: vec![col("c")],
        ..Script::default()
    }));
    let new = FakeNew {
        session: Session::new(
            Dialect::MySQL,
            Box::new(ClusterGlobals::new(Arc::clone(&cache))),
        ),
        script: script.clone(),
        globals: Some(cache),
    };
    (Router::new(mode, Box::new(new), &poc.values()), script)
}

/// #521: after `SET GLOBAL sql_mode = 'ANSI_QUOTES'` (and the two SET validation switches)
/// on Go TiDB, a new session starts with the cluster's values, not the compiled defaults:
/// parsing respects the cluster's `ANSI_QUOTES` mode (candidates route to the new stack),
/// and `SET sql_mode = DEFAULT` goes back to the cluster's global value (Go
/// `SetExecutor.getVarValue` → `GetGlobalSystemVar`), not to the compiled default.
#[test]
fn cluster_global_values_reach_a_new_session() {
    let mut p = poc();
    let cache = cluster_cache(&[
        ("sql_mode", "ANSI_QUOTES"),
        ("tidb_skip_isolation_level_check", "ON"),
        ("tidb_enable_noop_functions", "ON"),
    ]);
    let (mut r, script) = scripted_over(NewStackMode::Strict, &p, cache);
    assert_eq!(r.session_condition(&p), None);
    assert_eq!(get(&mut r, "sql_mode"), "ANSI_QUOTES");
    assert_eq!(get(&mut r, "tidb_skip_isolation_level_check"), "ON");
    assert_eq!(get(&mut r, "tidb_enable_noop_functions"), "ON");
    let quoted_point = r#"SELECT "c" FROM "sbtest1" WHERE "id" = 1"#;
    assert_eq!(
        r.route(&parse_with(quoted_point, &r), &p, false),
        Route::New
    );
    let sel_x = parse_with(r#"SELECT "x""#, &r);
    assert!(matches!(
        &sel_x,
        ast::Stmt::Select(s) if matches!(
            s.fields.fields[0].expr.as_ref(),
            Some(ast::Expr::Column(_))
        )
    ));
    // The global value as the new session reads it (`@@global.sql_mode`).
    assert_eq!(
        r.new
            .session_mut()
            .vars_mut()
            .get_global_system_var("sql_mode")
            .unwrap(),
        "ANSI_QUOTES"
    );
    // A session value without the bit: `"x"` parses as a string literal again.
    block_on(r.query(&mut p, "SET sql_mode = ''")).unwrap();
    assert_eq!(get(&mut r, "sql_mode"), "");
    assert_eq!(r.route(&parse_with(POINT, &r), &p, false), Route::New);
    assert!(matches!(
        parse_with(r#"SELECT "x""#, &r),
        ast::Stmt::Select(s) if matches!(
            s.fields.fields[0].expr.as_ref(),
            Some(ast::Expr::Value(_))
        )
    ));
    // DEFAULT is the cluster's global value (`ANSI_QUOTES`).
    block_on(r.query(&mut p, "SET sql_mode = DEFAULT")).unwrap();
    assert_eq!(get(&mut r, "sql_mode"), "ANSI_QUOTES");
    assert_eq!(r.session_condition(&p), None);
    assert!(matches!(
        parse_with(r#"SELECT "x""#, &r),
        ast::Stmt::Select(s) if matches!(
            s.fields.fields[0].expr.as_ref(),
            Some(ast::Expr::Column(_))
        )
    ));
    assert!(script.lock().unwrap().log.is_empty());
}

/// #521, fail safe: while the cluster's global values cannot be read (the loader's cache was
/// never built: TiKV unreachable at start, or no loader), a new session's statements stay on
/// the PoC, even in strict mode, and the new stack is never called; once the cache is built,
/// the session loads them at its next command (#539 review).
#[test]
fn unreadable_cluster_global_values_keep_the_session_on_the_poc_until_loaded() {
    let mut p = poc();
    let unbuilt = Arc::new(tidb_systable::sysvar_cache::SysVarCache::new());
    let (mut r, script) = scripted_over(NewStackMode::Strict, &p, Arc::clone(&unbuilt));
    script.lock().unwrap().answer = Some(Answer::Rows);
    assert_eq!(r.session_condition(&p), Some("global variables not loaded"));
    assert_eq!(
        r.route(&parse(POINT), &p, false),
        Route::Poc(Reason::Session("global variables not loaded"))
    );
    let res = block_on(r.query(&mut p, POINT));
    assert_eq!(rows(&res).map(|r| r.0), Some(3));
    assert!(script.lock().unwrap().log.is_empty());
    // The cache is built later: the session loads the cluster's values at its next command
    // (Go would have blocked its first statement on a rebuild) and then runs on the new
    // stack.
    unbuilt
        .rebuild(None, &mut || {
            Ok([("sql_mode".to_owned(), String::new())]
                .into_iter()
                .collect())
        })
        .unwrap();
    assert_eq!(r.session_condition(&p), Some("global variables not loaded"));
    let res = block_on(r.query(&mut p, POINT));
    assert_eq!(rows(&res).map(|r| r.0), Some(1));
    assert_eq!(script.lock().unwrap().log, vec!["execute".to_owned()]);
    assert_eq!(r.session_condition(&p), None);
    assert_eq!(get(&mut r, "sql_mode"), "");
}

/// #539 review: a PREPARE before the global variables are loaded stays on the PoC (step 0),
/// and its plan-cache decision reads the compiled defaults, not the unloaded session's
/// variables (whose `tidb_enable_prepared_plan_cache` is off: Go would not warn "plan cache
/// is disabled").
#[test]
fn prepare_before_the_global_variables_are_loaded_stays_on_the_poc() {
    let mut p = poc();
    let unbuilt = Arc::new(tidb_systable::sysvar_cache::SysVarCache::new());
    let (mut r, script) = scripted_over(NewStackMode::Strict, &p, Arc::clone(&unbuilt));
    script.lock().unwrap().prepare = Some(Answer::Rows);
    block_on(r.prepare(&mut p, 1, POINT)).unwrap();
    assert!(matches!(
        r.owner(1),
        Some(Owner::Poc(Reason::Session("global variables not loaded")))
    ));
    assert!(script.lock().unwrap().log.is_empty());
    let c = p
        .cacheability
        .get(&1)
        .expect("the PoC's plan-cache decision");
    assert_eq!(c.reason, "");
    assert!(c.warnings.is_empty(), "{:?}", c.warnings);
    // Once the cache is built, the next PREPARE runs on both sides.
    unbuilt
        .rebuild(None, &mut || Ok(std::collections::HashMap::new()))
        .unwrap();
    block_on(r.prepare(&mut p, 2, POINT)).unwrap();
    assert!(matches!(r.owner(2), Some(Owner::Both { .. })));
}

/// Go's `session.GetGlobalSysVar` over the cache: the cached value, type-validated (a value
/// that does not validate reads as the default, `ValidateFromType`); an unknown variable is
/// `ErrUnknownSystemVar`; a variable without global scope has its default; a cache that was
/// never built is an error (the read-only node does not rebuild from a session).
#[test]
fn cluster_globals_read_the_cache_as_go_reads_the_domain() {
    use tidb_sessionctx::variable::session::GlobalVarAccessor;
    let acc = ClusterGlobals::new(cluster_cache(&[
        ("max_execution_time", "1500"),
        ("tidb_enable_noop_functions", "not-a-value"),
        ("div_precision_increment", "abc"),
    ]));
    assert_eq!(
        acc.get_global_sys_var("max_execution_time").unwrap(),
        "1500"
    );
    let noop = tidb_sessionctx::variable::get_sys_var("tidb_enable_noop_functions").unwrap();
    assert_eq!(
        acc.get_global_sys_var("tidb_enable_noop_functions")
            .unwrap(),
        noop.value
    );
    assert_eq!(
        acc.get_global_sys_var("div_precision_increment").unwrap(),
        "4"
    );
    let e = acc.get_global_sys_var("no_such_variable").unwrap_err();
    assert!(
        tidb_sessionctx::variable::error::ErrUnknownSystemVar.equal(&e),
        "{e}"
    );
    // `warning_count`: session scope only, no entry in the global cache.
    assert_eq!(acc.get_global_sys_var("warning_count").unwrap(), "0");
    let unbuilt = ClusterGlobals::new(Arc::new(tidb_systable::sysvar_cache::SysVarCache::new()));
    assert!(unbuilt.get_global_sys_var("sql_mode").is_err());
    assert!(matches!(unbuilt.session_cache(), Some(Err(_))));
    let defaults = ClusterGlobals::compiled_defaults();
    let sql_mode = tidb_sessionctx::variable::get_sys_var("sql_mode").unwrap();
    assert_eq!(
        defaults.get_global_sys_var("sql_mode").unwrap(),
        sql_mode.value
    );
}

/// PoC, then new stack: the PoC's statement is written into the new session, where Go's
/// system variables read it.
#[test]
fn last_stmt_poc_then_new() {
    let mut p = poc();
    let (mut r, script) = scripted(NewStackMode::Fallback, &p);
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
    let (mut r, _) = scripted(NewStackMode::Fallback, &p);
    block_on(r.query(&mut p, "SET NAMES latin1")).unwrap();
    assert_eq!(get(&mut r, "character_set_client"), "latin1");
    assert_eq!(get(&mut r, "character_set_connection"), "latin1");
    assert_eq!(get(&mut r, "character_set_results"), "latin1");
    assert_eq!(get(&mut r, "collation_connection"), "latin1_bin");
    block_on(r.query(&mut p, "SET time_zone = '+08:00'")).unwrap();
    assert_eq!(get(&mut r, "time_zone"), "+08:00");
    block_on(r.query(&mut p, "SET sql_mode = 'ANSI_QUOTES'")).unwrap();
    assert_eq!(get(&mut r, "sql_mode"), "ANSI_QUOTES");
    // What drivers SET while connecting reaches the new session too (#505); a removed
    // variable is ignored, as Go ignores it, and diverges nothing.
    block_on(r.query(
        &mut p,
        "SET SQL_SELECT_LIMIT=DEFAULT, group_concat_max_len = 1048576, sql_safe_updates = 0, \
         foreign_key_checks = 0, unique_checks = 0, innodb_lock_wait_timeout = 100, \
         max_execution_time = 2.0e3, tidb_enable_streaming = 1",
    ))
    .unwrap();
    assert_eq!(get(&mut r, "sql_select_limit"), "18446744073709551615");
    assert_eq!(get(&mut r, "group_concat_max_len"), "1048576");
    assert_eq!(get(&mut r, "sql_safe_updates"), "OFF");
    assert_eq!(get(&mut r, "foreign_key_checks"), "OFF");
    assert_eq!(get(&mut r, "unique_checks"), "OFF");
    assert_eq!(get(&mut r, "innodb_lock_wait_timeout"), "100");
    assert_eq!(get(&mut r, "max_execution_time"), "2000");
    assert_eq!(r.session_condition(&p), None);
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
    let mut r = real(NewStackMode::Fallback, &p);
    assert_eq!(get(&mut r, "collation_connection"), "utf8_general_ci");
}

// ---------------------------------------------------------------------------------------------
// §4.7: prepared statements.

#[test]
fn prepared_both() {
    let mut p = poc();
    let (mut r, script) = scripted(NewStackMode::Fallback, &p);
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
    let (mut r, script) = scripted(NewStackMode::Fallback, &p);
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
    let (mut r, script) = scripted(NewStackMode::Fallback, &p);
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
    let (mut r, script) = scripted(NewStackMode::Fallback, &p);
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
    let (mut r, script) = scripted(NewStackMode::Fallback, &p);
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
    let (mut r, script) = scripted(NewStackMode::Fallback, &p);
    script.lock().unwrap().prepare = Some(Answer::Rows);
    block_on(r.prepare(&mut p, 1, POINT)).unwrap();
    script.lock().unwrap().prepare = Some(Answer::Blocked("D13.1"));
    block_on(r.prepare(&mut p, 2, POINT)).unwrap();
    p.prepare_fails = true;
    script.lock().unwrap().prepare = Some(Answer::Rows);
    block_on(r.prepare(&mut p, 3, POINT)).unwrap();
    for id in [1, 2, 3] {
        r.close(&mut p, id);
        // Closed, but kept until the transaction ends (Go's DropPreparedStmt).
        assert!(r.owner(id).is_some());
    }
    let new_log = script.lock().unwrap().log.clone();
    assert!(new_log.contains(&"close 101".to_owned()));
    assert!(new_log.contains(&"close 102".to_owned()));
    assert!(!p.log.iter().any(|l| l.starts_with("close")));
    // The next statement ends the (autocommit) transaction: the PoC's copies are closed.
    script.lock().unwrap().answer = Some(Answer::Blocked("D09.1"));
    block_on(r.query(&mut p, POINT)).unwrap();
    for id in [1, 2, 3] {
        assert_eq!(r.owner(id), None);
    }
    assert!(p.log.contains(&"close 1".to_owned()));
    assert!(p.log.contains(&"close 2".to_owned()));
    assert!(!p.log.contains(&"close 3".to_owned()));
}

/// `BEGIN; PREPARE; CLOSE; EXECUTE` (Go: the closed statement stays executable until the
/// transaction ends, `DropPreparedStmt` / `cleanRetryInfo`): the EXECUTE in the transaction
/// succeeds; the statement is gone after COMMIT, on both engines.
#[test]
fn closed_stmt_lives_until_txn_end() {
    let mut p = poc();
    let (mut r, script) = scripted(NewStackMode::Fallback, &p);
    p.in_txn = true; // BEGIN (on the PoC: an explicit transaction)
    script.lock().unwrap().prepare = Some(Answer::Rows);
    block_on(r.prepare(&mut p, 1, POINT)).unwrap();
    assert!(matches!(r.owner(1), Some(Owner::Both { .. })));
    r.close(&mut p, 1);
    assert!(script.lock().unwrap().log.contains(&"close 101".to_owned()));
    let res = block_on(r.execute(&mut p, 1, vec![]));
    assert_eq!(rows(&res).map(|r| r.0), Some(3));
    let res = block_on(r.execute(&mut p, 1, vec![]));
    assert_eq!(
        rows(&res).map(|r| r.0),
        Some(3),
        "still there inside the transaction"
    );
    assert!(!p.log.contains(&"close 1".to_owned()));
    // COMMIT ends the transaction (a statement the PoC runs).
    p.in_txn = false;
    script.lock().unwrap().answer = Some(Answer::Blocked("D09.1"));
    block_on(r.query(&mut p, POINT)).unwrap();
    assert!(p.log.contains(&"close 1".to_owned()));
    assert_eq!(r.owner(1), None);
    let e = block_on(r.execute(&mut p, 1, vec![])).unwrap_err();
    assert_eq!(e.code, 1243, "{e:?}");
}

/// Autocommit: `CLOSE 1; PREPARE …; EXECUTE 1`. Go's `PrepareStmt` ends with
/// `rollbackOnError`, which outside a transaction runs `cleanRetryInfo`: statement 1 is gone
/// before the EXECUTE (1243), on both engines.
#[test]
fn autocommit_close_then_prepare() {
    let mut p = poc();
    let (mut r, script) = scripted(NewStackMode::Fallback, &p);
    script.lock().unwrap().prepare = Some(Answer::Rows);
    block_on(r.prepare(&mut p, 1, POINT)).unwrap();
    r.close(&mut p, 1);
    assert!(r.owner(1).is_some());
    block_on(r.prepare(&mut p, 2, POINT)).unwrap();
    assert!(p.log.contains(&"close 1".to_owned()));
    assert_eq!(r.owner(1), None);
    assert!(r.owner(2).is_some());
    let e = block_on(r.execute(&mut p, 1, vec![])).unwrap_err();
    assert_eq!(e.code, 1243, "{e:?}");
}

/// Autocommit: `CLOSE` followed by a statement that the PoC runs. Go removes the closed
/// statement at the end of that statement (its autocommit transaction): the PoC's copy is
/// closed and the new session's statement removed (it no longer counts toward
/// `max_prepared_stmt_count`), not kept until the new session's own next statement.
#[test]
fn autocommit_close_then_poc_statement() {
    let mut p = poc();
    let (mut r, script) = scripted(NewStackMode::Fallback, &p);
    script.lock().unwrap().prepare = Some(Answer::Rows);
    block_on(r.prepare(&mut p, 1, POINT)).unwrap();
    // A statement in the new session standing for the scripted one (101).
    let vars = r.new.session_mut().vars_mut();
    let id = vars.get_next_prepared_stmt_id();
    vars.add_prepared_stmt(id, Arc::new(())).unwrap();
    r.new.session_mut().drop_prepared_stmt(id).unwrap();
    r.close(&mut p, 1);
    assert!(
        r.new
            .session()
            .vars()
            .state
            .prepared_stmts
            .contains_key(&id)
    );
    // A statement the PoC runs (SET is not a candidate for the new stack).
    block_on(r.query(&mut p, "SET NAMES utf8mb4")).unwrap();
    assert!(p.log.contains(&"close 1".to_owned()));
    assert_eq!(r.owner(1), None);
    assert!(
        !r.new
            .session()
            .vars()
            .state
            .prepared_stmts
            .contains_key(&id),
        "removed at the end of the PoC's statement, as Go"
    );
}

/// The real new session prepares a candidate (`PrepareStmt`: transaction context, statement
/// start, TSO warm-up, the prepare executor, whose `PlanBuilder.Build` runs in the planning
/// context the session hands the planner): the statement is prepared on both sides, with the
/// new stack's column definitions. EXECUTE reaches the compiler's planner, a boundary
/// ([`REAL_EXECUTE_BLOCK`]): `fallback` answers from the PoC and leaves the new session as
/// before (no trace of the attempt), `strict` reports the block (§4.7 row 2).
#[test]
fn real_prepare_builds_the_plan_and_execute_falls_back() {
    for mode in [NewStackMode::Fallback, NewStackMode::Strict] {
        let mut p = poc();
        // A database is selected: without one the preprocessor answers Go's 1046.
        p.values.current_db = Some("test".into());
        let mut r = real(mode, &p);
        block_on(r.prepare(&mut p, 1, POINT)).unwrap();
        let Some(Owner::Both { columns, .. }) = r.owner(1).cloned() else {
            panic!("{mode:?}: {:?}", r.owner(1));
        };
        let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["c"], "{mode:?}");
        assert_eq!(columns[0].table, "sbtest1");
        assert!(r.new.session().is_idle());
        // The fake PoC answers with the same column definitions as a real one would (the
        // router checks a fallback's columns against the prepared ones).
        p.columns = columns.clone();
        let before = snap(&r);
        let res = block_on(r.execute(&mut p, 1, vec![]));
        match mode {
            NewStackMode::Strict => {
                let e = res.unwrap_err();
                assert!(
                    e.code == 1235
                        && e.message
                            .contains(&format!("blocked-by:{REAL_EXECUTE_BLOCK}")),
                    "{e:?}"
                );
            }
            _ => {
                assert_eq!(rows(&res).map(|r| r.0), Some(3), "{res:?}");
                // Only the PoC's previous-statement state was written (found rows).
                assert_ne!(snap(&r), before);
            }
        }
        assert!(r.new.session().is_idle(), "{mode:?}");
        assert_eq!(r.new.session().executed_stmts(), 0, "{mode:?}");
        r.close(&mut p, 1);
        // Closed at the end of the next statement (Go's cleanRetryInfo).
        let _ = block_on(r.query(&mut p, "SET NAMES utf8mb4"));
        assert!(p.log.contains(&"close 1".to_owned()));
        assert_eq!(r.owner(1), None);
    }
}

/// Where a real EXECUTE of [`POINT`] stops on the new session: the session hands the
/// statement to `ExecuteStmt` as an EXECUTE node, `ResetContextOfStmt` reads the prepared
/// statement, and `Compile` runs `planner.Optimize`, whose fast plan needs the store client
/// of the planning context (the push down checks, D09.13): the router's session runs over
/// the stand-in store, which has none.
const REAL_EXECUTE_BLOCK: &str = "D09.13";

/// The real new session's `ExecutePreparedStmt` / `DropPreparedStmt` of an id it never
/// prepared: Go's `ErrStmtNotFound` (8111), not a boundary stub; with no plan cache
/// switched on, `GetSessionPlanCache` is `None`, otherwise the (empty) stand-in cache.
#[test]
fn real_session_prepared_lookup() {
    let mut s = Session::new(
        Dialect::MySQL,
        Box::new(ClusterGlobals::compiled_defaults()),
    );
    let e = block_on(s.execute_prepared_stmt(7, vec![])).err().unwrap();
    assert_eq!(e.code(), 8111, "{e}");
    let e = s.drop_prepared_stmt(7).unwrap_err();
    assert_eq!(e.code(), 8111, "{e}");
    s.vars_mut().enable_prepared_plan_cache = false;
    s.vars_mut().enable_non_prepared_plan_cache = false;
    assert!(s.get_session_plan_cache().is_none());
    s.vars_mut().enable_prepared_plan_cache = true;
    assert!(s.get_session_plan_cache().unwrap().deleted_keys.is_empty());
}

/// COM_STMT_SEND_LONG_DATA (§4.7): the protocol layer folds long data into the decoded
/// parameters (ADR 0010 §4.1), and the router hands them to the engine that runs the
/// EXECUTE: the new stack, or the PoC on a fallback (owner both), or the only owner.
#[test]
fn long_data_params_reach_the_engine_that_runs() {
    let long = || {
        vec![Param {
            ty: ColumnType::VarString,
            unsigned: false,
            value: Value::Bytes(b"long data".to_vec()),
        }]
    };
    let mut p = poc();
    let (mut r, script) = scripted(NewStackMode::Fallback, &p);
    script.lock().unwrap().prepare = Some(Answer::Rows);
    block_on(r.prepare(&mut p, 1, "SELECT c FROM t WHERE id = ?")).unwrap();
    script.lock().unwrap().answer = Some(Answer::Rows);
    block_on(r.execute(&mut p, 1, long())).unwrap();
    let want = format!("params {:?}", long());
    assert!(script.lock().unwrap().log.contains(&want));
    script.lock().unwrap().answer = Some(Answer::Blocked("D10.2"));
    block_on(r.execute(&mut p, 1, long())).unwrap();
    assert!(p.log.contains(&want), "{:?}", p.log);
    // PoC-owned.
    script.lock().unwrap().prepare = Some(Answer::Blocked("D12.12"));
    p.log.clear();
    block_on(r.prepare(&mut p, 2, "SELECT c FROM t WHERE id = ?")).unwrap();
    block_on(r.execute(&mut p, 2, long())).unwrap();
    assert!(p.log.contains(&want), "{:?}", p.log);
}

/// Every COM_STMT_EXECUTE starts its statement exactly once (the statement counter feeds the
/// weekly QPS): answered by the new stack, fallen back, or 1235.
#[test]
fn execute_starts_the_statement_once() {
    let starts = |p: &FakePoc| p.log.iter().filter(|l| l.starts_with("start")).count();
    for (mode, answer) in [
        (NewStackMode::Fallback, Answer::Rows),
        (NewStackMode::Fallback, Answer::Blocked("D10.2")),
        (NewStackMode::Fallback, Answer::GoError),
        (NewStackMode::Strict, Answer::Blocked("D10.2")),
    ] {
        let mut p = poc();
        let (mut r, script) = scripted(mode, &p);
        script.lock().unwrap().prepare = Some(Answer::Rows);
        block_on(r.prepare(&mut p, 1, POINT)).unwrap();
        script.lock().unwrap().answer = Some(answer.clone());
        p.log.clear();
        let _ = block_on(r.execute(&mut p, 1, vec![]));
        assert_eq!(starts(&p), 1, "{mode:?} {answer:?}: {:?}", p.log);
    }
    // New-only statements too.
    let mut p = poc();
    p.prepare_fails = true;
    let (mut r, script) = scripted(NewStackMode::Fallback, &p);
    script.lock().unwrap().prepare = Some(Answer::Rows);
    block_on(r.prepare(&mut p, 1, POINT)).unwrap();
    for answer in [Answer::Rows, Answer::Blocked("D10.4")] {
        script.lock().unwrap().answer = Some(answer.clone());
        p.log.clear();
        let _ = block_on(r.execute(&mut p, 1, vec![]));
        assert_eq!(starts(&p), 1, "new-only {answer:?}: {:?}", p.log);
    }
}

/// PREPARE while the InfoSchema versions differ stays on the PoC (the new stack's column
/// definitions would come from another version).
#[test]
fn prepare_with_unequal_schema_versions_is_poc_owned() {
    let mut p = poc();
    let (mut r, script) = scripted(NewStackMode::Fallback, &p);
    script.lock().unwrap().version = Some(2);
    block_on(r.prepare(&mut p, 1, POINT)).unwrap();
    assert_eq!(r.owner(1), Some(&Owner::Poc(Reason::SchemaVersion)));
    assert!(script.lock().unwrap().log.is_empty());
}

/// Prepared statements: without an admissible schema lease, PREPARE stays on the PoC, an
/// EXECUTE of a statement prepared on both sides runs on the PoC, and one only the new stack
/// prepared gets the 1235 that names the lease.
#[test]
fn prepared_without_admissible_schema_lease() {
    let mut p = poc();
    let (mut r, script) = scripted(NewStackMode::Fallback, &p);
    script.lock().unwrap().version = Some(1);
    script.lock().unwrap().prepare = Some(Answer::Rows);
    block_on(r.prepare(&mut p, 1, POINT)).unwrap();
    assert!(matches!(r.owner(1), Some(Owner::Both { .. })));
    p.schema_version = None;
    block_on(r.prepare(&mut p, 2, POINT)).unwrap();
    assert_eq!(r.owner(2), Some(&Owner::Poc(Reason::SchemaLease)));
    script.lock().unwrap().log.clear();
    p.log.clear();
    let res = block_on(r.execute(&mut p, 1, vec![]));
    assert_eq!(rows(&res).map(|r| r.0), Some(3));
    assert!(p.log.contains(&"execute 1".to_owned()));
    assert!(script.lock().unwrap().log.is_empty());
    // New-only: 1235, counted with the schema lease as its reason.
    super::trace::enable();
    let conn = 7_000_475;
    let mut p = poc();
    p.prepare_fails = true;
    let (mut r, script) = scripted(NewStackMode::Fallback, &p);
    r.set_connection_id(conn);
    script.lock().unwrap().version = Some(1);
    script.lock().unwrap().prepare = Some(Answer::Rows);
    block_on(r.prepare(&mut p, 1, POINT)).unwrap();
    assert_eq!(r.owner(1), Some(&Owner::New { new_id: 101 }));
    p.schema_version = None;
    let e = block_on(r.execute(&mut p, 1, vec![])).unwrap_err();
    assert!(
        e.code == 1235 && e.message.contains("schema lease"),
        "{}",
        e.message
    );
    let last = super::trace::get(conn);
    assert_eq!(
        (last.engine, last.reason.as_str()),
        (NOWHERE, "schema_lease")
    );
    // Once the lease is admissible again, the same statement runs on the new stack: no
    // re-PREPARE.
    p.schema_version = Some(1);
    script.lock().unwrap().answer = Some(Answer::Rows);
    let res = block_on(r.execute(&mut p, 1, vec![]));
    assert_eq!(rows(&res).map(|r| r.0), Some(1));
}

/// Lucky (#235): the router checks that the new session is idle before it tries a statement
/// there. A busy session (here a pending transaction) is a router bug: the statement goes to
/// the PoC (reason `not_idle`) in both modes and the new stack is not asked, so nothing is
/// written to the new session that a restore would have to undo.
#[test]
fn not_idle_new_session_goes_to_the_poc() {
    for mode in [NewStackMode::Fallback, NewStackMode::Strict] {
        let mut p = poc();
        let (mut r, script) = scripted(mode, &p);
        script.lock().unwrap().answer = Some(Answer::Rows);
        let core = r.new.session_mut().core_mut();
        block_on(core.prepare_txn_ctx()).unwrap();
        tidb_sessiontxn::TxnManager::advise_warmup(core).unwrap();
        assert!(!r.new.session().is_idle());
        let before = r.new.session().state_snapshot();
        let res = block_on(r.query(&mut p, POINT));
        assert_eq!(rows(&res).map(|r| r.0), Some(3), "{mode:?}");
        assert!(script.lock().unwrap().log.is_empty(), "{mode:?}");
        assert_eq!(Reason::NotIdle.label(), "not_idle");
        // Only the PoC's previous-statement state was written to the new session.
        let after = r.new.session().state_snapshot();
        assert!(after.contains("txn=txnFuture"), "{mode:?}");
        assert_ne!(before, after);
    }
}

/// VS1 plan §4.3: a fallback after the new stack's executors opened is detected (and logged
/// and counted as a bug by `try_new`).
#[test]
fn fallback_after_open_is_detected() {
    assert!(!fallback_after_open(3, 3));
    assert!(fallback_after_open(3, 4));
}

/// VS1 plan §4.3: a `Blocked` after the executors opened is not a fallback. The statement
/// may have read data and written session state; the PoC does not run it again. The
/// client gets the 1235 of the stub (constitution article 5), the statement ends on the
/// new stack, and `tidbrs_route_total{engine="new",reason="bug_blocked_after_open"}`
/// counts it. A `Blocked` before (the builder's stubs) still falls back.
#[test]
fn blocked_after_open_is_an_error_not_a_fallback() {
    for mode in [NewStackMode::Fallback, NewStackMode::Strict] {
        let mut p = poc();
        let (mut r, script) = scripted(mode, &p);
        script.lock().unwrap().answer = Some(Answer::BlockedAfterOpen("D12.2"));
        let e = block_on(r.query(&mut p, POINT)).unwrap_err();
        assert_eq!(
            e,
            SqlError {
                code: 1235,
                state: "42000",
                message: "This version of TiDB doesn't yet support 'something after the \
                          executors opened (blocked-by:D12.2)'"
                    .into(),
                source_err: None,
            },
            "{mode:?}"
        );
        assert!(
            !p.log.iter().any(|l| l.starts_with("run")),
            "{mode:?}: no fallback"
        );
        assert_eq!(r.last_stmt().engine, Engine::New);
        // The new session kept what the statement wrote (no checkpoint restore).
        assert_eq!(r.new.session().vars().last_found_rows, 5);
    }
    // Before the executors opened: the normal fallback.
    let mut p = poc();
    let (mut r, script) = scripted(NewStackMode::Fallback, &p);
    script.lock().unwrap().answer = Some(Answer::Blocked("D12.2"));
    assert_eq!(
        rows(&block_on(r.query(&mut p, POINT))).map(|r| r.0),
        Some(3)
    );
    assert!(p.log.iter().any(|l| l.starts_with("run")));
}

/// `--new-stack=off` (the default) builds no ported domain; the other modes do.
#[test]
fn ported_domain_only_with_the_new_stack_on() {
    assert!(!NewStackMode::default().uses_ported_domain());
    assert!(!NewStackMode::Off.uses_ported_domain());
    assert!(NewStackMode::Fallback.uses_ported_domain());
    assert!(NewStackMode::Strict.uses_ported_domain());
}

/// EXPLAIN never routes to the new stack until the new stack has an EXPLAIN (its own task):
/// even with the optimizer driver ported and a new stack that would answer, `EXPLAIN` and
/// `EXPLAIN ANALYZE` of a candidate SELECT never reach the new stack, in `fallback` and in
/// `strict` mode (the new stack is not asked at all; the PoC owns them).
#[test]
fn explain_is_never_routed_to_the_new_stack() {
    for mode in [NewStackMode::Fallback, NewStackMode::Strict] {
        for sql in [
            format!("EXPLAIN {POINT}"),
            format!("EXPLAIN ANALYZE {POINT}"),
            format!("EXPLAIN FORMAT = 'brief' {POINT}"),
            format!("DESC {POINT}"),
        ] {
            let mut p = poc();
            let (mut r, script) = scripted(mode, &p);
            script.lock().unwrap().answer = Some(Answer::Rows);
            let _ = block_on(r.query(&mut p, &sql));
            assert!(
                script.lock().unwrap().log.is_empty(),
                "{mode:?} {sql}: {:?}",
                script.lock().unwrap().log
            );
            // Whatever the PoC answers (or its parser rejects), the new session is untouched.
            assert_eq!(r.new.session().executed_stmts(), 0, "{mode:?} {sql}");
        }
    }
}

/// `tidb_cost_model_version = 1`: the cost model v1 is not ported (exception B5), and the
/// new stack's planner answers a SELECT with its boundary (`blocked-by:EXC.B`, pinned by the
/// planner test `select_with_cost_model_v1_is_the_cost_model_boundary`). The router falls
/// back to the prototype in `fallback` mode (the new session executed nothing) and returns
/// the 1235 of the boundary in `strict` mode.
#[test]
fn select_with_cost_model_v1_falls_back_to_the_prototype() {
    let mut p = poc();
    let (mut r, script) = scripted(NewStackMode::Fallback, &p);
    script.lock().unwrap().answer = Some(Answer::Blocked("EXC.B"));
    assert_eq!(
        rows(&block_on(r.query(&mut p, POINT))).map(|r| r.0),
        Some(3)
    );
    assert!(p.log.iter().any(|l| l.starts_with("run")));
    assert_eq!(r.new.session().executed_stmts(), 0);

    let mut p = poc();
    let (mut s, sscript) = scripted(NewStackMode::Strict, &p);
    sscript.lock().unwrap().answer = Some(Answer::Blocked("EXC.B"));
    let e = block_on(s.query(&mut p, POINT)).unwrap_err();
    assert_eq!(e.code, 1235);
}

/// An instance plan cache that counts its flushes.
#[derive(Default)]
struct CountingCache {
    flushes: std::sync::atomic::AtomicUsize,
}

impl tidb_sessionctx::context::InstancePlanCache for CountingCache {
    fn evict(&self) -> (String, usize) {
        (String::new(), 0)
    }
    fn size(&self) -> i64 {
        0
    }
    fn mem_usage(&self) -> i64 {
        0
    }
    fn get_limits(&self) -> (i64, i64) {
        (0, 0)
    }
    fn set_limits(&self, _: i64, _: i64) {}
    fn delete_all(&self) -> usize {
        self.flushes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        0
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// `ADMIN FLUSH <scope> PLAN_CACHE` runs on the new session in every routing mode (the plan
/// cache is the new stack's): INSTANCE clears the instance plan cache (the plan-cache
/// exception A3, decision 8), SESSION (and no scope) does nothing, GLOBAL is Go's error, and
/// with `tidb_enable_prepared_plan_cache` off it is Go's warning only, which SHOW WARNINGS
/// reads from the new session.
#[test]
fn admin_flush_plan_cache_clears_the_instance_cache() {
    for mode in [NewStackMode::Fallback, NewStackMode::Strict] {
        let mut p = poc();
        let (mut r, script) = scripted(mode, &p);
        let cache = Arc::new(CountingCache::default());
        r.new
            .session_mut()
            .set_instance_plan_cache(Some(Arc::clone(&cache) as _));
        r.new.session_mut().vars_mut().enable_prepared_plan_cache = true;
        let flushes = || cache.flushes.load(std::sync::atomic::Ordering::SeqCst);

        let ok = block_on(r.query(&mut p, "ADMIN FLUSH INSTANCE PLAN_CACHE")).unwrap();
        assert_eq!(ok, Response::Ok(OkInfo::default()));
        assert_eq!(flushes(), 1);
        assert_eq!(r.last_stmt().engine, Engine::New);
        block_on(r.query(&mut p, "ADMIN FLUSH SESSION PLAN_CACHE")).unwrap();
        block_on(r.query(&mut p, "ADMIN FLUSH PLAN_CACHE")).unwrap();
        assert_eq!(flushes(), 1);
        let e = block_on(r.query(&mut p, "ADMIN FLUSH GLOBAL PLAN_CACHE")).unwrap_err();
        assert_eq!(
            (e.code, e.message.as_str()),
            (1105, "Do not support the 'admin flush global scope.'")
        );

        r.new.session_mut().vars_mut().enable_prepared_plan_cache = false;
        let ok = block_on(r.query(&mut p, "ADMIN FLUSH INSTANCE PLAN_CACHE")).unwrap();
        assert_eq!(
            ok,
            Response::Ok(OkInfo {
                warnings: 1,
                ..OkInfo::default()
            })
        );
        assert_eq!(flushes(), 1);
        let Ok(Response::ResultSet(rs)) = block_on(r.query(&mut p, "SHOW WARNINGS")) else {
            panic!("SHOW WARNINGS")
        };
        assert_eq!(
            rs.rows[0][2],
            Value::from("The plan cache is disable. So there no need to flush the plan cache")
        );
        // Nothing reached the scripted new stack's statement path or the PoC.
        assert!(script.lock().unwrap().log.is_empty());
    }
}

/// Only `ADMIN FLUSH INSTANCE PLAN_CACHE` clears the instance plan cache. As in Go:
/// `SESSION` and no scope (`ast.StatementScopeSession`) clear only the session-level cache,
/// which tidb-rs does not have, so they return OK with no warning and leave the instance cache
/// alone; `GLOBAL` is Go's error `Do not support the 'admin flush global scope.'` (1105) and
/// clears nothing. Each runs in a fresh router.
#[test]
fn admin_flush_plan_cache_scopes_as_go() {
    type Want = Result<OkInfo, (u16, &'static str)>;
    let cases: [(&str, Want, usize); 4] = [
        ("ADMIN FLUSH SESSION PLAN_CACHE", Ok(OkInfo::default()), 0),
        ("ADMIN FLUSH PLAN_CACHE", Ok(OkInfo::default()), 0),
        (
            "ADMIN FLUSH GLOBAL PLAN_CACHE",
            Err((1105, "Do not support the 'admin flush global scope.'")),
            0,
        ),
        ("ADMIN FLUSH INSTANCE PLAN_CACHE", Ok(OkInfo::default()), 1),
    ];
    for (sql, want, want_flushes) in cases {
        let mut p = poc();
        let (mut r, script) = scripted(NewStackMode::Fallback, &p);
        let cache = Arc::new(CountingCache::default());
        r.new
            .session_mut()
            .set_instance_plan_cache(Some(Arc::clone(&cache) as _));
        r.new.session_mut().vars_mut().enable_prepared_plan_cache = true;
        let got = block_on(r.query(&mut p, sql));
        match (&got, &want) {
            (Ok(Response::Ok(ok)), Ok(w)) => assert_eq!(ok, w, "{sql}"),
            (Err(e), Err((code, msg))) => {
                assert_eq!((e.code, e.message.as_str()), (*code, *msg), "{sql}")
            }
            _ => panic!("{sql}: {got:?}"),
        }
        assert_eq!(
            cache.flushes.load(std::sync::atomic::Ordering::SeqCst),
            want_flushes,
            "{sql}"
        );
        assert_eq!(r.last_stmt().engine, Engine::New, "{sql}");
        assert!(script.lock().unwrap().log.is_empty(), "{sql}");
    }
}

#[test]
fn parse_error_wrapping_has_no_double_trailing_space() {
    let msg = "You have an error in your SQL syntax; check the manual that corresponds to your TiDB version for the right syntax to use line 3 column 7 near \"FROM nonexistent\" ";
    let err = log_and_wrap_parse_error(
        7,
        "",
        "OFF",
        "SELECT\n  1,\n  FROM nonexistent",
        SqlError::new(1064, msg),
    );
    let src = err.source_err.expect("source_err attached");
    let rendered = src.to_string();
    assert_eq!(rendered, format!("[parser:1064]{msg}"));
    assert!(
        !rendered.ends_with("  "),
        "must not append a second trailing space: {rendered:?}"
    );
}

#[test]
fn set_connection_id_propagates_to_new_stack_session_vars() {
    let p = poc();
    let (mut r, _) = scripted(NewStackMode::Fallback, &p);
    assert_eq!(r.new.session().vars().connection_id, 0);
    r.set_connection_id(2097158);
    assert_eq!(r.conn_id, 2097158);
    assert_eq!(r.new.session().vars().connection_id, 2097158);
}

// ---------------------------------------------------------------------------------------------
// #512: Go's prepare-time plan-cache decision for the statements the PoC prepared.

fn warning_texts(c: &crate::plan_cache::PrepareCacheability) -> Vec<String> {
    c.warnings
        .iter()
        .map(|w| format!("{}: {}", w.level, String::from_utf8_lossy(&w.err.error())))
        .collect()
}

/// A statement only the PoC prepares (a system variable read) gets Go's reason, decided with
/// the new session's variables; SHOW WARNINGS then goes to the PoC. A cacheable one gets
/// none.
#[test]
fn poc_prepare_takes_the_plan_cache_decision() {
    let mut p = poc();
    let (mut r, script) = scripted(NewStackMode::Fallback, &p);
    r.last.engine = Engine::New;
    block_on(r.prepare(&mut p, 1, "SELECT @@max_allowed_packet")).unwrap();
    assert_eq!(r.owner(1), Some(&Owner::Poc(Reason::Ast("variable"))));
    assert!(script.lock().unwrap().log.is_empty());
    let c = &p.cacheability[&1];
    assert_eq!(c.reason, "query has user-defined variables is un-cacheable");
    assert_eq!(
        warning_texts(c),
        ["Warning: skip prepared plan-cache: query has user-defined variables is un-cacheable"]
    );
    assert_eq!(r.last.engine, Engine::Poc);
    // Cacheable (no InfoSchema here: the table checks are left out).
    block_on(r.prepare(&mut p, 2, "SELECT count(*) FROM t")).unwrap();
    assert_eq!(
        p.cacheability[&2],
        crate::plan_cache::PrepareCacheability::default()
    );
}

/// The decision reads the new session's (synced) variables: with the prepared plan cache
/// switched off there, the reason is "plan cache is disabled".
#[test]
fn poc_prepare_uses_the_new_session_variables() {
    let mut p = poc();
    let (mut r, _script) = scripted(NewStackMode::Fallback, &p);
    r.new
        .session_mut()
        .vars_mut()
        .set_system_var("tidb_enable_prepared_plan_cache", "OFF")
        .unwrap();
    block_on(r.prepare(&mut p, 1, "SELECT count(*) FROM t")).unwrap();
    assert_eq!(p.cacheability[&1].reason, "plan cache is disabled");
}

/// Prepared on both sides: the PoC's copy gets the decision too (it answers a fallback
/// EXECUTE), and SHOW WARNINGS goes to the new session, which ran Go's PREPARE.
#[test]
fn both_prepared_statement_gets_the_decision_and_new_stack_warnings() {
    let mut p = poc();
    let (mut r, script) = scripted(NewStackMode::Fallback, &p);
    script.lock().unwrap().prepare = Some(Answer::Rows);
    block_on(r.prepare(&mut p, 1, POINT)).unwrap();
    assert!(matches!(r.owner(1), Some(Owner::Both { .. })));
    assert_eq!(
        p.cacheability[&1],
        crate::plan_cache::PrepareCacheability::default()
    );
    assert_eq!(r.last.engine, Engine::New);
    // New-stack Blocked at PREPARE: the PoC owns it and answers SHOW WARNINGS.
    script.lock().unwrap().prepare = Some(Answer::Blocked("D13.1"));
    block_on(r.prepare(&mut p, 2, POINT)).unwrap();
    assert_eq!(r.last.engine, Engine::Poc);
    assert!(p.cacheability.contains_key(&2));
    // Prepared only by the new stack: no PoC copy, no decision for it.
    p.prepare_fails = true;
    script.lock().unwrap().prepare = Some(Answer::Rows);
    block_on(r.prepare(&mut p, 3, POINT)).unwrap();
    assert!(!p.cacheability.contains_key(&3));
    assert_eq!(r.last.engine, Engine::New);
}

/// With an admissible InfoSchema, the decision checks the statement's tables in the new
/// session's InfoSchema (a plain table: cacheable).
#[test]
fn poc_prepare_checks_tables_in_the_new_session_info_schema() {
    let mut p = poc();
    let (mut r, script) = scripted(NewStackMode::Fallback, &p);
    script.lock().unwrap().version = Some(1);
    r.new
        .session_mut()
        .set_domain_info_schema(sbtest_info_schema());
    block_on(r.prepare(&mut p, 1, "SELECT count(*) FROM test.sbtest1")).unwrap();
    assert_eq!(r.owner(1), Some(&Owner::Poc(Reason::Ast("aggregate"))));
    assert_eq!(
        p.cacheability[&1],
        crate::plan_cache::PrepareCacheability::default()
    );
}

/// An error the router answers itself (strict mode, nothing to fall back to) clears the
/// PoC's warnings, so a SHOW WARNINGS after it does not show an older statement's.
#[test]
fn router_error_clears_the_poc_warnings() {
    let mut p = poc();
    let (mut s, sscript) = scripted(NewStackMode::Strict, &p);
    sscript.lock().unwrap().prepare = Some(Answer::Blocked("D13.1"));
    block_on(s.prepare(&mut p, 1, POINT)).unwrap();
    assert_eq!(p.warnings_cleared, 0);
    block_on(s.execute(&mut p, 1, vec![])).unwrap_err();
    assert_eq!(p.warnings_cleared, 1);
    assert_eq!(s.last.engine, Engine::Poc);
    // A statement the PoC answered: the PoC cleared its own list, the router does not.
    block_on(s.prepare(&mut p, 2, "SELECT count(*) FROM t")).unwrap();
    block_on(s.execute(&mut p, 2, vec![])).unwrap();
    assert_eq!(p.warnings_cleared, 1);
}
