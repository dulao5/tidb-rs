//! Tests of the session (VS1-14 part 1; spec `specs/D05.3-session/spec.md` §3.4, §5): the
//! statement path over a mock store and TSO, the fallback checkpoint (VS1 plan §4.6), the
//! result dropped half-way, the kill signal of the coprocessor client.
//!
//! The planner is still a boundary stub (`Compile` returns `blocked-by:D09.13`), so a
//! statement that reaches the executors is driven step by step as `ExecuteStmt` does, with
//! the plan of `tidb_executor::testutil` (Go's `testutil.MockPhysicalPlan`).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering::SeqCst;
use std::sync::atomic::{AtomicU64, AtomicUsize};

use futures::FutureExt;
use parking_lot::Mutex;
use tidb_base::Error;
use tidb_base::blocked::blocked_by;
use tidb_executor::adapter::ExecStmt;
use tidb_executor::glue::SessionContext;
use tidb_executor::select::reset_context_of_stmt;
use tidb_executor::testutil::{mock_plan, mock_plan_of_kind};
use tidb_kv::Context;
use tidb_kv::key::Key;
use tidb_kv::kv::{
    BatchGetOption, BatchGetter, Client, GetOption, Getter, Iterator as KvIterator, KvFuture,
    OptionValue, Retriever, Snapshot, Storage, Transaction, TxnOption, ValueEntry,
};
use tidb_kv::version::Version;
use tidb_parser::ast;
use tidb_sessionctx::variable::session::GlobalVarAccessor;
use tidb_sessiontxn::sctx::{Oracle, TsFuture};

use crate::session::run_stmt;
use crate::{Dialect, Session};

/// The executors' tasks on the session's ExecPool (#363).
mod exec_pool;

// ---------------------------------------------------------------- mocks

struct Defaults;

impl GlobalVarAccessor for Defaults {
    fn get_global_sys_var(&self, name: &str) -> Result<String, Error> {
        Ok(tidb_sessionctx::variable::get_sys_var(name)
            .map(|v| v.value.clone())
            .unwrap_or_default())
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

/// The rows of the mock store (key → value), shared by its snapshots.
type MockRows = Arc<Mutex<HashMap<Vec<u8>, Vec<u8>>>>;

/// A snapshot of the mock store: it has the rows a test put there (none by default).
struct MockSnapshot(MockRows);

impl Getter for MockSnapshot {
    fn get<'a>(
        &'a self,
        _: &'a Context,
        k: &'a Key,
        _: &'a [GetOption],
    ) -> KvFuture<'a, ValueEntry> {
        let v = self.0.lock().get(&k.0).cloned();
        Box::pin(async move {
            match v {
                Some(v) => Ok(tidb_kv::kv::new_value_entry(v, 0)),
                None => Err(tidb_kv::error::ErrNotExist.clone()),
            }
        })
    }
}
impl Retriever for MockSnapshot {
    fn iter<'a: 'f, 'f>(
        &'a self,
        _: &'f Key,
        _: &'f Key,
    ) -> KvFuture<'f, Box<dyn KvIterator + 'a>> {
        unimplemented!()
    }
    fn iter_reverse<'a: 'f, 'f>(
        &'a self,
        _: &'f Key,
        _: &'f Key,
    ) -> KvFuture<'f, Box<dyn KvIterator + 'a>> {
        unimplemented!()
    }
}
impl BatchGetter for MockSnapshot {
    fn batch_get<'a>(
        &'a self,
        _: &'a Context,
        _: &'a [Key],
        _: &'a [BatchGetOption],
    ) -> KvFuture<'a, HashMap<Vec<u8>, ValueEntry>> {
        unimplemented!()
    }
}
impl Snapshot for MockSnapshot {
    fn set_option(&self, _: isize, _: OptionValue) {}
}

/// A read-only transaction.
struct MockTxn {
    start_ts: u64,
    log: Arc<Mutex<Vec<String>>>,
    rows: MockRows,
}

impl Getter for MockTxn {
    fn get<'a>(
        &'a self,
        _: &'a Context,
        _: &'a Key,
        _: &'a [GetOption],
    ) -> KvFuture<'a, ValueEntry> {
        unimplemented!()
    }
}
impl Retriever for MockTxn {
    fn iter<'a: 'f, 'f>(
        &'a self,
        _: &'f Key,
        _: &'f Key,
    ) -> KvFuture<'f, Box<dyn KvIterator + 'a>> {
        unimplemented!()
    }
    fn iter_reverse<'a: 'f, 'f>(
        &'a self,
        _: &'f Key,
        _: &'f Key,
    ) -> KvFuture<'f, Box<dyn KvIterator + 'a>> {
        unimplemented!()
    }
}
impl BatchGetter for MockTxn {
    fn batch_get<'a>(
        &'a self,
        _: &'a Context,
        _: &'a [Key],
        _: &'a [BatchGetOption],
    ) -> KvFuture<'a, HashMap<Vec<u8>, ValueEntry>> {
        unimplemented!()
    }
}
impl Transaction for MockTxn {
    fn size(&self) -> isize {
        0
    }
    fn len(&self) -> isize {
        0
    }
    fn commit<'a>(&'a mut self, _: &'a Context) -> KvFuture<'a, ()> {
        self.log.lock().push(format!("commit {}", self.start_ts));
        Box::pin(async { Ok(()) })
    }
    fn rollback(&mut self) -> Result<(), Error> {
        self.log.lock().push(format!("rollback {}", self.start_ts));
        Ok(())
    }
    fn string(&self) -> String {
        format!("txn {}", self.start_ts)
    }
    fn set_option(&mut self, _: isize, _: OptionValue) {}
    fn get_option(&self, _: isize) -> Option<&(dyn std::any::Any + Send + Sync)> {
        None
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn start_ts(&self) -> u64 {
        self.start_ts
    }
    fn valid(&self) -> bool {
        true
    }
    fn get_snapshot(&self) -> Arc<dyn Snapshot> {
        Arc::new(MockSnapshot(Arc::clone(&self.rows)))
    }
    fn is_pessimistic(&self) -> bool {
        false
    }
}

/// A store whose TSO counts requests and waits; each request gets `step` more than the last
/// (`step == 0`: always 100).
#[derive(Default)]
struct MockStore {
    next: AtomicU64,
    step: u64,
    requests: AtomicUsize,
    waits: Arc<AtomicUsize>,
    log: Arc<Mutex<Vec<String>>>,
    rows: MockRows,
}

impl MockStore {
    fn new(step: u64) -> Arc<MockStore> {
        Arc::new(MockStore {
            step,
            ..MockStore::default()
        })
    }
}

impl Oracle for MockStore {
    fn get_timestamp_async(&self, _: &str) -> TsFuture {
        self.requests.fetch_add(1, SeqCst);
        let ts = if self.step == 0 {
            100
        } else {
            self.next.fetch_add(self.step, SeqCst) + self.step
        };
        let waits = Arc::clone(&self.waits);
        async move {
            waits.fetch_add(1, SeqCst);
            Ok(ts)
        }
        .boxed()
    }
    fn get_low_resolution_timestamp_async(&self, s: &str) -> TsFuture {
        self.get_timestamp_async(s)
    }
}

impl Storage for MockStore {
    fn begin<'a>(&'a self, opts: &'a [TxnOption]) -> KvFuture<'a, Box<dyn Transaction>> {
        let start_ts = opts
            .iter()
            .find_map(|o| match o {
                TxnOption::StartTS(ts) => Some(*ts),
                _ => None,
            })
            .expect("a start ts");
        self.log.lock().push(format!("begin {start_ts}"));
        let log = Arc::clone(&self.log);
        let rows = Arc::clone(&self.rows);
        Box::pin(async move {
            Ok(Box::new(MockTxn {
                start_ts,
                log,
                rows,
            }) as Box<dyn Transaction>)
        })
    }
    fn get_snapshot(&self, _: Version) -> Arc<dyn Snapshot> {
        Arc::new(MockSnapshot(Arc::clone(&self.rows)))
    }
    fn get_client(&self) -> Arc<dyn Client> {
        Arc::new(MockClient(Arc::clone(&self.rows)))
    }
    fn close(&self) -> Result<(), Error> {
        Ok(())
    }
    fn uuid(&self) -> String {
        "mock".into()
    }
    fn current_version<'a>(&'a self, _: &'a str) -> KvFuture<'a, Version> {
        unimplemented!()
    }
    fn name(&self) -> String {
        "mock".into()
    }
    fn describe(&self) -> String {
        "mock".into()
    }
    fn get_min_safe_ts(&self, _: &str) -> u64 {
        0
    }
}

fn session(store: &Arc<MockStore>) -> Session {
    let mut s = Session::with_store(
        Dialect::MySQL,
        Box::new(Defaults),
        Arc::clone(store) as Arc<dyn Storage>,
        Arc::clone(store) as Arc<dyn Oracle>,
    );
    s.core.sctx.set_connection_id(1);
    s
}

fn parse(sql: &str) -> ast::Stmt {
    tidb_parser::parse(sql, &Default::default())
        .unwrap()
        .stmts
        .remove(0)
}

fn block_on<T>(f: impl std::future::Future<Output = T>) -> T {
    futures::executor::block_on(f)
}

/// `ExecuteStmt` up to and including `Compile` for a plan returning `rows` rows: what Go's
/// `ExecuteStmt` does before `runStmt`, with the planner's part (`AdviseWarmup`,
/// `AdviseOptimizeWithPlan`) done on the mock plan.
fn compile(s: &mut Session, sql: &str, rows: i64) -> ExecStmt {
    compile_counted(s, sql, rows).0
}

/// [`compile`], with the counters of the plan's executors.
fn compile_counted(
    s: &mut Session,
    sql: &str,
    rows: i64,
) -> (ExecStmt, Arc<tidb_executor::testutil::MockCounters>) {
    block_on(s.finish_unfinished_stmt());
    block_on(s.core.prepare_txn_ctx()).unwrap();
    let node = parse(sql);
    reset_context_of_stmt(&mut s.core, &node, sql).unwrap();
    s.core
        .txn_manager
        .on_stmt_start(&mut s.core.sctx, Some(&node))
        .unwrap();
    let (plan, counters) = mock_plan(rows);
    s.core
        .advise_optimize_with_plan_and_then_warm_up(&plan)
        .unwrap();
    let mut stmt = ExecStmt::new(sql.to_owned(), Some(plan), None);
    let node = Arc::new(node);
    s.core.txn_manager.set_current_stmt(Some(Arc::clone(&node)));
    stmt.stmt_node = Some(node);
    stmt.output_names = vec![Arc::new(tidb_types::field_name::FieldName {
        col_name: tidb_parser::model::CIStr::new("a"),
        ..Default::default()
    })];
    (stmt, counters)
}

/// Reads every row of a result.
fn drain(s: &mut Session, rs: &mut crate::ExecStmtResult) -> Vec<i64> {
    let mut out = Vec::new();
    let mut chk = rs.new_chunk().unwrap();
    loop {
        block_on(rs.next(s, &mut chk)).unwrap();
        if chk.num_rows() == 0 {
            return out;
        }
        out.push(chk.get_row(0).get_int64(0));
    }
}

/// The lines of two snapshots that differ (for assertion messages).
fn diff(a: &str, b: &str) -> String {
    let (la, lb): (Vec<&str>, Vec<&str>) = (a.lines().collect(), b.lines().collect());
    let mut out = String::new();
    for i in 0..la.len().max(lb.len()) {
        let (x, y) = (la.get(i).copied(), lb.get(i).copied());
        if x != y {
            let (x, y) = (x.unwrap_or(""), y.unwrap_or(""));
            let at = x.bytes().zip(y.bytes()).take_while(|(p, q)| p == q).count();
            let from = at.saturating_sub(60);
            out.push_str(&format!(
                "- ...{:?}\n+ ...{:?}\n",
                x.get(from..(at + 120).min(x.len())).unwrap_or(x),
                y.get(from..(at + 120).min(y.len())).unwrap_or(y)
            ));
        }
    }
    out
}

fn blocked(e: &Error) -> Option<&str> {
    blocked_by(e)
}

// ---------------------------------------------------------------- statement path

/// An autocommit read: `PrepareTxnCtx` enters a lazy optimistic transaction (no TSO), the
/// warm-up before planning issues the TSO (prefetch), the first read waits for it and
/// begins the transaction at that ts. After the statement the session is idle again: the
/// transaction is gone, no provider, not in a transaction.
#[test]
fn autocommit_read_prefetches_tso_and_ends_idle() {
    let store = MockStore::new(10);
    let mut s = session(&store);
    let stmt = compile(&mut s, "SELECT a FROM t", 2);
    assert_eq!(store.requests.load(SeqCst), 1, "TSO issued at warm-up");
    assert_eq!(store.waits.load(SeqCst), 0, "not waited yet");
    assert!(s.core.sctx.lazy_txn().pending());
    assert!(!s.in_txn());

    // The executors read at the statement's ts (Go's builder asks GetStmtReadTS).
    let ts = block_on(s.core.get_stmt_read_ts()).unwrap();
    assert_eq!(ts, 10);
    assert_eq!(store.waits.load(SeqCst), 1);
    assert!(s.core.sctx.lazy_txn().valid());
    assert_eq!(s.vars().state.txn_ctx.start_ts, 10);
    assert_eq!(store.log.lock().as_slice(), ["begin 10"]);

    let mut rs = block_on(run_stmt(&mut s, stmt)).unwrap().expect("rows");
    assert!(s.has_unfinished_stmt());
    assert_eq!(drain(&mut s, &mut rs), vec![0, 1]);
    assert_eq!(s.vars().last_found_rows, 2);
    // `finishStmt` asks `IsReadOnly` (a SELECT is read only) and the autocommit
    // transaction is committed.
    block_on(rs.close(&mut s)).unwrap();
    assert!(s.is_idle(), "{}", s.state_snapshot());
    assert!(!s.in_txn());
    assert_eq!(s.vars().last_query_info.start_ts, 10);
    assert_eq!(store.log.lock().as_slice(), ["begin 10"]);
    assert_eq!(store.requests.load(SeqCst), 1);
}

/// VS1 plan §4.3: the executed-statement count (the router's "an executor opened" check)
/// moves only after the executor was built and opened. A builder that stops at a boundary
/// stub (here a plan kind without an executor yet, `blocked-by:D12.0`; the cached-table,
/// locking-read and txn-scope stubs of the builder are the same path) is a normal fallback.
#[test]
fn executed_stmts_counts_only_opened_executors() {
    let store = MockStore::new(10);
    let mut s = session(&store);
    assert_eq!(s.executed_stmts(), 0);
    let mut stmt = compile(&mut s, "SELECT a FROM t", 0);
    stmt.plan = Some(mock_plan_of_kind(0, tidb_planner::PhysicalKind::NominalSort).0);
    let Err(e) = block_on(run_stmt(&mut s, stmt)) else {
        panic!("a plan kind without an executor is a boundary stub");
    };
    assert_eq!(blocked(&e), Some("D12.0"));
    assert_eq!(
        s.executed_stmts(),
        0,
        "a builder's Blocked is not an opened executor"
    );

    let stmt = compile(&mut s, "SELECT a FROM t", 1);
    let rs = block_on(run_stmt(&mut s, stmt)).unwrap().expect("rows");
    assert_eq!(s.executed_stmts(), 1);
    let _ = block_on(rs.close(&mut s));
}

/// Statements that read no table run on the basic executors (D12.2, VS1-35) through the
/// session: Projection over TableDual, with LIMIT 0 and a WHERE folded to false (an empty
/// TableDual). Each opened an executor, returns its rows and leaves the session idle.
#[test]
fn statements_without_a_table_run_on_the_basic_executors() {
    let cases: [(&str, Vec<i64>); 3] = [
        ("SELECT 1", vec![1]),
        ("SELECT 1 LIMIT 0", vec![]),
        ("SELECT 1 FROM DUAL WHERE 1 = 0", vec![]),
    ];
    for (sql, rows) in cases {
        let store = MockStore::new(10);
        let mut s = session(&store);
        let mut rs = block_on(s.execute_stmt(parse(sql), sql))
            .unwrap_or_else(|e| panic!("{sql}: {e}"))
            .unwrap_or_else(|| panic!("{sql}: no record set"));
        assert_eq!(s.executed_stmts(), 1, "{sql}");
        assert_eq!(drain(&mut s, &mut rs), rows, "{sql}");
        block_on(rs.close(&mut s)).unwrap();
        assert!(s.is_idle(), "{sql}: {}", s.state_snapshot());
    }
}

/// `autoCommitAfterStmt` without an error commits the autocommit transaction (`CommitTxn`):
/// the transaction ends, the manager's provider is cleared, `TxnCtx` is cleaned up.
#[test]
fn auto_commit_after_stmt_commits() {
    let store = MockStore::new(10);
    let mut s = session(&store);
    let stmt = compile(&mut s, "SELECT a FROM t", 0);
    block_on(s.core.get_stmt_read_ts()).unwrap();
    block_on(crate::tidb::auto_commit_after_stmt(
        &mut s.core,
        None,
        &stmt,
    ))
    .unwrap();
    assert!(s.is_idle());
    // A read-only transaction commits without a commit request (Go doCommit: IsReadOnly).
    assert_eq!(store.log.lock().as_slice(), ["begin 10"]);

    // With an error: rolled back, the error returned.
    let stmt = compile(&mut s, "SELECT a FROM t", 0);
    block_on(s.core.get_stmt_read_ts()).unwrap();
    let e = block_on(crate::tidb::auto_commit_after_stmt(
        &mut s.core,
        Some(Error::plain("boom")),
        &stmt,
    ))
    .unwrap_err();
    assert_eq!(e.to_string(), "boom");
    assert!(s.is_idle());
    assert_eq!(
        store.log.lock().as_slice(),
        ["begin 10", "begin 20", "rollback 20"]
    );
}

/// A statement that never needed its prefetched ts: `finishStmt` drops the pending future
/// (Go: "Reset txn state to invalid to dispose the pending start ts"), no transaction begins.
#[test]
fn unused_prefetched_ts_is_disposed() {
    let store = MockStore::new(10);
    let mut s = session(&store);
    let stmt = compile(&mut s, "SELECT 1", 0);
    assert!(s.core.sctx.lazy_txn().pending());
    let _ = block_on(crate::tidb::finish_stmt(&mut s.core, None, &stmt));
    assert!(s.is_idle());
    assert_eq!(store.waits.load(SeqCst), 0);
    assert!(store.log.lock().is_empty());
}

/// `Compile` fails (the preprocessor's "No database selected"): `rollbackOnError` ends the lazy
/// transaction, so the next statement starts a new one; no TSO was requested.
#[test]
fn compile_error_rolls_back() {
    let store = MockStore::new(10);
    let mut s = session(&store);
    let e = block_on(s.execute_stmt(parse("SELECT a FROM t"), "SELECT a FROM t"))
        .err()
        .unwrap();
    // The statement is handed over to Compile (spec §9.2); without a database the
    // preprocessor answers Go's 1046.
    assert_eq!(e.code(), 1046, "{e}");
    assert!(s.is_idle(), "{}", s.state_snapshot());
    assert_eq!(store.requests.load(SeqCst), 0);
    // ResetContextOfStmt ran: the new statement context is a SELECT's.
    assert!(s.vars().stmt_ctx().in_select_stmt);
    assert_eq!(s.vars().stmt_ctx().original_sql, "SELECT a FROM t");
}

/// A result dropped after its first row (KILL, a disconnect, a cancelled future): the next
/// statement first finishes it (`finishStmt`, `CloseRecordSet`), so the session is idle and
/// the last query info is written before the next statement starts (#222, spec §2.3).
#[test]
fn dropped_result_is_finished_before_the_next_statement() {
    let store = MockStore::new(10);
    let mut s = session(&store);
    let stmt = compile(&mut s, "SELECT a FROM t", 5);
    block_on(s.core.get_stmt_read_ts()).unwrap();
    let mut rs = block_on(run_stmt(&mut s, stmt)).unwrap().expect("rows");
    let mut chk = rs.new_chunk().unwrap();
    block_on(rs.next(&mut s, &mut chk)).unwrap();
    drop(rs);
    assert!(s.has_unfinished_stmt());
    assert!(
        s.core.sctx.lazy_txn().valid(),
        "the transaction is still open"
    );

    // The next statement (it fails at Compile) finishes the dropped one first: the dropped
    // SELECT is read only, so its autocommit transaction is committed.
    let e = block_on(s.execute_stmt(parse("SELECT b FROM t"), "SELECT b FROM t"))
        .err()
        .unwrap();
    assert_eq!(e.code(), 1046, "{e}");
    assert!(!s.has_unfinished_stmt());
    assert!(s.is_idle());
    assert_eq!(s.vars().last_query_info.start_ts, 10);
    assert_eq!(store.log.lock().as_slice(), ["begin 10"]);

    // Same with an explicit call, as the router does before a checkpoint.
    let stmt = compile(&mut s, "SELECT a FROM t", 5);
    block_on(s.core.get_stmt_read_ts()).unwrap();
    let rs = block_on(run_stmt(&mut s, stmt)).unwrap().expect("rows");
    drop(rs);
    block_on(s.finish_unfinished_stmt());
    assert!(s.is_idle());
}

/// Lucky (#235): a result is bound to its statement. Keep result A, execute B (which
/// finishes A first, as `ExecuteStmt` does), then use A: `next` and `fields` fail, `close`
/// does nothing, and B's own result still reads and closes its statement.
#[test]
fn an_old_result_does_not_touch_a_newer_statement() {
    let store = MockStore::new(10);
    let mut s = session(&store);
    let (stmt, a_counters) = compile_counted(&mut s, "SELECT a FROM t", 3);
    block_on(s.core.get_stmt_read_ts()).unwrap();
    let mut a = block_on(run_stmt(&mut s, stmt)).unwrap().expect("rows");
    let mut chk = a.new_chunk().unwrap();
    block_on(a.next(&mut s, &mut chk)).unwrap();
    assert_eq!(chk.num_rows(), 1);

    let stmt = compile(&mut s, "SELECT b FROM t", 2);
    block_on(s.core.get_stmt_read_ts()).unwrap();
    let mut b = block_on(run_stmt(&mut s, stmt)).unwrap().expect("rows");
    // A was kept, not dropped: B's start finished it with its record set, executors closed.
    assert_eq!(a_counters.closed.load(SeqCst), 1);

    let e = block_on(a.next(&mut s, &mut chk)).unwrap_err();
    assert_eq!(e.to_string(), "the record set is closed");
    assert!(a.fields(&s).is_err());
    block_on(a.close(&mut s)).unwrap();
    assert!(s.has_unfinished_stmt(), "A's close left B running");

    assert_eq!(b.fields(&s).unwrap().len(), 1);
    assert_eq!(drain(&mut s, &mut b), vec![0, 1]);
    let _ = block_on(b.close(&mut s));
    assert!(!s.has_unfinished_stmt());
    assert!(s.is_idle());
}

/// KILL reaches the coprocessor client: its variables share the session's kill signal
/// (Go `tikv.Variables.Killed = &SQLKiller.Signal`), and the next statement resets it.
#[test]
fn kill_signal_reaches_the_coprocessor_variables() {
    let store = MockStore::new(10);
    let mut s = session(&store);
    let vars = s.sctx().copr_vars();
    let killed = vars.killed.expect("the session's kill signal");
    assert_eq!(killed.load(SeqCst), 0);
    s.sql_killer()
        .send_kill_signal(tidb_memory::sqlkiller::QueryInterrupted);
    assert_ne!(killed.load(SeqCst), 0);
    assert_eq!(vars.backoff_weight, 2);
    // ResetContextOfStmt resets the killer for the next statement.
    let _ = block_on(s.execute_stmt(parse("SELECT 1"), "SELECT 1"));
    assert_eq!(killed.load(SeqCst), 0);
}

/// `ResetContextOfStmt`: the warning and error counts of the previous statement become
/// `@@warning_count` / `@@error_count`; a SELECT's `ROW_COUNT()` is -1; SHOW WARNINGS keeps
/// the previous warnings; `SQL_NO_CACHE` / `HIGH_PRIORITY`.
#[test]
fn reset_context_of_stmt_counts_and_flags() {
    let store = MockStore::new(10);
    let mut s = session(&store);
    let sc = s.vars().stmt_ctx_arc();
    sc.append_warning(Error::plain("w1"));
    sc.append_warning(Error::plain("w2"));
    sc.append_error(Error::plain("e1"));
    drop(sc);
    let node = parse("SELECT SQL_NO_CACHE HIGH_PRIORITY 1");
    reset_context_of_stmt(&mut s.core, &node, "x").unwrap();
    assert_eq!(s.vars().sys_warning_count, 3);
    assert_eq!(s.vars().sys_error_count, 1);
    let sc = s.vars().stmt_ctx();
    assert!(sc.in_select_stmt && sc.not_fill_cache);
    assert_eq!(sc.priority, tidb_base::mysql::HighPriority);
    assert_eq!(sc.prev_affected_rows, 0);
    sc.append_warning(Error::plain("w3"));

    let node = parse("SHOW WARNINGS");
    reset_context_of_stmt(&mut s.core, &node, "SHOW WARNINGS").unwrap();
    assert_eq!(s.vars().stmt_ctx().prev_affected_rows, -1);
    assert!(s.vars().stmt_ctx().in_show_warning);
    // WarningCount is 0 inside SHOW WARNINGS (Go); the warnings themselves are kept.
    assert_eq!(s.vars().stmt_ctx().warning_count(), 0);
    assert_eq!(s.vars().stmt_ctx().get_warnings().len(), 1);
    assert_eq!(s.vars().sys_warning_count, 1);

    // A statement outside the Rust AST is a boundary stub.
    let node = unported();
    let e = reset_context_of_stmt(&mut s.core, &node, "INSERT").unwrap_err();
    assert_eq!(blocked(&e), Some("D02.2"));
}

/// The transaction manager: the provider of each mode, Go's errors without a provider and
/// for an unknown mode.
#[test]
fn txn_manager_providers_and_errors() {
    let store = MockStore::new(10);
    let mut s = session(&store);
    let e = s
        .core
        .txn_manager
        .on_stmt_start(&mut s.core.sctx, None)
        .unwrap_err();
    assert_eq!(e.to_string(), "context provider not set");
    let e = block_on(s.core.get_stmt_read_ts()).unwrap_err();
    assert_eq!(e.to_string(), "context provider not set");
    assert_eq!(s.core.get_txn_scope(), "global");

    let req = |mode: &str| tidb_sessiontxn::EnterNewTxnRequest {
        tp: tidb_sessiontxn::EnterNewTxnType::EnterNewTxnBeforeStmt,
        provider: None,
        txn_mode: mode.to_owned(),
        causal_consistency_only: false,
        stale_read_ts: 0,
    };
    let e = block_on(
        s.core
            .txn_manager
            .enter_new_txn(&mut s.core.sctx, req("BAD")),
    )
    .unwrap_err();
    assert_eq!(e.to_string(), "Invalid txn mode 'BAD'");

    s.vars_mut()
        .set_system_var("tidb_skip_isolation_level_check", "1")
        .unwrap();
    for (isolation, expected) in [
        ("READ-COMMITTED", "READ-COMMITTED"),
        ("SERIALIZABLE", "SERIALIZABLE"),
        ("REPEATABLE-READ", "REPEATABLE-READ"),
    ] {
        s.vars_mut()
            .set_system_var("transaction_isolation", isolation)
            .unwrap();
        block_on(
            s.core
                .txn_manager
                .enter_new_txn(&mut s.core.sctx, req("PESSIMISTIC")),
        )
        .unwrap();
        assert_eq!(s.vars().state.txn_ctx.isolation, expected);
        assert!(s.vars().state.txn_ctx.is_pessimistic);
        s.core.rollback_txn();
    }
    block_on(
        s.core
            .txn_manager
            .enter_new_txn(&mut s.core.sctx, req("OPTIMISTIC")),
    )
    .unwrap();
    assert!(!s.vars().state.txn_ctx.is_pessimistic);
    assert!(!s.core.txn_manager.is_idle());
    s.core.rollback_txn();
    assert!(s.core.txn_manager.is_idle());
}

/// `LazyTxn`: invalid → pending → valid, `String`, `GetOption(TxnScope)` without a
/// transaction.
#[test]
fn lazy_txn_states() {
    let store = MockStore::new(10);
    let mut s = session(&store);
    let txn = s.core.sctx.lazy_txn();
    assert_eq!(txn.string(), "invalid transaction");
    let scope = txn.get_option(tidb_kv::option::TxnScope).unwrap();
    assert_eq!(scope.downcast_ref::<String>().map(String::as_str), Some(""));
    assert!(txn.get_option(tidb_kv::option::Pessimistic).is_none());
    s.core
        .sctx
        .prepare_ts_future(tidb_sessiontxn::sctx::TxnTsFuture::Constant(42), "global")
        .unwrap();
    assert_eq!(s.core.sctx.lazy_txn().string(), "txnFuture");
    assert!(s.core.sctx.get_prepared_txn_future());
    let mut vars = std::mem::replace(
        &mut s.core.sctx.vars,
        tidb_sessionctx::variable::session::SessionVars::new(Box::new(Defaults)),
    );
    block_on(s.core.sctx.txn.wait(&mut vars)).unwrap();
    s.core.sctx.vars = vars;
    assert_eq!(s.core.sctx.lazy_txn().string(), "txn 42");
    let e = s
        .core
        .sctx
        .prepare_ts_future(tidb_sessiontxn::sctx::TxnTsFuture::Constant(43), "global")
        .unwrap_err();
    assert_eq!(e.to_string(), "cannot prepare ts future when txn is valid");
    s.core.rollback_txn();
    let e = block_on(s.core.sctx.txn.wait(&mut s.core.sctx.vars)).unwrap_err();
    assert_eq!(e.code(), tidb_kv::error::ErrInvalidTxn.code());
}

// ---------------------------------------------------------------- §4.6 checkpoint

/// Each place a statement can fall back (a boundary stub before `Compile`, in `Compile`,
/// after the warm-up issued a TSO): restoring the checkpoint gives back the session exactly
/// (the serialized snapshot), and the session is idle again.
#[test]
fn every_fallback_point_restores_the_session() {
    type Step = fn(&mut Session) -> &'static str;
    let points: Vec<(&str, Step)> = vec![
        ("before Compile (ResetContextOfStmt)", |s| {
            // A statement outside the Rust AST is blocked in ResetContextOfStmt, after the
            // transaction was entered.
            let e = block_on(s.execute_stmt(unported(), "INSERT INTO t VALUES (1)"))
                .err()
                .unwrap();
            assert_eq!(blocked(&e), Some("D02.2"));
            "D02.2"
        }),
        ("after Compile (planner boundary stub)", |s| {
            // `SELECT c FROM sbtest1 WHERE id BETWEEN 1 AND 10` now runs end-to-end on the
            // new stack (D12.1 Part 2A TableReader); use an aggregate query that stops in
            // logical optimization (D09.13 BuildKeySolver) before any executor opens.
            let sql = "SELECT COUNT(*) FROM sbtest1 WHERE id BETWEEN 1 AND 10";
            let e = block_on(s.execute_stmt(parse(sql), sql)).err().unwrap();
            assert_eq!(blocked(&e), Some("D09.13"), "{e:?}");
            "D09.13"
        }),
        ("after the warm-up (TSO pending)", |s| {
            let _ = compile(s, "SELECT a FROM t", 1);
            assert!(s.core.sctx.lazy_txn().pending());
            "warm-up"
        }),
        ("after activation (transaction begun)", |s| {
            let _ = compile(s, "SELECT a FROM t", 1);
            block_on(s.core.get_stmt_read_ts()).unwrap();
            assert!(s.core.sctx.lazy_txn().valid());
            "activated"
        }),
    ];
    for (name, step) in points {
        let store = MockStore::new(10);
        let mut s = if name.starts_with("after Compile") {
            sbtest_session(&store)
        } else {
            session(&store)
        };
        // Some state left by earlier statements.
        s.vars_mut().last_found_rows = 3;
        s.vars().stmt_ctx().append_warning(Error::plain("earlier"));
        let before = s.state_snapshot();
        let cp = s.checkpoint();
        let _ = step(&mut s);
        assert_ne!(
            s.state_snapshot(),
            before,
            "{name}: the step changed nothing"
        );
        s.restore(cp).unwrap();
        assert_eq!(s.state_snapshot(), before, "{name}");
        assert!(s.is_idle(), "{name}");
        assert_eq!(s.vars().stmt_ctx().warning_count(), 1, "{name}");
        assert_eq!(
            store.waits.load(SeqCst),
            usize::from(name.starts_with("after activation")),
            "{name}"
        );
    }
}

/// A statement outside the Rust AST (`Stmt::Unported`).
fn unported() -> ast::Stmt {
    let node = parse("INSERT INTO t VALUES (1)");
    assert!(matches!(node, ast::Stmt::Unported(_)));
    node
}

/// Lucky's requirement (#205): after a fallback the session equals a fresh session, item by
/// item: transaction, autocommit, snapshot, previous-statement state.
#[test]
fn fallback_leaves_a_fresh_session() {
    let store = MockStore::new(10);
    let mut s = session(&store);
    let mut fresh = session(&MockStore::new(10));
    // The session's random seed differs per session (rand_seed1 / rand_seed2).
    fresh.vars_mut().state.rng = s.vars().state.rng.clone();
    let cp = s.checkpoint();
    let _ = compile(&mut s, "SELECT a FROM t", 1);
    block_on(s.core.get_stmt_read_ts()).unwrap();
    s.restore(cp).unwrap();

    assert!(
        s.state_snapshot() == fresh.state_snapshot(),
        "{}",
        diff(&s.state_snapshot(), &fresh.state_snapshot())
    );
    let (v, f) = (s.vars(), fresh.vars());
    // Transaction.
    assert!(!s.in_txn());
    assert_eq!(v.state.txn_ctx, f.state.txn_ctx);
    assert!(!s.core.sctx.lazy_txn().valid_or_pending());
    assert!(s.core.txn_manager.is_idle());
    assert!(!s.has_unfinished_stmt());
    // Autocommit and the server status.
    assert!(s.is_autocommit());
    assert_eq!(v.status(), f.status());
    // Snapshot reads.
    assert_eq!(v.snapshot_ts, 0);
    assert_eq!(v.state.txn_read_ts, f.state.txn_read_ts);
    // The previous statement.
    assert_eq!(v.sys_warning_count, f.sys_warning_count);
    assert_eq!(v.sys_error_count, f.sys_error_count);
    assert_eq!(
        v.stmt_ctx().prev_affected_rows,
        f.stmt_ctx().prev_affected_rows
    );
    assert_eq!(v.last_found_rows, f.last_found_rows);
    assert_eq!(v.last_query_info, f.last_query_info);
    assert_eq!(v.found_in_plan_cache, f.found_in_plan_cache);
    assert_eq!(v.prev_found_in_plan_cache, f.prev_found_in_plan_cache);
    assert_eq!(v.stmt_ctx().warning_count(), 0);
}

/// A checkpoint of a session that is not idle cannot be restored (the router reports a bug).
#[test]
fn checkpoint_of_a_busy_session_is_refused() {
    let store = MockStore::new(10);
    let mut s = session(&store);
    let _ = compile(&mut s, "SELECT a FROM t", 1);
    let cp = s.checkpoint();
    assert!(s.restore(cp).is_err());
}

/// Random interleavings of statements that run on the new stack, fall back, or run on the
/// PoC (its state is written as the router's sync writes it): every fallback leaves the
/// session as it was, and the session ends equal to one that never saw the fallbacks.
#[test]
fn random_interleaving_of_new_fallback_and_poc() {
    let mut seed: u64 = 0x5eed_1234;
    let mut next = move |n: u64| {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (seed >> 33) % n
    };
    for round in 0..30 {
        let (sa, sb) = (MockStore::new(0), MockStore::new(0));
        let mut a = session(&sa);
        let mut b = session(&sb);
        b.vars_mut().state.rng = a.vars().state.rng.clone();
        for step in 0..12 {
            match next(4) {
                // New stack: a statement that reads its rows and closes.
                0 => {
                    for s in [&mut a, &mut b] {
                        let stmt = compile(s, "SELECT a FROM t", 2);
                        block_on(s.core.get_stmt_read_ts()).unwrap();
                        let mut rs = block_on(run_stmt(s, stmt)).unwrap().unwrap();
                        drain(s, &mut rs);
                        let _ = block_on(rs.close(s));
                    }
                }
                // PoC: the router writes the previous statement's state.
                1 => {
                    let rows = next(100);
                    for s in [&mut a, &mut b] {
                        let vars = s.vars_mut();
                        vars.last_found_rows = rows;
                        vars.set_stmt_ctx(Arc::new(tidb_sessionctx::stmtctx::new_stmt_ctx()));
                        vars.stmt_ctx()
                            .append_warning(Error::plain(format!("poc {rows}")));
                    }
                }
                // A fallback on `a` only, at a random point.
                k => {
                    let before = a.state_snapshot();
                    let cp = a.checkpoint();
                    if k == 2 {
                        let _ =
                            block_on(a.execute_stmt(parse("SELECT a FROM t"), "SELECT a FROM t"));
                    } else {
                        let _ = compile(&mut a, "SELECT a FROM t", 1);
                    }
                    a.restore(cp).unwrap();
                    assert_eq!(a.state_snapshot(), before, "round {round} step {step}");
                }
            }
        }
        // The durations (DurationWaitTS, DurationCompile) are timings.
        let timeless = |s: &Session| {
            s.state_snapshot()
                .lines()
                .filter(|l| !l.starts_with("Duration"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert!(
            timeless(&a) == timeless(&b),
            "round {round}: {}",
            diff(&timeless(&a), &timeless(&b))
        );
    }
}

// ---------------------------------------------------------------- prepared statements

/// Go's deferred `DropPreparedStmt` (spec §6.1): `BEGIN; PREPARE → 1; CLOSE 1; EXECUTE 1`
/// still finds statement 1, and it still counts toward `max_prepared_stmt_count`, until the
/// transaction ends. Each end removes it — commit, rollback, the end of an autocommit
/// statement — but closing the session does not clean (Go: `inCloseSession`; `Close`
/// withdraws every statement instead).
/// The global prepared-statement count and limit are process-wide: the tests that read them
/// run one at a time.
static PREPARED_COUNT: Mutex<()> = Mutex::new(());

#[test]
fn closed_prepared_stmt_lives_until_txn_end() {
    use tidb_sessionctx::variable::session_methods::PreparedStmtCount;
    let _serial = PREPARED_COUNT.lock();
    let store = MockStore::new(10);
    let found = |s: &mut Session| match block_on(s.execute_prepared_stmt(1, vec![])) {
        Ok(_) => true,
        Err(e) => e.code() != 8111,
    };
    let prepare_and_close = |s: &mut Session| {
        // PREPARE (the plan-cache statement is prepared-statement planning, VS1-15).
        s.vars_mut().add_prepared_stmt(1, Arc::new(())).unwrap();
        s.drop_prepared_stmt(1).unwrap();
    };
    for end in ["commit", "rollback", "autocommit statement", "close"] {
        let mut s = session(&store);
        if end != "autocommit statement" {
            s.vars_mut().set_in_txn(true); // BEGIN
        }
        let before = PreparedStmtCount.load(SeqCst);
        prepare_and_close(&mut s);
        assert!(
            found(&mut s),
            "{end}: EXECUTE after CLOSE in the transaction"
        );
        assert_eq!(s.vars().state.prepared_stmts.len(), 1, "{end}");
        assert_eq!(
            PreparedStmtCount.load(SeqCst),
            before + 1,
            "{end}: still counted"
        );
        match end {
            "commit" => block_on(s.core.commit_txn()).unwrap(),
            "rollback" => s.core.rollback_txn(),
            "autocommit statement" => {
                let stmt = compile(&mut s, "SELECT 1", 0);
                let _ = block_on(crate::tidb::finish_stmt(&mut s.core, None, &stmt));
            }
            _ => {
                s.close();
                // Not cleaned: still recorded, and Close withdrew it from the count.
                assert_eq!(s.vars().state.retry_info.dropped_prepared_stmt_ids, [1]);
                assert_eq!(PreparedStmtCount.load(SeqCst), before);
                continue;
            }
        }
        assert!(!found(&mut s), "{end}: EXECUTE after the transaction ended");
        assert!(s.vars().state.prepared_stmts.is_empty(), "{end}");
        assert!(
            s.vars()
                .state
                .retry_info
                .dropped_prepared_stmt_ids
                .is_empty()
        );
        assert_eq!(PreparedStmtCount.load(SeqCst), before, "{end}: uncounted");
        assert_eq!(s.drop_prepared_stmt(1).unwrap_err().code(), 8111);
    }
}

/// A prepared statement on the plan cache's side (`PlanCacheStmt`) with `sql` as its text.
fn plan_cache_stmt(sql: &str) -> Arc<dyn std::any::Any + Send + Sync> {
    let st = tidb_planner::core::plan_cache_utils::PlanCacheStmt {
        stmt_text: sql.to_owned(),
        stmt_db: "test".to_owned(),
        ..Default::default()
    };
    Arc::new(Mutex::new(st))
}

/// Go v8.5.8's `cleanRetryInfo` (D13.1 tasks, possibly an upstream bug, kept as it is): with
/// two statements closed in one transaction, the plan cache key is computed from the first
/// statement both times, so the first statement's plan is deleted twice and the second's is
/// never deleted. Both statements are removed.
#[test]
fn clean_retry_info_deletes_the_first_closed_stmt_plan_each_time() {
    let _serial = PREPARED_COUNT.lock();
    let store = MockStore::new(10);
    let mut s = session(&store);
    s.vars_mut().enable_prepared_plan_cache = true;
    s.vars_mut().set_in_txn(true); // BEGIN
    s.vars_mut()
        .add_prepared_stmt(1, plan_cache_stmt("select 1"))
        .unwrap();
    s.vars_mut()
        .add_prepared_stmt(2, plan_cache_stmt("select 2"))
        .unwrap();
    s.drop_prepared_stmt(1).unwrap();
    s.drop_prepared_stmt(2).unwrap();
    s.core.rollback_txn();
    let key = |sql: &str| {
        let st = tidb_planner::core::plan_cache_utils::PlanCacheStmt {
            stmt_text: sql.to_owned(),
            stmt_db: "test".to_owned(),
            ..Default::default()
        };
        tidb_planner::core::plan_cache_utils::session_plan_cache_key(&st)
            .unwrap()
            .0
    };
    let first = key("select 1");
    assert_ne!(first, key("select 2"));
    assert!(s.vars().state.prepared_stmts.is_empty());
    assert_eq!(
        s.get_session_plan_cache().unwrap().deleted_keys,
        vec![first.clone(), first]
    );

    // tidb_ignore_prepared_cache_close_stmt keeps the plans; the statements still go.
    s.vars_mut().ignore_prepared_cache_close_stmt = true;
    s.vars_mut()
        .add_prepared_stmt(3, plan_cache_stmt("select 3"))
        .unwrap();
    s.drop_prepared_stmt(3).unwrap();
    s.core.rollback_txn();
    assert!(s.vars().state.prepared_stmts.is_empty());
    assert_eq!(s.get_session_plan_cache().unwrap().deleted_keys.len(), 2);
}

/// `PrepareExec` adds the statement with `AddPreparedStmt`, which checks
/// `max_prepared_stmt_count` (1461); a closed statement still counts until the transaction
/// ends (spec §6.1).
#[test]
fn max_prepared_stmt_count_counts_closed_stmts() {
    use tidb_sessionctx::variable::session_methods::PreparedStmtCount;
    use tidb_sessionctx::variable::tidb_vars::MaxPreparedStmtCountValue;
    let _serial = PREPARED_COUNT.lock();
    let store = MockStore::new(10);
    let mut s = session(&store);
    s.vars_mut().set_in_txn(true); // BEGIN
    let old_max = MaxPreparedStmtCountValue.load(SeqCst);
    MaxPreparedStmtCountValue.store(PreparedStmtCount.load(SeqCst) + 1, SeqCst);
    s.vars_mut()
        .add_prepared_stmt(1, plan_cache_stmt("select 1"))
        .unwrap();
    s.drop_prepared_stmt(1).unwrap();
    let e = s
        .vars_mut()
        .add_prepared_stmt(2, plan_cache_stmt("select 2"))
        .unwrap_err();
    MaxPreparedStmtCountValue.store(old_max, SeqCst);
    assert_eq!(e.code(), 1461, "{e}");
    s.core.rollback_txn();
    assert!(s.vars().state.prepared_stmts.is_empty());
}

/// The real `PrepareStmt` runs the prepare executor: a SELECT without a table is built by
/// `PlanBuilder.Build` in the planning context the session hands the planner (the prepared
/// statement is added with its column definitions), one with a table that the (empty)
/// InfoSchema does not have stops at the privilege check of a missing table (D04.1); two
/// statements are Go's `ErrPrepareMulti`.
#[test]
fn prepare_stmt_runs_the_prepare_executor() {
    let _serial = PREPARED_COUNT.lock();
    let store = MockStore::new(10);
    let mut s = session(&store);
    s.vars_mut().current_db = "test".to_owned();
    let info = block_on(s.prepare_stmt("select ?")).unwrap();
    assert_eq!(info.param_count, 1);
    let names: Vec<&str> = info
        .fields
        .iter()
        .map(|f| f.column_name.o.as_str())
        .collect();
    assert_eq!(names, ["?"]);
    assert_eq!(s.vars().state.prepared_stmts.len(), 1);
    s.drop_prepared_stmt(info.stmt_id).unwrap();
    let e = block_on(s.prepare_stmt("select c from sbtest1 where id = ?")).unwrap_err();
    assert_eq!(blocked_by(&e), Some("D04.1"), "{e}");
    let e = block_on(s.prepare_stmt("select 1; select 2")).unwrap_err();
    assert_eq!(e.code(), 8115, "{e}");
    assert!(s.is_idle());
}

// ---------------------------------------------------------------- metadata lock, MaxTS

/// An InfoSchema (version 1) with `test.sbtest1 (id BIGINT PRIMARY KEY, c CHAR)`, the table
/// resident in the cache (as the router's tests build it).
fn sbtest_info_schema() -> Arc<tidb_infoschema::InfoSchemaV2> {
    sbtest_info_schema_at(1)
}

/// `sbtest_info_schema` at the schema version `version` (as after a DDL).
fn sbtest_info_schema_at(version: i64) -> Arc<tidb_infoschema::InfoSchemaV2> {
    sbtest_info_schema_with(version, None)
}

/// `sbtest_info_schema` with `c CHAR CHARACTER SET utf8mb4 COLLATE utf8mb4_bin`: a condition
/// on `c` needs the column's collation (the ranger builds a field type from it; the plain
/// fixture's `c` has none).
fn sbtest_info_schema_collated() -> Arc<tidb_infoschema::InfoSchemaV2> {
    sbtest_info_schema_with(1, Some(("utf8mb4", "utf8mb4_bin")))
}

/// `sbtest_info_schema_at(version)`, `c` with the charset and collation `c_collation`.
fn sbtest_info_schema_with(
    version: i64,
    c_collation: Option<(&str, &str)>,
) -> Arc<tidb_infoschema::InfoSchemaV2> {
    use tidb_model::{ColumnInfo, DBInfo, StatePublic, TableInfo};
    use tidb_parser::model::CIStr;
    use tidb_parser::types::FieldType;
    let col = |id: i64, name: &str, tp: u8| {
        let mut field_type = FieldType::new(tp);
        if id == 1 {
            // The handle: `id INT PRIMARY KEY` (pk_is_handle).
            field_type.add_flag(tidb_base::mysql::PriKeyFlag | tidb_base::mysql::NotNullFlag);
        } else if let Some((charset, collation)) = c_collation {
            field_type.set_charset(charset);
            field_type.set_collate(collation);
        }
        Arc::new(ColumnInfo {
            id,
            name: CIStr::new(name),
            offset: (id - 1) as isize,
            field_type,
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
    block_on(b.init_with_db_infos(vec![db], vec![], vec![], version)).unwrap();
    Arc::new(block_on(b.build(1)))
}

/// A session over `store` whose domain InfoSchema has `test.sbtest1`, with
/// `tidb_enable_metadata_lock` on (Go's default; read at transaction start).
fn sbtest_session(store: &Arc<MockStore>) -> Session {
    tidb_sessionctx::variable::tidb_vars::EnableMDL.store(true, SeqCst);
    let mut s = session(store);
    s.set_domain_info_schema(sbtest_info_schema());
    s.vars_mut().current_db = "test".to_owned();
    s
}

const SBTEST_POINT: &str = "SELECT c FROM sbtest1 WHERE id=?";

/// The table IDs and versions registered in the session's metadata-lock table.
fn mdl_entries(s: &Session) -> Option<Vec<(i64, i64)>> {
    let m = s.vars().state.txn_ctx.related_table_for_mdl()?;
    let mut v: Vec<_> = m.0.lock().iter().map(|(k, v)| (*k, *v)).collect();
    v.sort();
    Some(v)
}

/// `PREPARE` registers the table (Go: a fresh statement context is not read-only, so the
/// lock branch runs) with the domain's schema version while the prepare executor runs, and
/// `PrepareStmt` clears it at the end (`rollbackOnError` → `RollbackTxn`), also when the
/// statement fails. The statement passes the metadata lock and stops at the next boundary
/// stub (see `prepare_of_the_sbtest_point_select_stops_after_the_lock`).
#[test]
fn prepare_registers_the_table_and_clears_it_at_the_end() {
    let store = MockStore::new(10);
    let mut s = sbtest_session(&store);
    // PrepareStmt's steps up to the prepare executor.
    let core = &mut s.core;
    core.sctx.vars.state.txn_ctx.info_schema = Some(
        tidb_sessionctx::variable::session_state::TxnInfoSchema(core.sctx.get_domain_info_schema()),
    );
    block_on(core.prepare_txn_ctx()).unwrap();
    assert!(core.sctx.vars.state.txn_ctx.enable_mdl);
    let mut exec = tidb_executor::prepared::new_prepare_exec(SBTEST_POINT);
    let res = exec.next(core);
    assert!(!core.sctx.vars.stmt_ctx().is_read_only);
    assert_eq!(mdl_entries(&s), Some(vec![(110, 1)]), "{res:?}");
    assert!(s.vars().stmt_ctx().related_table_ids.contains(&110));
    s.core.rollback_on_error();
    assert_eq!(mdl_entries(&s), None);

    // The whole PrepareStmt: cleared at the end, whether it succeeds or not.
    let _ = block_on(s.prepare_stmt(SBTEST_POINT));
    assert_eq!(mdl_entries(&s), None);
    assert!(s.is_idle());
    // A failing PREPARE (a table that does not exist) leaves nothing either.
    let _ = block_on(s.prepare_stmt("SELECT c FROM nosuch WHERE id=?"));
    assert_eq!(mdl_entries(&s), None);
}

/// Which path an EXECUTE of the session's running statement took (D05.3 spec §10.1).
#[derive(Debug, PartialEq)]
enum ExecutePath {
    /// The point plan short path: `PsStmt` set, `ExecStmt.PointGet`, a bare record set.
    PointGetShortPath,
    /// `runStmt` → `ExecStmt.Exec` → `finishStmt`.
    Normal,
}

/// The path of the running statement, checked against everything each path leaves behind:
/// the short path keeps the Execute plan in the statement context, runs at high priority and
/// is not a prepared statement for the slow log (`isPreparedStmt` false); the normal path
/// replaced both plans with the point get (`inheritContextFromExecuteStmt`).
fn execute_path(s: &Session) -> ExecutePath {
    let run = s.running.as_ref().expect("a running statement");
    let ctx_plan = s.vars().stmt_ctx().get_plan().expect("a plan");
    let ctx_plan_is_execute = ctx_plan
        .downcast_ref::<tidb_planner::core::common_plans::Execute>()
        .is_some();
    let ctx_plan_is_point_get = ctx_plan
        .downcast_ref::<tidb_planner::PhysicalPlanRef>()
        .is_some_and(|p| p.kind() == tidb_planner::PhysicalKind::PointGet);
    assert!(
        run.stmt
            .plan
            .as_ref()
            .is_some_and(|p| p.kind() == tidb_planner::PhysicalKind::PointGet)
    );
    assert!(run.stmt.execute_plan.is_none());
    if run.stmt.ps_stmt.is_some() {
        assert!(!run.wrapped, "a bare record set");
        assert!(!run.stmt.is_prepared(), "Prepared is false (slow log)");
        assert!(
            ctx_plan_is_execute,
            "the statement context keeps the Execute plan"
        );
        assert_eq!(s.vars().stmt_ctx().priority, tidb_base::mysql::HighPriority);
        ExecutePath::PointGetShortPath
    } else {
        assert!(run.wrapped, "an execStmtResult");
        assert!(run.stmt.is_prepared(), "inheritContextFromExecuteStmt");
        assert!(ctx_plan_is_point_get, "inheritContextFromExecuteStmt");
        assert_ne!(s.vars().stmt_ctx().priority, tidb_base::mysql::HighPriority);
        ExecutePath::Normal
    }
}

/// `f` over the session's prepared statement `stmt_id` (`PlanCacheStmt`).
fn prepared_of<R>(
    s: &Session,
    stmt_id: u32,
    f: impl FnOnce(&tidb_planner::core::plan_cache_utils::PlanCacheStmt) -> R,
) -> R {
    let ps = s.vars().get_prepared_stmt_by_id(stmt_id).unwrap();
    let ps = ps
        .downcast_ref::<parking_lot::Mutex<tidb_planner::core::plan_cache_utils::PlanCacheStmt>>()
        .unwrap();
    f(&ps.lock())
}

/// EXECUTE of the prepared point select, read to the end (the mock store has no row).
fn execute_point_select(s: &mut Session, stmt_id: u32) -> ExecutePath {
    let mut rs =
        block_on(s.execute_prepared_stmt(stmt_id, vec![tidb_types::datum::new_int_datum(1)]))
            .unwrap()
            .expect("a result set");
    let path = execute_path(s);
    let mut chk = rs.new_chunk().unwrap();
    block_on(rs.next(s, &mut chk)).unwrap();
    assert_eq!(chk.num_rows(), 0);
    block_on(rs.close(s)).unwrap();
    path
}

/// `PREPARE` + `EXECUTE` of the sysbench point select on the real session over the sbtest
/// InfoSchema (mock store): PREPARE passes the metadata lock and builds the plan (the column
/// `c` of `sbtest1`); EXECUTE plans the point get on the fast path with the parameter's
/// value. The statement is an autocommit read, not stale, at the schema version it was
/// prepared with, so `Compile` takes Go's point plan short path (D05.3 spec §10.1): the
/// session calls `ExecStmt.PointGet`, which builds and opens a point get executor, reads the
/// mock store (no row), and keeps the executor in the prepared statement (MaxTS); the
/// session is idle after the close. The second EXECUTE takes the same path and reuses it.
#[test]
fn prepare_of_the_sbtest_point_select_builds_the_plan() {
    let _serial = PREPARED_COUNT.lock();
    let store = MockStore::new(10);
    let mut s = sbtest_session(&store);
    let info = block_on(s.prepare_stmt(SBTEST_POINT)).unwrap();
    let names: Vec<&str> = info
        .fields
        .iter()
        .map(|f| f.column_name.o.as_str())
        .collect();
    assert_eq!(names, ["c"]);
    assert_eq!(info.fields[0].table_name.o, "sbtest1");
    assert_eq!(mdl_entries(&s), None);
    for round in 1..=2 {
        assert_eq!(
            execute_point_select(&mut s, info.stmt_id),
            ExecutePath::PointGetShortPath
        );
        assert_eq!(params_of(&s), vec!["1".to_owned()]);
        assert_eq!(
            s.executed_stmts(),
            round,
            "the point get executor was opened"
        );
        assert!(s.is_idle(), "{}", s.state_snapshot());
        assert!(
            prepared_of(&s, info.stmt_id, |p| p.point_get.executor.is_some()),
            "the MaxTS point get executor is kept in the prepared statement for the next EXECUTE"
        );
    }
    s.drop_prepared_stmt(info.stmt_id).unwrap();
}

/// Puts the row `(handle, c)` of `table_id` (row format v2, `c` is column 2) into the mock
/// store.
fn put_row(store: &MockStore, table_id: i64, handle: i64, col_id: i64, c: &str) {
    let mut enc = tidb_tablecodec::rowcodec::Encoder::default();
    enc.enable = true;
    let mut d = tidb_types::datum::Datum::default();
    d.set_string(c.as_bytes().to_vec(), "utf8mb4_bin".to_owned());
    let val = tidb_tablecodec::encode_row(None, &[d], &[col_id], None, Some(&mut enc)).unwrap();
    let key = tidb_tablecodec::encode_row_key_with_handle(
        table_id,
        &tidb_kv::Handle::Int(tidb_kv::key::IntHandle(handle)),
    );
    store.rows.lock().insert(key.0, val);
}

/// EXECUTE of a one-parameter prepared statement read to the end: the first column of every
/// row as text, and whether the plan came from the instance plan cache.
fn execute_rows(
    s: &mut Session,
    stmt_id: u32,
    param: tidb_types::datum::Datum,
) -> (Vec<String>, bool) {
    let mut rs = block_on(s.execute_prepared_stmt(stmt_id, vec![param]))
        .unwrap()
        .expect("a result set");
    let found = s.vars().found_in_plan_cache;
    let mut out = Vec::new();
    loop {
        let mut chk = rs.new_chunk().unwrap();
        block_on(rs.next(s, &mut chk)).unwrap();
        if chk.num_rows() == 0 {
            break;
        }
        for i in 0..chk.num_rows() {
            out.push(String::from_utf8_lossy(chk.get_row(i).get_string(0)).into_owned());
        }
    }
    block_on(rs.close(s)).unwrap();
    assert!(s.is_idle(), "{}", s.state_snapshot());
    (out, found)
}

/// Go's point get executor reuse (`ExecStmt.PointGet`, D05.3 spec §10.1 item 3): the first
/// EXECUTE of the autocommit primary-key point get (MaxTS) builds the executor and keeps it
/// in the session's prepared statement; the next ones take it back and `Init` it from the
/// current plan, which is the instance plan cache's shared plan (a hit) rebuilt for the
/// parameter. Every parameter reads its own row (1, 2, a missing id, 1 again: no handle or
/// `done` flag from the previous execution leaks), only one executor is ever built, and the
/// cached plan's structural hash never changes (the executor only reads the plan).
#[test]
fn point_get_executor_is_reused_with_the_current_plan() {
    let _serial = PREPARED_COUNT.lock();
    let store = MockStore::new(10);
    put_row(&store, 110, 1, 2, "one");
    put_row(&store, 110, 2, 2, "two");
    let cache = new_test_cache();
    let mut s = cached_session(&store, sbtest_info_schema(), &cache);
    let int = tidb_types::datum::new_int_datum;
    let p = block_on(s.prepare_stmt(SBTEST_POINT)).unwrap();
    let kept = |s: &Session| prepared_of(s, p.stmt_id, |p| p.point_get.executor.is_some());

    assert_eq!(
        execute_rows(&mut s, p.stmt_id, int(1)),
        (vec!["one".to_owned()], false)
    );
    assert!(kept(&s), "kept after the first EXECUTE");
    let before = cached_hashes(&cache);
    for (param, want) in [
        (2, Some("two")),
        (3, None),
        (1, Some("one")),
        (2, Some("two")),
    ] {
        let want: Vec<String> = want.into_iter().map(str::to_owned).collect();
        assert_eq!(
            execute_rows(&mut s, p.stmt_id, int(param)),
            (want, true),
            "id = {param}"
        );
        assert!(kept(&s), "given back after id = {param}");
    }
    assert_eq!(
        cached_hashes(&cache),
        before,
        "the cached plan is never written"
    );
    assert_eq!(s.executed_stmts(), 5);
    s.drop_prepared_stmt(p.stmt_id).unwrap();
}

/// A point get by a secondary unique index reads at a TSO (not MaxTS), so its executor is
/// neither kept nor reused (Go's `useMaxTS`); every EXECUTE builds a new one, reads its own
/// row through the index, and the cached plan's index values are not written (A3 §2.3 9a).
#[test]
fn unique_index_point_get_is_not_reused_and_reads_its_own_row() {
    let _serial = PREPARED_COUNT.lock();
    let store = MockStore::new(10);
    for (k, h, c) in [(7, 1, "seven"), (8, 2, "eight")] {
        let mut d = tidb_types::datum::Datum::default();
        d.set_int64(k);
        let mut enc = Vec::new();
        tidb_codec::encode_key(&tidb_base::gostd::time::utc(), &mut enc, &[d]).unwrap();
        let handle = tidb_kv::Handle::Int(tidb_kv::key::IntHandle(h));
        store.rows.lock().insert(
            tidb_tablecodec::encode_index_seek_key(111, 1, &enc).0,
            tidb_tablecodec::encode_handle_in_unique_index_value(&handle, false),
        );
        put_row(&store, 111, h, 3, c);
    }
    let cache = new_test_cache();
    let mut s = cached_session(&store, uk_info_schema(), &cache);
    let int = tidb_types::datum::new_int_datum;
    let p = block_on(s.prepare_stmt("SELECT c FROM uk1 WHERE k = ?")).unwrap();
    assert_eq!(
        execute_rows(&mut s, p.stmt_id, int(7)),
        (vec!["seven".to_owned()], false)
    );
    let before = cached_hashes(&cache);
    for (param, want) in [(8, Some("eight")), (9, None), (7, Some("seven"))] {
        let want: Vec<String> = want.into_iter().map(str::to_owned).collect();
        assert_eq!(
            execute_rows(&mut s, p.stmt_id, int(param)),
            (want, true),
            "k = {param}"
        );
        assert!(prepared_of(&s, p.stmt_id, |p| p
            .point_get
            .executor
            .is_none()));
    }
    assert_eq!(cached_hashes(&cache), before);
    s.drop_prepared_stmt(p.stmt_id).unwrap();
}

/// Outside an autocommit transaction (`autocommit = 0`, or after `BEGIN`) the same EXECUTE
/// takes the normal path: `IsSafeToReusePointGetExecutor` answers false, the statement keeps
/// the Execute plan and `Exec` replaces it with the point get.
#[test]
fn prepared_point_get_outside_autocommit_takes_the_normal_path() {
    let _serial = PREPARED_COUNT.lock();
    for case in ["autocommit = 0", "begin"] {
        let store = MockStore::new(10);
        let mut s = sbtest_session(&store);
        let info = block_on(s.prepare_stmt(SBTEST_POINT)).unwrap();
        match case {
            "begin" => s.vars_mut().set_in_txn(true),
            _ => s
                .vars_mut()
                .set_status_flag(tidb_base::mysql::ServerStatusAutocommit, false),
        }
        assert_eq!(
            execute_point_select(&mut s, info.stmt_id),
            ExecutePath::Normal,
            "{case}"
        );
        assert_eq!(s.executed_stmts(), 1, "{case}");
        s.core.rollback_txn();
        s.drop_prepared_stmt(info.stmt_id).unwrap();
    }
}

/// After a schema change the EXECUTE re-preprocesses the prepared statement and records the
/// new schema version (`planCachePreprocess` step 4) before `Compile` compares it, so it
/// still takes the short path, as Go.
#[test]
fn prepared_point_get_after_a_schema_change_takes_the_short_path() {
    let _serial = PREPARED_COUNT.lock();
    let store = MockStore::new(10);
    let mut s = sbtest_session(&store);
    let info = block_on(s.prepare_stmt(SBTEST_POINT)).unwrap();
    s.set_domain_info_schema(sbtest_info_schema_at(2));
    assert_eq!(
        execute_point_select(&mut s, info.stmt_id),
        ExecutePath::PointGetShortPath
    );
    assert_eq!(prepared_of(&s, info.stmt_id, |p| p.schema_version), 2);
    s.drop_prepared_stmt(info.stmt_id).unwrap();
}

/// `IsSafeToReusePointGetExecutor`'s three conditions one by one: a stale read, a session
/// outside an autocommit transaction, and a prepared statement of another schema version
/// each answer false (stale reads do not reach `Compile` on the new stack yet: the
/// preprocessor's stale-read branch is a boundary stub).
#[test]
fn is_safe_to_reuse_point_get_executor_checks_three_conditions() {
    use tidb_planner::core::plan_cache::is_safe_to_reuse_point_get_executor as safe;
    use tidb_planner::core::plan_cache_utils::PlanCacheStmt;
    let store = MockStore::new(10);
    let mut s = sbtest_session(&store);
    let is = sbtest_info_schema();
    let stmt = PlanCacheStmt {
        schema_version: 1,
        ..Default::default()
    };
    assert!(safe(s.vars(), is.as_ref(), &stmt));

    s.vars_mut().stmt_ctx_mut().is_staleness = true;
    assert!(!safe(s.vars(), is.as_ref(), &stmt), "stale read");
    s.vars_mut().stmt_ctx_mut().is_staleness = false;

    s.vars_mut().set_in_txn(true);
    assert!(!safe(s.vars(), is.as_ref(), &stmt), "explicit transaction");
    s.vars_mut().set_in_txn(false);
    s.vars_mut()
        .set_status_flag(tidb_base::mysql::ServerStatusAutocommit, false);
    assert!(!safe(s.vars(), is.as_ref(), &stmt), "autocommit = 0");
    s.vars_mut()
        .set_status_flag(tidb_base::mysql::ServerStatusAutocommit, true);

    let old = PlanCacheStmt {
        schema_version: 0,
        ..Default::default()
    };
    assert!(!safe(s.vars(), is.as_ref(), &old), "another schema version");
    assert!(safe(s.vars(), is.as_ref(), &stmt));
}

/// The text-protocol point select on the real session over the sbtest InfoSchema (mock store,
/// spec §9.2): the statement is handed over to `Compile`, planned on the fast path and frozen;
/// the transaction manager's current statement is then the very `Arc` of the `ExecStmt`. The
/// point get executor is built and opened, reads the mock store (no row), and the statement
/// ends with its autocommit transaction.
#[test]
fn text_protocol_point_select_runs_to_the_end() {
    let store = MockStore::new(10);
    let mut s = sbtest_session(&store);
    let sql = "SELECT c FROM sbtest1 WHERE id=10";
    let mut rs = block_on(s.execute_stmt(parse(sql), sql))
        .unwrap()
        .expect("a result set");
    assert_eq!(
        s.executed_stmts(),
        1,
        "the point get executor was built and opened"
    );
    assert!(
        s.vars().stmt_ctx().get_plan().is_some_and(|p| p
            .downcast_ref::<tidb_planner::PhysicalPlanRef>()
            .is_some_and(|p| p.kind() == tidb_planner::PhysicalKind::PointGet)),
        "the fast plan's point get"
    );
    let frozen = s.running.as_ref().unwrap().stmt.stmt_node.as_ref().unwrap();
    let current = s.core.txn_manager.get_current_stmt().unwrap();
    assert!(Arc::ptr_eq(frozen, current), "one frozen statement");
    let mut chk = rs.new_chunk().unwrap();
    block_on(rs.next(&mut s, &mut chk)).unwrap();
    assert_eq!(chk.num_rows(), 0);
    block_on(rs.close(&mut s)).unwrap();
    assert!(s.is_idle(), "{}", s.state_snapshot());
}

/// The outcome the router takes for a fallback (VS1 plan §4.3): the statement ends with a
/// boundary stub's error and the executed-statement count did not move (no executor opened).
/// Any other error (such as a 1105 "cannot find the reference" from `ResolveIndices`) would
/// reach the client instead of falling back. Returns the stub's block.
fn assert_falls_back<T>(s: &Session, what: &str, before: u64, r: Result<T, Error>) -> String {
    let e = match r {
        Ok(_) => panic!("{what}: ran on the new stack, expected a fallback"),
        Err(e) => e,
    };
    let Some(block) = blocked(&e) else {
        panic!("{what}: an error ({}) instead of a fallback: {e}", e.code());
    };
    assert_eq!(s.executed_stmts(), before, "{what}: an executor opened");
    assert!(s.is_idle(), "{what}: {}", s.state_snapshot());
    block.to_owned()
}

/// Range scans and `ORDER BY ... LIMIT` over the sbtest table now execute end-to-end on the
/// new stack (D12.1 Part 2A `TableReaderExecutor` + D07.3 `distsql` + D12.2/D12.5 root
/// operators) over the in-memory coprocessor `MockStore`, both as text queries and as
/// prepared statements (including plan cache hits on the second EXECUTE).
#[test]
fn range_and_order_by_limit_queries_run_on_new_stack() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let _enter = rt.enter();
    use tidb_types::datum::new_int_datum;
    let _serial = PREPARED_COUNT.lock();
    let store = MockStore::new(10);
    for id in 1..=10 {
        put_row(&store, 110, id, 2, &format!("c{id:02}"));
    }
    let mut s = sbtest_session(&store);
    let text_cases: [(&str, Vec<String>); 4] = [
        (
            "SELECT c FROM sbtest1 WHERE id BETWEEN 1 AND 5",
            (1..=5).map(|i| format!("c{i:02}")).collect(),
        ),
        (
            "SELECT c FROM sbtest1 WHERE id BETWEEN 1 AND 5 ORDER BY c",
            (1..=5).map(|i| format!("c{i:02}")).collect(),
        ),
        (
            "SELECT c FROM sbtest1 ORDER BY id LIMIT 5",
            (1..=5).map(|i| format!("c{i:02}")).collect(),
        ),
        (
            "SELECT id, c FROM sbtest1 WHERE id BETWEEN 1 AND 10 ORDER BY c LIMIT 3",
            (1..=3).map(|i| format!("c{i:02}")).collect(),
        ),
    ];
    for (sql, want_c) in text_cases {
        let mut rs = block_on(s.execute_stmt(parse(sql), sql))
            .unwrap_or_else(|e| panic!("{sql}: {e}"))
            .expect("a result set");
        let fields = rs.fields(&s).unwrap();
        let c_idx = fields.len() - 1;
        let mut chk = rs.new_chunk().unwrap();
        let mut got = Vec::new();
        loop {
            block_on(rs.next(&mut s, &mut chk)).unwrap();
            if chk.num_rows() == 0 {
                break;
            }
            for i in 0..chk.num_rows() {
                got.push(String::from_utf8_lossy(chk.get_row(i).get_string(c_idx)).into_owned());
            }
        }
        block_on(rs.close(&mut s)).unwrap();
        assert!(s.is_idle(), "{sql}: {}", s.state_snapshot());
        assert_eq!(got, want_c, "{sql}");
    }
    let prepared = [
        ("SELECT c FROM sbtest1 WHERE id BETWEEN ? AND ?", 2),
        (
            "SELECT c FROM sbtest1 WHERE id BETWEEN ? AND ? ORDER BY c",
            2,
        ),
        (
            "SELECT id, c FROM sbtest1 WHERE id BETWEEN ? AND ? ORDER BY c LIMIT 3",
            2,
        ),
        ("SELECT c FROM sbtest1 WHERE id > ? ORDER BY id LIMIT 5", 1),
    ];
    for (sql, n) in prepared {
        let info = block_on(s.prepare_stmt(sql)).unwrap();
        for round in 1..=2 {
            let params = (0..n).map(|i| new_int_datum(1 + 9 * i)).collect();
            let mut rs = block_on(s.execute_prepared_stmt(info.stmt_id, params))
                .unwrap_or_else(|e| panic!("EXECUTE {sql} (round {round}): {e}"))
                .expect("a result set");
            let mut chk = rs.new_chunk().unwrap();
            let mut count = 0usize;
            loop {
                block_on(rs.next(&mut s, &mut chk)).unwrap();
                if chk.num_rows() == 0 {
                    break;
                }
                count += chk.num_rows();
            }
            block_on(rs.close(&mut s)).unwrap();
            assert!(s.is_idle());
            assert!(count > 0, "EXECUTE {sql} (round {round}): expected rows");
        }
        s.drop_prepared_stmt(info.stmt_id).unwrap();
    }
    // Parameter marker in LIMIT executes on the new stack.
    let sql = "SELECT c FROM sbtest1 ORDER BY id LIMIT ?";
    let info = block_on(s.prepare_stmt(sql)).unwrap();
    for round in 1..=2 {
        let mut rs = block_on(s.execute_prepared_stmt(info.stmt_id, vec![new_int_datum(5)]))
            .unwrap_or_else(|e| panic!("EXECUTE {sql} (round {round}): {e}"))
            .expect("a result set");
        let mut chk = rs.new_chunk().unwrap();
        let mut count = 0usize;
        loop {
            block_on(rs.next(&mut s, &mut chk)).unwrap();
            if chk.num_rows() == 0 {
                break;
            }
            count += chk.num_rows();
        }
        block_on(rs.close(&mut s)).unwrap();
        assert!(s.is_idle());
        assert_eq!(count, 5, "EXECUTE {sql} (round {round})");
    }
    s.drop_prepared_stmt(info.stmt_id).unwrap();
    assert!(s.executed_stmts() > 0);
}

/// What an EXECUTE of a prepared statement did: the rows `(id, c)` it returned when it ran
/// to the end on the new stack, or the block of the boundary stub it fell back at (checked
/// with [`assert_falls_back`]: no executor opened, the session idle); with
/// `FoundInPlanCache`.
fn execute_id_c_rows(
    s: &mut Session,
    stmt_id: u32,
    params: Vec<tidb_types::datum::Datum>,
    what: &str,
) -> (Result<Vec<(i64, String)>, String>, bool) {
    let before = s.executed_stmts();
    let r = block_on(s.execute_prepared_stmt(stmt_id, params));
    let found = s.vars().found_in_plan_cache;
    let mut rs = match r {
        Ok(rs) => rs.expect("a result set"),
        Err(e) => {
            let block = assert_falls_back::<()>(s, what, before, Err(e));
            return (Err(block), found);
        }
    };
    let mut out = Vec::new();
    loop {
        let mut chk = rs.new_chunk().unwrap();
        block_on(rs.next(s, &mut chk)).unwrap();
        if chk.num_rows() == 0 {
            break;
        }
        for i in 0..chk.num_rows() {
            let row = chk.get_row(i);
            let c = String::from_utf8_lossy(row.get_string(1)).into_owned();
            out.push((row.get_int64(0), c));
        }
    }
    block_on(rs.close(s)).unwrap();
    assert!(s.is_idle(), "{what}: {}", s.state_snapshot());
    (Ok(out), found)
}

/// Every `?` of a prepared statement reads its own parameter (Go numbers the markers by
/// position in `GeneratePlanCacheStmtWithAST`, `SetOrder`; #352: every marker read the first
/// parameter, so `id BETWEEN ? AND ?` became the range `[first, first]` and a point get that
/// returned one row). The table has the rows 1..=10 (`c` = "c<id>"). Each statement runs
/// twice with different values: the first EXECUTE optimizes (a plan cache miss), the second
/// meets the cached plan where it is cacheable (a hit, its ranges rebuilt for the new
/// values). Each EXECUTE either returns exactly the rows Go returns or falls back before any
/// executor opened, at the stub pinned here (the router then runs it on Go TiDB): it never
/// returns other rows.
#[test]
fn prepared_statement_markers_read_their_own_parameters() {
    use tidb_types::datum::{Datum, new_int_datum, new_string_datum};
    let _serial = PREPARED_COUNT.lock();
    let store = MockStore::new(10);
    for id in 1..=10 {
        put_row(&store, 110, id, 2, &format!("c{id}"));
    }
    let cache = new_test_cache();
    let mut s = cached_session(&store, sbtest_info_schema_collated(), &cache);
    let int = new_int_datum;
    let str = new_string_datum;
    let rows = |ids: std::ops::RangeInclusive<i64>| -> Vec<(i64, String)> {
        ids.map(|id| (id, format!("c{id}"))).collect()
    };
    type Round = (Vec<Datum>, Vec<(i64, String)>, Option<&'static str>, bool);
    let cases: Vec<(&str, Vec<Round>)> = vec![
        // A table range scan [3, 7], then [2, 9] (Go: TableReader > TableRangeScan); both now
        // run end-to-end on the new stack, and the second is a plan cache hit.
        (
            "SELECT id, c FROM sbtest1 WHERE id BETWEEN ? AND ?",
            vec![
                (vec![int(3), int(7)], rows(3..=7), None, false),
                (vec![int(2), int(9)], rows(2..=9), None, true),
            ],
        ),
        (
            "SELECT id, c FROM sbtest1 WHERE id >= ? AND id <= ?",
            vec![
                (vec![int(3), int(7)], rows(3..=7), None, false),
                (vec![int(2), int(9)], rows(2..=9), None, true),
            ],
        ),
        // Markers not all in the range: the third parameter is the upper bound.
        (
            "SELECT id, c FROM sbtest1 WHERE c = ? AND id BETWEEN ? AND ?",
            vec![
                (vec![str("c5"), int(3), int(7)], rows(5..=5), None, false),
                (vec![str("c8"), int(2), int(9)], rows(8..=8), None, true),
            ],
        ),
        // Two equal conditions on the handle: the same value is a point get of that row,
        // run to the end on the new stack; different values are an empty range (Go: a
        // TableDual, not cached), now run by the basic executors (D12.2) and read as no rows.
        // With every marker reading the first parameter, the second EXECUTE returned the row 5.
        (
            "SELECT id, c FROM sbtest1 WHERE id = ? AND id = ?",
            vec![
                (vec![int(4), int(4)], rows(4..=4), None, false),
                (vec![int(5), int(6)], vec![], None, false),
            ],
        ),
    ];
    for (sql, rounds) in cases {
        let info = block_on(s.prepare_stmt(sql)).unwrap();
        for (round, (params, want, stop, hit)) in rounds.into_iter().enumerate() {
            let what = format!("EXECUTE {sql} (round {})", round + 1);
            let (got, found) = execute_id_c_rows(&mut s, info.stmt_id, params, &what);
            match got {
                Ok(got) => assert_eq!(got, want, "{what}: the rows"),
                Err(block) => {
                    assert_eq!(Some(block.as_str()), stop, "{what}: Go returns {want:?}")
                }
            }
            assert_eq!(found, hit, "{what}: FoundInPlanCache");
        }
        s.drop_prepared_stmt(info.stmt_id).unwrap();
    }
}

/// The parameter values in the session (`SessionVars.PlanCacheParams`), as text.
fn params_of(s: &Session) -> Vec<String> {
    s.vars()
        .state
        .plan_cache_params
        .lock()
        .all_param_values()
        .iter()
        .map(datum_text)
        .collect()
}

/// A datum as text (`NULL` for the null datum).
fn datum_text(d: &tidb_types::datum::Datum) -> String {
    if d.is_null() {
        return "NULL".to_owned();
    }
    String::from_utf8_lossy(&d.to_string().unwrap()).into_owned()
}

/// A real-session EXECUTE reads its parameters (D05.3 spec §8–§9): for an int, a string and
/// NULL, the plan cache's miss path sets the values into the session; the planning context
/// the session hands the planner (its `sessionexpr` context) reads them, and a `?` the
/// planner's expression rewriter builds in that context is a constant of the parameter's
/// value and type (Go's `ParamMarkerExpression`); planning goes on to the physical
/// optimization boundary, before anything ran.
#[test]
fn execute_reads_the_parameters_through_the_session_expression_context() {
    use tidb_sessionctx::exprctx::BuildContext;
    use tidb_types::datum::{Datum, new_int_datum, new_string_datum};
    let _serial = PREPARED_COUNT.lock();
    let store = MockStore::new(10);
    let mut s = sbtest_session(&store);
    let info = block_on(s.prepare_stmt("SELECT c FROM sbtest1 WHERE id > ?")).unwrap();
    let select_marker = parse("SELECT ?");
    let ast::Stmt::Select(sel) = &select_marker else {
        unreachable!()
    };
    let marker = sel.fields.fields[0].expr.as_ref().unwrap();
    let cases: [(Datum, &str, u8); 3] = [
        (new_int_datum(7), "7", tidb_base::mysql::TypeLonglong),
        (new_string_datum("a"), "a", tidb_base::mysql::TypeVarString),
        // Go's InferParamTypeFromUnderlyingValue: a NULL parameter is a VARCHAR.
        (Datum::default(), "NULL", tidb_base::mysql::TypeVarString),
    ];
    for (param, text, tp) in cases {
        match block_on(s.execute_prepared_stmt(info.stmt_id, vec![param])) {
            Err(e) => {
                assert!(
                    PHYSICAL_BOUNDARIES.contains(&blocked(&e).unwrap_or_default()),
                    "{text}: {e}"
                );
                assert_eq!(s.executed_stmts(), 0, "{text}");
            }
            // With TableReaderExecutor (D12.1 Part 2A), `SELECT c FROM sbtest1 WHERE id > ?`
            // runs to completion on the new stack (empty table here, so 0 rows).
            Ok(rs) => {
                let mut rs = rs.expect("rows");
                let mut chk = rs.new_chunk().unwrap();
                block_on(rs.next(&mut s, &mut chk)).unwrap();
                assert_eq!(chk.num_rows(), 0, "{text}");
                block_on(rs.close(&mut s)).unwrap();
            }
        }
        assert!(s.is_idle(), "{text}");
        // The value the plan cache set (planCachePreprocess) ...
        let expected = vec![text.to_owned()];
        assert_eq!(params_of(&s), expected, "{text}");
        // ... is what the session's planning context reads ...
        let ctx = s.core.get_plan_ctx().expr_ctx;
        let v = ctx.get_eval_ctx().get_param_value(0).unwrap();
        assert_eq!(datum_text(&v), expected[0], "{text}");
        assert!(ctx.get_eval_ctx().get_param_value(1).is_err());
        // ... and what the planner builds a parameter marker from.
        let (expr, _) = tidb_planner::core::expression_rewriter::rewrite_ast_expr_with_plan_ctx(
            &*ctx as &dyn BuildContext,
            marker,
            None,
            &[],
            false,
        )
        .unwrap();
        let tidb_expression::Expr::Constant(c) = &*expr else {
            panic!("{text}: {expr:?}");
        };
        assert_eq!(datum_text(&c.value), expected[0], "{text}");
        assert_eq!(c.ret_type.get_type(), tp, "{text}");
    }
    s.drop_prepared_stmt(info.stmt_id).unwrap();
}

/// An autocommit read (`EXECUTE`, a plain SELECT) registers nothing: its statement context is
/// read-only and the session is not in a transaction (Go returns before the lock branch).
/// A statement of an explicit transaction registers (and keeps it to the end of the
/// transaction).
#[test]
fn autocommit_read_registers_nothing_explicit_txn_does() {
    use tidb_planner::core::plan_cache_utils::InfoSchemaLookup;
    use tidb_planner::core::preprocess::try_lock_mdl_and_update_schema_if_necessary as lock;
    let store = MockStore::new(10);
    let mut s = sbtest_session(&store);
    let is = sbtest_info_schema();
    let db = tidb_parser::model::CIStr::new("test");
    let tbl = InfoSchemaLookup::table_by_name(
        is.as_ref(),
        &db,
        &tidb_parser::model::CIStr::new("sbtest1"),
    )
    .unwrap();
    // Autocommit read (what EXECUTE of the point select is).
    let _ = compile(&mut s, "SELECT c FROM sbtest1 WHERE id = 1", 0);
    // What Compile sets (`plannercore.IsReadOnly`, still a boundary stub: D09.13).
    s.vars_mut().stmt_ctx_mut().is_read_only = true;
    let t = lock(
        &mut s.core.sctx.vars,
        &db,
        Arc::clone(&tbl),
        is.as_ref(),
        Some(is.as_ref()),
    )
    .unwrap();
    assert!(Arc::ptr_eq(&t, &tbl));
    assert_eq!(mdl_entries(&s).unwrap_or_default(), vec![]);
    assert!(s.vars().stmt_ctx().related_table_ids.contains(&110));
    s.core.rollback_txn();

    // In an explicit transaction the same read registers.
    s.vars_mut().set_in_txn(true);
    s.vars_mut().state.txn_ctx.enable_mdl = true;
    lock(
        &mut s.core.sctx.vars,
        &db,
        tbl,
        is.as_ref(),
        Some(is.as_ref()),
    )
    .unwrap();
    assert_eq!(mdl_entries(&s), Some(vec![(110, 1)]));
    // Reported per Go: a DDL job on the table at version 2 is held back by this session.
    let mut job2ver = HashMap::from([(7, 2), (8, 2)]);
    let job2ids = HashMap::from([(7, [110].into()), (8, [111].into())]);
    crate::session::remove_lock_ddl_jobs(s.vars(), &mut job2ver, &job2ids);
    assert_eq!(job2ver, HashMap::from([(8, 2)]));
    s.core.rollback_txn();
    assert_eq!(mdl_entries(&s), None);
}

/// A point get by the integer primary key, and by the primary key of a clustered table, read
/// at MaxTS with no TSO request (Go `IsPointGetWithPKOrUniqueKeyByAutoCommit`); a point get
/// by a secondary unique index needs a second read and takes a TSO, as Go.
#[test]
fn autocommit_point_get_by_primary_key_reads_at_max_ts() {
    use tidb_planner::core::point_get_plan::PointGetPlan;
    let tbl = sbtest_info_schema()
        .table_by_name_resident(
            &tidb_parser::model::CIStr::new("test"),
            &tidb_parser::model::CIStr::new("sbtest1"),
        )
        .unwrap()
        .map(|t| Arc::clone(t.meta()))
        .unwrap();
    let uk = Arc::new(tidb_model::IndexInfo {
        id: 1,
        name: tidb_parser::model::CIStr::new("k"),
        unique: true,
        state: tidb_model::StatePublic,
        ..Default::default()
    });
    let mut clustered = tidb_model::TableInfo::clone(&tbl);
    clustered.is_common_handle = true;
    let pk = Arc::new(tidb_model::IndexInfo {
        primary: true,
        ..(*uk).clone()
    });
    let cases: Vec<(&str, PointGetPlan, usize, bool)> = vec![
        (
            "int handle",
            PointGetPlan {
                tbl_info: Arc::clone(&tbl),
                handle: Some(tidb_kv::Handle::Int(tidb_kv::key::IntHandle(1))),
                ..PointGetPlan::default()
            },
            0,
            true,
        ),
        (
            "clustered primary key",
            PointGetPlan {
                tbl_info: Arc::new(clustered),
                index_info: Some(pk),
                ..PointGetPlan::default()
            },
            0,
            true,
        ),
        (
            "secondary unique index",
            PointGetPlan {
                tbl_info: Arc::clone(&tbl),
                index_info: Some(uk),
                ..PointGetPlan::default()
            },
            1,
            false,
        ),
    ];
    for (name, plan, tso, max_ts) in cases {
        let store = MockStore::new(10);
        let mut s = session(&store);
        block_on(s.core.prepare_txn_ctx()).unwrap();
        let node = parse("SELECT c FROM sbtest1 WHERE id = 1");
        reset_context_of_stmt(&mut s.core, &node, "").unwrap();
        s.core
            .txn_manager
            .on_stmt_start(&mut s.core.sctx, Some(&node))
            .unwrap();
        let plan: tidb_planner::PhysicalPlanRef = Arc::new(plan);
        s.core
            .advise_optimize_with_plan_and_then_warm_up(&plan)
            .unwrap();
        let ts = block_on(s.core.get_stmt_read_ts()).unwrap();
        assert_eq!(ts == u64::MAX, max_ts, "{name}: ts {ts}");
        assert_eq!(store.requests.load(SeqCst), tso, "{name}");
        s.core.rollback_txn();
    }
}

/// A prepared statement as PREPARE leaves it in the session (on a real session PREPARE
/// still stops at the plan builder, so the test builds the `PlanCacheStmt`).
fn register_prepared(s: &mut Session, id: u32, sql: &str) {
    let mut ps = tidb_planner::core::plan_cache_utils::PlanCacheStmt::default();
    ps.prepared_ast.stmt = Box::new(parse(sql));
    ps.prepared_ast.stmt_type = "Select".to_owned();
    ps.stmt_text = sql.to_owned();
    ps.params = sql.match_indices('?').map(|(i, _)| i).collect();
    s.vars_mut()
        .add_prepared_stmt(id, Arc::new(parking_lot::Mutex::new(ps)))
        .unwrap();
}

/// Where an EXECUTE that is not a point get stops while the planner and the executors are not
/// finished: the statistics derivation (D10.6), FindBestTask (D10.2), ResolveIndices
/// (D10.9) or the cost (D10.5), in that order; with a finished plan, building an executor
/// not ported yet in `Exec` (the table reader D12.1, VS1-34). Every one of them comes before an executor opened, so the caller falls
/// back. The plan replayer capture (`recordTableRuntimeStats`) no longer stops it (see
/// `plan_replayer_capture_records_the_table_stats_and_goes_on`).
const PHYSICAL_BOUNDARIES: [&str; 5] = ["D10.6", "D10.2", "D10.9", "D10.5", "D12.1"];

/// With the plan replayer capture on (Go's default once the session loaded the global
/// values), the column statistics usage records the statistics of the visited tables in
/// `StmtCtx.TableStats` (`recordTableRuntimeStats`: the pseudo table, as Go's nil statistics
/// handle gives) and the statement goes on: without a capture task (the new stack loads
/// none) neither the planner nor the capture check of `Exec` stops it, and a non-point
/// EXECUTE is planned to the end and stops when `Exec` builds the table reader executor
/// (D12.1), before any executor opened.
#[test]
fn plan_replayer_capture_records_the_table_stats_and_goes_on() {
    let _serial = PREPARED_COUNT.lock();
    let store = MockStore::new(10);
    let mut s = sbtest_session(&store);
    s.vars_mut().enable_plan_replayer_capture = true;
    assert!(s.vars().is_plan_replayer_capture_enabled());
    let info = block_on(s.prepare_stmt("SELECT c FROM sbtest1 WHERE id > ?")).unwrap();
    let mut rs =
        block_on(s.execute_prepared_stmt(info.stmt_id, vec![tidb_types::datum::new_int_datum(1)]))
            .unwrap()
            .expect("a result set");
    let mut chk = rs.new_chunk().unwrap();
    block_on(rs.next(&mut s, &mut chk)).unwrap();
    block_on(rs.close(&mut s)).unwrap();
    let sc = s.vars().stmt_ctx();
    assert_eq!(sc.table_stats.len(), 1);
    let stats = sc.table_stats[&110]
        .downcast_ref::<Option<Arc<tidb_statistics::Table>>>()
        .expect("a statistics table")
        .as_ref()
        .expect("the pseudo table");
    assert!(stats.hist_coll.pseudo);
    assert_eq!(stats.hist_coll.physical_id, 110);
    assert!(s.is_idle());
}

/// The binary protocol's EXECUTE of a prepared statement runs through `ExecuteStmt` with an
/// EXECUTE node (Go `ExecutePreparedStmt`): `ResetContextOfStmt` works on the prepared
/// SELECT and its text, and `Compile` runs `IsReadOnly`, the preprocessor and
/// `planner.Optimize` (the plan cache's miss path) up to the physical optimization
/// boundary, so the caller falls back; restoring the checkpoint gives the session back
/// exactly (also the parameter values the planner set). The statement-level helpers read the
/// prepared statement too.
#[test]
fn execute_of_a_prepared_stmt_reaches_the_planner() {
    let _serial = PREPARED_COUNT.lock();
    let store = MockStore::new(10);
    let mut s = sbtest_session(&store);
    // Prepare under default settings, then switch to cost_model_version=1 (EXC.B) before
    // EXECUTE so EXECUTE reaches physical planning and falls back, exercising checkpoint /
    // restore now that TableReader executes `SELECT c FROM sbtest1 WHERE id > ?`.
    let sql = "SELECT c FROM sbtest1 WHERE id > ?";
    let info = block_on(s.prepare_stmt(sql)).unwrap();
    s.vars_mut().cost_model_version = 1;
    s.vars().stmt_ctx().append_warning(Error::plain("earlier"));
    let before = s.state_snapshot();
    let cp = s.checkpoint();
    let e =
        block_on(s.execute_prepared_stmt(info.stmt_id, vec![tidb_types::datum::new_int_datum(1)]))
            .err()
            .unwrap();
    assert_eq!(blocked(&e), Some("EXC.B"), "{e}");
    let sc = s.vars().stmt_ctx();
    assert!(sc.in_select_stmt);
    assert!(sc.is_read_only, "Compile ran planner.IsReadOnly");
    assert_eq!(sc.original_sql, sql);
    assert_eq!(s.executed_stmts(), 0);
    assert!(
        s.is_idle(),
        "rollbackOnError ended the statement's transaction"
    );
    assert_eq!(
        s.vars()
            .state
            .plan_cache_params
            .lock()
            .all_param_values()
            .len(),
        1
    );
    s.restore(cp).unwrap();
    assert_eq!(s.state_snapshot(), before);
    assert!(s.is_idle());
    assert_eq!(s.vars().stmt_ctx().warning_count(), 1);

    // isEndTxnStmt of an EXECUTE is that of its statement; an unknown name is Go's 8111.
    let exec_of = |s: &Session, id: u32| {
        ast::Stmt::Execute(Box::new(ast::ExecuteStmt {
            prep_stmt: Some(s.vars().get_prepared_stmt_by_id(id).unwrap()),
            prep_stmt_id: id,
            ..Default::default()
        }))
    };
    assert!(!crate::session::is_end_txn_stmt(&exec_of(&s, info.stmt_id), s.vars()).unwrap());
    register_prepared(&mut s, 2, "COMMIT");
    assert!(crate::session::is_end_txn_stmt(&exec_of(&s, 2), s.vars()).unwrap());
    let by_name = ast::Stmt::Execute(Box::new(ast::ExecuteStmt {
        name: "nope".to_owned(),
        ..Default::default()
    }));
    let e = crate::session::is_end_txn_stmt(&by_name, s.vars()).unwrap_err();
    assert_eq!(e.code(), 8111);
    assert!(matches!(
        crate::session::resolve_prepared_stmt(Some(&exec_of(&s, 2)), Some(s.vars())),
        Ok(Some(crate::session::ResolvedStmt::Prepared(_)))
    ));
    assert!(matches!(
        crate::session::resolve_prepared_stmt(Some(&exec_of(&s, 2)), None),
        Ok(None)
    ));

    // A value that is not a plan-cache statement is refused before anything runs.
    s.vars_mut().add_prepared_stmt(3, Arc::new(())).unwrap();
    let e = block_on(s.execute_prepared_stmt(3, vec![])).err().unwrap();
    assert!(e.to_string().contains("invalid PlanCacheStmt type"), "{e}");
}

/// The coprocessor client of [`MockStore`]: the planner keeps it in the planning context;
/// no test sends a request.
/// In-memory coprocessor client over [`MockRows`]: decodes `tipb::DagRequest`, scans matching
/// key ranges from `MockStore.rows`, executes pushed-down `TableScan` / `Selection` / `TopN`
/// / `Limit`, and encodes the result as a `tipb::SelectResponse` (`TypeChunk` or
/// `TypeDefault`). Used by session integration tests (`sbtest_results`, range queries).
struct MockClient(MockRows);

struct MockCopSubset(Vec<u8>);

impl tidb_kv::kv::ResultSubset for MockCopSubset {
    fn get_data(&self) -> &[u8] {
        &self.0
    }
    fn get_start_key(&self) -> &Key {
        static EMPTY: Key = Key(Vec::new());
        &EMPTY
    }
    fn mem_size(&self) -> i64 {
        self.0.len() as i64
    }
    fn resp_time(&self) -> std::time::Duration {
        std::time::Duration::ZERO
    }
}

struct MockCopResponse {
    data: Option<Vec<u8>>,
}

impl tidb_kv::kv::Response for MockCopResponse {
    fn next<'a>(
        &'a mut self,
        _: &'a tidb_kv::Context,
    ) -> KvFuture<'a, Option<Box<dyn tidb_kv::kv::ResultSubset>>> {
        let data = self.data.take();
        Box::pin(async move {
            Ok(data.map(|d| Box::new(MockCopSubset(d)) as Box<dyn tidb_kv::kv::ResultSubset>))
        })
    }
    fn close(&mut self) -> Result<(), Error> {
        self.data = None;
        Ok(())
    }
}

fn tipb_col_to_field_type(col: &tipb::ColumnInfo) -> tidb_parser::types::FieldType {
    let mut ft = tidb_types::field_type::new_field_type(col.get_tp() as u8);
    ft.set_flag(col.get_flag() as usize);
    ft.set_flen(col.get_column_len() as isize);
    ft.set_decimal(col.get_decimal() as isize);
    if matches!(
        ft.get_type(),
        tidb_base::mysql::TypeString
            | tidb_base::mysql::TypeVarchar
            | tidb_base::mysql::TypeVarString
    ) {
        ft.set_charset("utf8mb4");
        ft.set_collate("utf8mb4_bin");
    }
    ft
}

fn eval_tipb_expr(expr: &tipb::Expr, row: &[tidb_types::datum::Datum]) -> tidb_types::datum::Datum {
    use tidb_types::datum::{Datum, new_bytes_datum, new_int_datum, new_uint_datum};
    match expr.get_tp() {
        tipb::ExprType::ColumnRef => {
            let (_, idx) = tidb_codec::number::decode_int(expr.get_val()).unwrap();
            row[idx as usize].clone()
        }
        tipb::ExprType::Int64 => {
            let (_, v) = tidb_codec::number::decode_int(expr.get_val()).unwrap();
            new_int_datum(v)
        }
        tipb::ExprType::Uint64 => {
            let (_, v) = tidb_codec::number::decode_uint(expr.get_val()).unwrap();
            new_uint_datum(v)
        }
        tipb::ExprType::String | tipb::ExprType::Bytes => new_bytes_datum(expr.get_val().to_vec()),
        tipb::ExprType::Null => Datum::default(),
        tipb::ExprType::ScalarFunc => {
            let children = expr.get_children();
            let sig = expr.get_sig();
            match sig {
                tipb::ScalarFuncSig::EqInt
                | tipb::ScalarFuncSig::NeInt
                | tipb::ScalarFuncSig::LtInt
                | tipb::ScalarFuncSig::LeInt
                | tipb::ScalarFuncSig::GtInt
                | tipb::ScalarFuncSig::GeInt => {
                    let a = eval_tipb_expr(&children[0], row);
                    let b = eval_tipb_expr(&children[1], row);
                    if a.is_null() || b.is_null() {
                        return Datum::default();
                    }
                    let (av, bv) = (a.get_int64(), b.get_int64());
                    let res = match sig {
                        tipb::ScalarFuncSig::EqInt => av == bv,
                        tipb::ScalarFuncSig::NeInt => av != bv,
                        tipb::ScalarFuncSig::LtInt => av < bv,
                        tipb::ScalarFuncSig::LeInt => av <= bv,
                        tipb::ScalarFuncSig::GtInt => av > bv,
                        tipb::ScalarFuncSig::GeInt => av >= bv,
                        _ => unreachable!(),
                    };
                    new_int_datum(i64::from(res))
                }
                tipb::ScalarFuncSig::EqString
                | tipb::ScalarFuncSig::NeString
                | tipb::ScalarFuncSig::LtString
                | tipb::ScalarFuncSig::LeString
                | tipb::ScalarFuncSig::GtString
                | tipb::ScalarFuncSig::GeString => {
                    let a = eval_tipb_expr(&children[0], row);
                    let b = eval_tipb_expr(&children[1], row);
                    if a.is_null() || b.is_null() {
                        return Datum::default();
                    }
                    let (av, bv) = (a.get_bytes(), b.get_bytes());
                    let res = match sig {
                        tipb::ScalarFuncSig::EqString => av == bv,
                        tipb::ScalarFuncSig::NeString => av != bv,
                        tipb::ScalarFuncSig::LtString => av < bv,
                        tipb::ScalarFuncSig::LeString => av <= bv,
                        tipb::ScalarFuncSig::GtString => av > bv,
                        tipb::ScalarFuncSig::GeString => av >= bv,
                        _ => unreachable!(),
                    };
                    new_int_datum(i64::from(res))
                }
                tipb::ScalarFuncSig::LogicalAnd => {
                    let a = eval_tipb_expr(&children[0], row);
                    let b = eval_tipb_expr(&children[1], row);
                    let av = !a.is_null() && a.get_int64() != 0;
                    let bv = !b.is_null() && b.get_int64() != 0;
                    new_int_datum(i64::from(av && bv))
                }
                tipb::ScalarFuncSig::LogicalOr => {
                    let a = eval_tipb_expr(&children[0], row);
                    let b = eval_tipb_expr(&children[1], row);
                    let av = !a.is_null() && a.get_int64() != 0;
                    let bv = !b.is_null() && b.get_int64() != 0;
                    new_int_datum(i64::from(av || bv))
                }
                other => panic!("MockClient: unsupported ScalarFuncSig {other:?}"),
            }
        }
        other => panic!("MockClient: unsupported ExprType {other:?}"),
    }
}

impl Client for MockClient {
    fn send(
        &self,
        _: &tidb_kv::Context,
        req: &tidb_kv::kv::Request,
        _: Option<tidb_kv::kv::OptionValue>,
        _: &tidb_kv::kv::ClientSendOption,
    ) -> Box<dyn tidb_kv::kv::Response> {
        use protobuf::Message;
        let mut dag = tipb::DagRequest::default();
        dag.merge_from_bytes(&req.data).expect("valid DAGRequest");
        let execs = dag.get_executors();
        let key_ranges = req
            .key_ranges
            .as_ref()
            .map(|krs| krs.append_self_to(Vec::new()))
            .unwrap_or_default();
        let mut matched: Vec<(Vec<u8>, Vec<u8>)> = {
            let store = self.0.lock();
            store
                .iter()
                .filter(|(k, _)| {
                    key_ranges.iter().any(|kr| {
                        k.as_slice() >= kr.start_key.0.as_slice()
                            && k.as_slice() < kr.end_key.0.as_slice()
                    })
                })
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        };
        if req.desc {
            matched.sort_by(|a, b| b.0.cmp(&a.0));
        } else {
            matched.sort_by(|a, b| a.0.cmp(&b.0));
        }

        let mut col_fts: Vec<tidb_parser::types::FieldType>;
        let mut rows: Vec<Vec<tidb_types::datum::Datum>> = Vec::with_capacity(matched.len());
        match execs[0].get_tp() {
            tipb::ExecType::TypeTableScan => {
                let scan = execs[0].get_tbl_scan();
                let cols = scan.get_columns();
                col_fts = cols.iter().map(tipb_col_to_field_type).collect();
                let pk_col_ids: Vec<i64> = cols
                    .iter()
                    .filter(|c| c.get_pk_handle())
                    .map(|c| c.get_column_id())
                    .collect();
                let decoder_cols: Vec<tidb_tablecodec::rowcodec::decoder::ColInfo> = cols
                    .iter()
                    .zip(col_fts.iter())
                    .map(|(c, ft)| tidb_tablecodec::rowcodec::decoder::ColInfo {
                        id: c.get_column_id(),
                        ft: Arc::new(ft.clone()),
                        ..Default::default()
                    })
                    .collect();
                let decoder = tidb_tablecodec::rowcodec::decoder::new_chunk_decoder(
                    decoder_cols,
                    pk_col_ids,
                    None,
                    None,
                );
                let mut tmp_chk = tidb_chunk::chunk::new_chunk_with_capacity(&col_fts, 1);
                for (k, v) in &matched {
                    let handle =
                        tidb_tablecodec::decode_row_key(&tidb_kv::key::Key(k.clone())).unwrap();
                    tmp_chk.reset();
                    decoder
                        .decode_to_chunk(v, 0, Some(&handle), &mut tmp_chk)
                        .unwrap();
                    let row = tmp_chk.get_row(0);
                    let datums = col_fts
                        .iter()
                        .enumerate()
                        .map(|(i, ft)| row.get_datum(i, ft))
                        .collect();
                    rows.push(datums);
                }
            }
            tipb::ExecType::TypeIndexScan => {
                let scan = execs[0].get_idx_scan();
                let cols = scan.get_columns();
                col_fts = cols.iter().map(tipb_col_to_field_type).collect();
                for (k, _v) in &matched {
                    // Strip 't'(1) + table_id(8) + "_i"(2) + index_id(8) = 19-byte prefix.
                    let mut rest = &k[19..];
                    let mut datums = Vec::with_capacity(cols.len());
                    for _ in 0..cols.len() {
                        let (next_rest, d, err) = tidb_codec::codec::decode_one(rest);
                        err.unwrap();
                        datums.push(d);
                        rest = next_rest;
                    }
                    rows.push(datums);
                }
            }
            other => panic!("MockClient: unsupported scan executor {other:?}"),
        }

        for exec in &execs[1..] {
            match exec.get_tp() {
                tipb::ExecType::TypeSelection => {
                    let conds = exec.get_selection().get_conditions();
                    rows.retain(|r| {
                        conds.iter().all(|c| {
                            let v = eval_tipb_expr(c, r);
                            !v.is_null() && v.get_int64() != 0
                        })
                    });
                }
                tipb::ExecType::TypeProjection => {
                    let exprs = exec.get_projection().get_exprs();
                    rows = rows
                        .iter()
                        .map(|r| exprs.iter().map(|e| eval_tipb_expr(e, r)).collect())
                        .collect();
                    col_fts = exprs
                        .iter()
                        .map(|e| {
                            if e.has_field_type() {
                                let pft = e.get_field_type();
                                let mut ft =
                                    tidb_types::field_type::new_field_type(pft.get_tp() as u8);
                                ft.set_flag(pft.get_flag() as usize);
                                ft.set_flen(pft.get_flen() as isize);
                                ft.set_decimal(pft.get_decimal() as isize);
                                if matches!(
                                    ft.get_type(),
                                    tidb_base::mysql::TypeString
                                        | tidb_base::mysql::TypeVarchar
                                        | tidb_base::mysql::TypeVarString
                                ) {
                                    ft.set_charset("utf8mb4");
                                    ft.set_collate("utf8mb4_bin");
                                }
                                ft
                            } else if e.get_tp() == tipb::ExprType::ColumnRef {
                                let (_, idx) = tidb_codec::number::decode_int(e.get_val()).unwrap();
                                col_fts[idx as usize].clone()
                            } else {
                                tidb_types::field_type::new_field_type(
                                    tidb_base::mysql::TypeLonglong,
                                )
                            }
                        })
                        .collect();
                }
                tipb::ExecType::TypeLimit => {
                    rows.truncate(exec.get_limit().get_limit() as usize);
                }
                tipb::ExecType::TypeTopN => {
                    let topn = exec.get_top_n();
                    let order_by = topn.get_order_by();
                    rows.sort_by(|a, b| {
                        for item in order_by {
                            let va = eval_tipb_expr(item.get_expr(), a);
                            let vb = eval_tipb_expr(item.get_expr(), b);
                            let ord = match (va.is_null(), vb.is_null()) {
                                (true, true) => std::cmp::Ordering::Equal,
                                (true, false) => std::cmp::Ordering::Less,
                                (false, true) => std::cmp::Ordering::Greater,
                                (false, false) => {
                                    if va.kind() == tidb_types::datum::KindInt64 {
                                        va.get_int64().cmp(&vb.get_int64())
                                    } else {
                                        va.get_bytes().cmp(vb.get_bytes())
                                    }
                                }
                            };
                            let ord = if item.get_desc() { ord.reverse() } else { ord };
                            if ord != std::cmp::Ordering::Equal {
                                return ord;
                            }
                        }
                        std::cmp::Ordering::Equal
                    });
                    rows.truncate(topn.get_limit() as usize);
                }
                other => panic!("MockClient: unsupported pushed-down executor {other:?}"),
            }
        }

        let out_offsets: Vec<usize> = dag
            .get_output_offsets()
            .iter()
            .map(|&o| o as usize)
            .collect();
        let out_fts: Vec<tidb_parser::types::FieldType> =
            out_offsets.iter().map(|&o| col_fts[o].clone()).collect();
        let mut resp = tipb::SelectResponse::default();
        resp.set_encode_type(dag.get_encode_type());
        if !rows.is_empty() {
            let mut pb_chunk = tipb::Chunk::default();
            if dag.get_encode_type() == tipb::EncodeType::TypeChunk {
                let mut chk = tidb_chunk::chunk::new_chunk_with_capacity(&out_fts, rows.len());
                for r in &rows {
                    for (j, &off) in out_offsets.iter().enumerate() {
                        chk.append_datum(j, &r[off]);
                    }
                }
                let codec = tidb_chunk::codec::new_codec(out_fts);
                pb_chunk.set_rows_data(codec.encode(&chk));
            } else {
                let mut buf = Vec::new();
                for r in &rows {
                    for &off in &out_offsets {
                        tidb_codec::codec::encode_value(
                            &tidb_base::gostd::time::utc(),
                            &mut buf,
                            &[r[off].clone()],
                        )
                        .unwrap();
                    }
                }
                pb_chunk.set_rows_data(buf);
            }
            resp.mut_chunks().push(pb_chunk);
        }
        let bytes = resp.write_to_bytes().unwrap();
        Box::new(MockCopResponse { data: Some(bytes) })
    }
    fn is_request_type_supported(&self, _: i64, _: i64) -> bool {
        true
    }
}

// ---------------------------------------------------------------- sessionexpr (spec §8)

/// Go `TestSessionEvalContextBasic`, on a view built after the variables are set (the
/// scalar variables are a snapshot, spec §8.1): the fields, and the warnings, which go to the
/// statement context and back.
#[test]
fn session_eval_context_basic() {
    use tidb_base::contextutil::{WarnAppender, WarnHandler, WarnLevelWarning};
    use tidb_sessionctx::exprctx::EvalContext as _;
    let store = MockStore::new(10);
    let mut s = session(&store);
    s.vars_mut().sql_mode =
        (tidb_base::mysql::ModeStrictTransTables | tidb_base::mysql::ModeNoZeroDate).0;
    s.vars_mut().current_db = "db1".to_owned();
    s.vars_mut().max_allowed_packet = 123456;
    let ctx = s.core.sctx.expr_ctx();
    let imp = ctx.eval();
    assert_eq!(imp.sql_mode().0, s.vars().sql_mode);
    assert_eq!(imp.current_db(), "db1");
    assert_eq!(imp.get_max_allowed_packet(), 123456);
    assert_eq!(
        imp.location().string(),
        s.vars().stmt_ctx().time_zone().string()
    );
    assert_eq!(imp.get_default_week_format_mode(), "0");
    s.vars_mut()
        .set_system_var("default_week_format", "5")
        .unwrap();
    assert_eq!(
        s.core.sctx.expr_ctx().eval().get_default_week_format_mode(),
        "5"
    );
    assert!(Arc::ptr_eq(
        &(imp.get_user_vars_reader()),
        &(Arc::clone(&s.vars().state.user_vars)
            as tidb_sessionctx::variable::uservars::UserVarsReaderRef)
    ));

    // handle warnings: the statement context's handler, both ways.
    assert_eq!(imp.warning_count(), 0);
    imp.append_warning(Error::plain("err1"));
    assert_eq!(imp.warning_count(), 1);
    s.vars().stmt_ctx().append_warning(Error::plain("err2"));
    assert_eq!(imp.warning_count(), 2);
    assert_eq!(s.vars().stmt_ctx().warning_count(), 2);
    let warnings = imp.copy_warnings(Vec::new());
    assert_eq!(warnings.len(), 2);
    assert!(warnings.iter().all(|w| w.level == WarnLevelWarning));
    let warnings = imp.truncate_warnings(1);
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].err.to_string(), "err2");
    assert_eq!(imp.warning_count(), 1);
}

#[test]
fn session_get_dist_sql_ctx_caches_and_resets_on_retry() {
    let store = MockStore::new(10);
    let mut s = session(&store);
    s.vars_mut().enable_chunk_rpc = true;
    s.vars_mut().set_dist_sql_scan_concurrency(7);
    s.vars_mut().stmt_ctx_mut().task_id = 42;

    let dctx1 = s.core.get_dist_sql_ctx();
    let dctx2 = s.core.get_dist_sql_ctx();
    assert!(Arc::ptr_eq(&dctx1, &dctx2));
    assert_eq!(dctx1.dist_sql_concurrency, 7);
    assert_eq!(dctx1.task_id, 42);
    assert!(dctx1.enable_chunk_rpc);
    assert!(!dctx1.has_runaway_rule);
    assert!(dctx1.resource_group_tagger.is_some());
    assert!(Arc::ptr_eq(
        dctx1.kv_vars.killed.as_ref().unwrap(),
        &s.core.sctx.killer.signal_handle()
    ));

    dctx1.append_warning(Error::plain("distsql warn"));
    assert_eq!(s.vars().stmt_ctx().warning_count(), 1);

    // Drop shared Arcs so reset_for_retry can mutate StmtCtx, then verify cache invalidates.
    let old_task_id = dctx1.task_id;
    drop(dctx1);
    drop(dctx2);
    s.vars_mut().stmt_ctx_mut().reset_for_retry();
    let dctx3 = s.core.get_dist_sql_ctx();
    assert_ne!(dctx3.task_id, old_task_id);
}

/// Go `TestSessionEvalContextCurrentTime`: the stale read TSO (evaluated once), else the
/// `timestamp` variable, else the current time.
#[test]
fn session_eval_context_current_time() {
    use tidb_sessionctx::exprctx::EvalContext as _;
    let store = MockStore::new(10);
    let mut s = session(&store);
    let calls = Arc::new(AtomicUsize::new(0));
    let c = Arc::clone(&calls);
    s.vars()
        .stmt_ctx()
        .set_stale_tso_provider_if_not_exist(Box::new(move || {
            c.fetch_add(1, SeqCst);
            // oracle.GoTimeToTS(time.UnixMilli(123456789))
            Ok(123_456_789u64 << 18)
        }));
    let imp = s.core.sctx.expr_ctx();
    for _ in 0..2 {
        let tm = imp.eval().current_time().unwrap();
        assert_eq!((tm.unix(), tm.nanosecond()), (123_456, 789_000_000));
    }
    assert_eq!(calls.load(SeqCst), 1);

    // The "timestamp" variable when it is set.
    s.vars_mut().stmt_ctx_mut().reset();
    s.vars_mut()
        .set_system_var("timestamp", "7654321.875")
        .unwrap();
    let tm = s.core.sctx.expr_ctx().eval().current_time().unwrap();
    assert_eq!((tm.unix(), tm.nanosecond()), (7_654_321, 875_000_000));

    // The current time otherwise.
    s.vars_mut().set_system_var("timestamp", "0").unwrap();
    let tm = s.core.sctx.expr_ctx().eval().current_time().unwrap();
    let now = tidb_base::gostd::time::now().unix();
    assert!((tm.unix() - now).abs() <= 5, "{} {now}", tm.unix());
}

/// Go `TestSessionEvalContextPrivilegeCheck` (no privilege manager: every check passes) and
/// `TestSessionEvalContextOptProps`: all nine properties exist, the five the session cannot
/// back answer a boundary error (spec §8.3).
#[test]
fn session_eval_context_privileges_and_optional_props() {
    use tidb_sessionctx::exprctx::{
        EvalContext as _, OptPropAdvisoryLock, OptPropCurrentUser, OptPropDDLOwnerInfo,
        OptPropInfoSchema, OptPropKVStore, OptPropPrivilegeChecker, OptPropSQLExecutor,
        OptPropSequenceOperator, OptPropSessionVars,
    };
    use tidb_sessionctx::expropt::{
        AdvisoryLockPropReader, CurrentUserPropReader, DDLOwnerPropReader, InfoSchemaPropReader,
        PrivilegeChecker, PrivilegeCheckerPropReader, SQLExecutorPropReader,
        SequenceOperatorPropReader,
    };
    let store = MockStore::new(10);
    let s = sbtest_session(&store);
    let ctx = s.core.sctx.expr_ctx();
    let imp = ctx.eval();
    assert!(imp.request_verification("test", "tbl1", "col1", tidb_base::mysql::SuperPriv));
    assert!(imp.request_dynamic_verification("RESTRICTED_TABLES_ADMIN", true));

    // Every property exists (Go code branches on that), as in Go.
    let set = imp.get_optional_prop_set();
    assert!(set.is_full());
    for key in [
        OptPropInfoSchema,
        OptPropKVStore,
        OptPropSQLExecutor,
        OptPropSequenceOperator,
        OptPropDDLOwnerInfo,
        OptPropPrivilegeChecker,
        OptPropCurrentUser,
        OptPropSessionVars,
        OptPropAdvisoryLock,
    ] {
        assert!(set.contains(key), "{key}");
        assert_eq!(
            imp.get_optional_prop_provider(key).unwrap().desc().key(),
            key
        );
    }
    let dyn_ctx: &dyn tidb_sessionctx::exprctx::EvalContext = &**imp;
    // The ones the session cannot back answer a boundary error when taken.
    let e = CurrentUserPropReader.current_user(dyn_ctx).err().unwrap();
    assert_eq!(blocked(&e), Some("D04.1"));
    let e = AdvisoryLockPropReader
        .advisory_lock_ctx(dyn_ctx)
        .err()
        .unwrap();
    assert_eq!(blocked(&e), Some("D05.3"));
    let e = tidb_sessionctx::expropt::get_prop_provider::<()>(dyn_ctx, OptPropSessionVars)
        .err()
        .unwrap();
    assert_eq!(blocked(&e), Some("D05.3"));
    assert_eq!(
        InfoSchemaPropReader
            .get_session_info_schema(dyn_ctx)
            .unwrap()
            .unwrap()
            .schema_meta_version(),
        1
    );
    assert!(!DDLOwnerPropReader.is_ddl_owner(dyn_ctx).unwrap());
    let checker = PrivilegeCheckerPropReader
        .get_privilege_checker(dyn_ctx)
        .unwrap();
    assert!(checker.request_verification("db", "t", "c", tidb_base::mysql::CreatePriv));
    let e = SQLExecutorPropReader
        .get_sql_executor(dyn_ctx)
        .err()
        .unwrap();
    assert_eq!(blocked(&e), Some("D00.3"));
    let e = SequenceOperatorPropReader
        .get_sequence_operator(dyn_ctx, "test", "seq")
        .err()
        .unwrap();
    assert_eq!(blocked(&e), Some("D06.4"));
}

/// Go `TestSessionBuildContext`: the build context's variables, and the parts it shares with
/// the session (the random generator, the plan cache tracker, the plan column ids, the
/// parameter values) read and write the session's own.
#[test]
fn session_build_context_shares_the_session_state() {
    use tidb_sessionctx::exprctx::{BuildContext, ParamValues, StaticConvertibleExprContext};
    let store = MockStore::new(10);
    let mut s = session(&store);
    s.vars_mut().default_collation_for_utf8mb4 = "utf8mb4_0900_ai_ci".to_owned();
    s.vars_mut().sysdate_is_now = true;
    s.vars_mut().noop_funcs_mode = 2;
    s.core.sctx.set_connection_id(123);
    let imp = s.core.sctx.expr_ctx();
    assert_eq!(imp.get_charset_info(), s.vars().get_charset_info());
    assert_eq!(
        imp.get_default_collation_for_utf8mb4(),
        "utf8mb4_0900_ai_ci"
    );
    assert!(imp.get_sysdate_is_now());
    assert_eq!(imp.get_noop_funcs_mode(), 2);
    assert_eq!(imp.connection_id(), 123);
    assert!(!imp.is_in_null_reject_check());
    assert!(!imp.is_constant_propagate_check());

    // Rng: the session's generator.
    assert!(Arc::ptr_eq(&imp.rng(), &s.vars().state.rng.share()));

    // PlanCache: the statement's tracker.
    s.vars().stmt_ctx().plan_cache_tracker.enable_plan_cache();
    assert!(imp.is_use_cache());
    imp.set_skip_plan_cache("mockReason");
    assert!(!imp.is_use_cache());
    assert!(!s.vars().stmt_ctx().plan_cache_tracker.use_cache());

    // Alloc column id: the session's counter.
    let prev = s.vars().state.plan_column_id.0.load(SeqCst);
    assert_eq!(imp.alloc_plan_column_id(), prev + 1);
    assert_eq!(imp.alloc_plan_column_id(), prev + 2);
    s.vars().alloc_plan_column_id();
    assert_eq!(imp.alloc_plan_column_id(), prev + 4);
    assert_eq!(imp.get_last_plan_column_id(), prev + 4);

    // The parameter values as they are now (set after the context was built).
    assert!(imp.eval().get_param_value(0).is_err());
    s.vars()
        .state
        .plan_cache_params
        .lock()
        .append(&[tidb_types::datum::new_int_datum(9)]);
    assert_eq!(imp.eval().get_param_value(0).unwrap().get_int64(), 9);

    // IsReadonlyUserVar: the map the preprocessor handed the session; Reset forgets it.
    use tidb_executor::glue::SessionContext;
    s.core
        .set_readonly_user_var_map(std::collections::HashSet::from(["a".to_owned()]));
    assert!(s.core.sctx.expr_ctx().is_readonly_user_var("a"));
    assert!(!s.core.sctx.expr_ctx().is_readonly_user_var("b"));
    s.core.reset_plan_ctx();
    assert!(!s.core.sctx.expr_ctx().is_readonly_user_var("a"));

    // IntoStatic keeps the values.
    let st = imp.into_static();
    assert_eq!(st.connection_id(), 123);
}

/// Restoring a checkpoint writes the saved values back into the cells the session shares
/// with its expression contexts (parameters, plan column ids, `RAND()` generator, user
/// variables): a context built before the statement sees the restored values.
#[test]
fn restore_writes_back_into_the_shared_cells() {
    use tidb_sessionctx::exprctx::{
        BuildContext, EvalContext as _, ParamValues, StaticConvertibleExprContext,
    };
    let store = MockStore::new(10);
    let mut s = session(&store);
    s.vars()
        .state
        .user_vars
        .set_user_var_val("a", tidb_types::datum::new_int_datum(1));
    let holder = s.core.sctx.expr_ctx();
    let rng_before = s.vars().state.rng.lock().clone();
    let col_before = holder.alloc_plan_column_id();
    let cp = s.checkpoint();
    // What a statement changes before it falls back.
    s.vars()
        .state
        .plan_cache_params
        .lock()
        .append(&[tidb_types::datum::new_int_datum(5)]);
    holder.alloc_plan_column_id();
    holder.rng().lock().r#gen();
    s.vars()
        .state
        .user_vars
        .set_user_var_val("a", tidb_types::datum::new_int_datum(2));
    s.restore(cp).unwrap();
    // The holder's cells are still the session's, with the values of the checkpoint.
    assert!(Arc::ptr_eq(&holder.rng(), &s.vars().state.rng.share()));
    assert_eq!(*holder.rng().lock(), rng_before);
    assert!(holder.eval().get_param_value(0).is_err());
    assert_eq!(holder.get_last_plan_column_id(), col_before);
    assert_eq!(s.vars().state.plan_column_id.0.load(SeqCst), col_before);
    let a = holder
        .eval()
        .get_user_vars_reader()
        .get_user_var_val("a")
        .unwrap();
    assert_eq!(a.get_int64(), 1);
}

// ---------------------------------------------------------------- instance plan cache

/// An InfoSchema (version 1) with `test.uk1 (id BIGINT PRIMARY KEY, k INT NOT NULL, c CHAR,
/// UNIQUE KEY uk(k))`.
fn uk_info_schema() -> Arc<tidb_infoschema::InfoSchemaV2> {
    use tidb_model::{ColumnInfo, DBInfo, IndexColumn, IndexInfo, StatePublic, TableInfo};
    use tidb_parser::model::CIStr;
    use tidb_parser::types::FieldType;
    let col = |id: i64, name: &str, tp: u8, flag: usize| {
        let mut field_type = FieldType::new(tp);
        field_type.add_flag(flag);
        Arc::new(ColumnInfo {
            id,
            name: CIStr::new(name),
            offset: (id - 1) as isize,
            field_type,
            state: StatePublic,
            ..Default::default()
        })
    };
    use tidb_base::mysql::{NotNullFlag, PriKeyFlag, UniqueKeyFlag};
    let t = TableInfo {
        id: 111,
        dbid: 2,
        name: CIStr::new("uk1"),
        columns: Some(vec![
            col(
                1,
                "id",
                tidb_base::mysql::TypeLonglong,
                PriKeyFlag | NotNullFlag,
            ),
            col(
                2,
                "k",
                tidb_base::mysql::TypeLong,
                UniqueKeyFlag | NotNullFlag,
            ),
            col(3, "c", tidb_base::mysql::TypeString, 0),
        ]),
        indices: Some(vec![Arc::new(IndexInfo {
            id: 1,
            name: CIStr::new("uk"),
            table: CIStr::new("uk1"),
            columns: Some(vec![IndexColumn {
                name: CIStr::new("k"),
                offset: 1,
                length: -1,
                ..Default::default()
            }]),
            state: StatePublic,
            unique: true,
            ..Default::default()
        })]),
        pk_is_handle: true,
        state: StatePublic,
        version: 5,
        max_column_id: 3,
        max_index_id: 1,
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

/// A session over `store` and `is` that uses `cache`, with the prepared plan cache on
/// (Go's default; the test sessions do not load the global values).
fn cached_session(
    store: &Arc<MockStore>,
    is: Arc<tidb_infoschema::InfoSchemaV2>,
    cache: &Arc<dyn tidb_sessionctx::context::InstancePlanCache>,
) -> Session {
    tidb_sessionctx::variable::tidb_vars::EnableMDL.store(true, SeqCst);
    let mut s = session(store);
    s.set_domain_info_schema(is);
    s.vars_mut().current_db = "test".to_owned();
    s.vars_mut().enable_prepared_plan_cache = true;
    s.set_instance_plan_cache(Some(Arc::clone(cache)));
    s
}

/// Runs a prepared statement to the end; returns `FoundInPlanCache`.
fn execute_to_end(s: &mut Session, stmt_id: u32, param: tidb_types::datum::Datum) -> bool {
    let mut rs = block_on(s.execute_prepared_stmt(stmt_id, vec![param]))
        .unwrap()
        .expect("a result set");
    let found = s.vars().found_in_plan_cache;
    let mut chk = rs.new_chunk().unwrap();
    block_on(rs.next(s, &mut chk)).unwrap();
    block_on(rs.close(s)).unwrap();
    assert!(s.is_idle(), "{}", s.state_snapshot());
    found
}

/// The structural hashes of every plan in the cache.
fn cached_hashes(cache: &Arc<dyn tidb_sessionctx::context::InstancePlanCache>) -> Vec<u64> {
    let c = cache
        .as_any()
        .downcast_ref::<tidb_planner::core::plan_cache_instance::InstancePlanCacheImpl>()
        .unwrap();
    let mut v: Vec<u64> = c
        .values()
        .iter()
        .map(|v| tidb_planner::core::plan_cache_heap_size::plan_structure_hash(v.plan.as_ref()))
        .collect();
    v.sort_unstable();
    v
}

fn new_test_cache() -> Arc<dyn tidb_sessionctx::context::InstancePlanCache> {
    Arc::new(tidb_planner::core::plan_cache_instance::new_instance_plan_cache(90 << 20, 100 << 20))
}

/// The instance plan cache (the plan-cache exception A3, first stage) on the sysbench point
/// select: the first EXECUTE optimizes and puts the plan, the next ones hit, also in another
/// session (the cache is per instance); a hit with another parameter copies only the point
/// get (path copying), one with the cached parameter copies nothing; executing the cached
/// plan leaves it unchanged (structural hash).
#[test]
fn instance_plan_cache_point_get_is_shared_by_sessions() {
    let _serial = PREPARED_COUNT.lock();
    let store = MockStore::new(10);
    let cache = new_test_cache();
    let mut a = cached_session(&store, sbtest_info_schema(), &cache);
    let mut b = cached_session(&store, sbtest_info_schema(), &cache);
    let int = tidb_types::datum::new_int_datum;

    let pa = block_on(a.prepare_stmt(SBTEST_POINT)).unwrap();
    assert!(
        !execute_to_end(&mut a, pa.stmt_id, int(1)),
        "the first EXECUTE optimizes"
    );
    assert_eq!(cache.size(), 1);
    assert!(cache.mem_usage() > 0);
    let before = cached_hashes(&cache);

    let copies = tidb_planner::physicalop::clone_node_calls();
    assert!(
        execute_to_end(&mut a, pa.stmt_id, int(1)),
        "same parameter: a hit"
    );
    assert_eq!(tidb_planner::physicalop::clone_node_calls() - copies, 0);
    let copies = tidb_planner::physicalop::clone_node_calls();
    assert!(
        execute_to_end(&mut a, pa.stmt_id, int(2)),
        "another parameter: a hit"
    );
    assert_eq!(
        tidb_planner::physicalop::clone_node_calls() - copies,
        1,
        "the point get only"
    );

    // Another session: its first EXECUTE hits the plan the first session put.
    let pb = block_on(b.prepare_stmt(SBTEST_POINT)).unwrap();
    assert!(execute_to_end(&mut b, pb.stmt_id, int(3)));
    assert_eq!(cache.size(), 1);
    assert_eq!(
        cached_hashes(&cache),
        before,
        "the cached plan is never written"
    );

    // A parameter of another type is another plan of the same key.
    assert!(!execute_to_end(
        &mut b,
        pb.stmt_id,
        tidb_types::datum::new_string_datum("4")
    ));
    assert_eq!(cache.size(), 2);

    // ADMIN FLUSH INSTANCE PLAN_CACHE (A3 decision 8): the next EXECUTE optimizes again.
    assert_eq!(cache.delete_all(), 2);
    assert!(!execute_to_end(&mut a, pa.stmt_id, int(1)));
    assert!(execute_to_end(&mut b, pb.stmt_id, int(1)));

    // tidb_enable_prepared_plan_cache = OFF in a session: no lookup there.
    b.vars_mut().enable_prepared_plan_cache = false;
    assert!(!execute_to_end(&mut b, pb.stmt_id, int(1)));
    a.drop_prepared_stmt(pa.stmt_id).unwrap();
    b.drop_prepared_stmt(pb.stmt_id).unwrap();
}

/// A3 spec §2.3 row 9a: the unique-index point get executor converts the index values into
/// its own vector (`EncodeUniqueIndexValuesForKey`), never into the cached plan's
/// `IndexValues`: after executing a cached plan with a parameter that needs a conversion
/// (a string for an INT column), the cached plan's structural hash is unchanged.
#[test]
fn unique_index_point_get_does_not_write_the_cached_index_values() {
    let _serial = PREPARED_COUNT.lock();
    let store = MockStore::new(10);
    let cache = new_test_cache();
    let mut s = cached_session(&store, uk_info_schema(), &cache);
    let p = block_on(s.prepare_stmt("SELECT c FROM uk1 WHERE k = ?")).unwrap();
    let int = tidb_types::datum::new_int_datum;
    assert!(!execute_to_end(&mut s, p.stmt_id, int(7)));
    assert_eq!(cache.size(), 1, "a unique-index point get is cacheable");
    let before = cached_hashes(&cache);
    assert!(execute_to_end(&mut s, p.stmt_id, int(8)));
    assert!(execute_to_end(&mut s, p.stmt_id, int(7)));
    assert_eq!(cached_hashes(&cache), before);
    // A string parameter: another plan; executing it twice leaves both plans unchanged.
    let str_ = tidb_types::datum::new_string_datum;
    assert!(!execute_to_end(&mut s, p.stmt_id, str_("9")));
    let before = cached_hashes(&cache);
    assert!(execute_to_end(&mut s, p.stmt_id, str_("10")));
    assert_eq!(cached_hashes(&cache), before);
    s.drop_prepared_stmt(p.stmt_id).unwrap();
}

/// Counts the bytes this thread frees while [`FREED`] is armed (the accuracy test of the
/// plan cache's memory accounting).
struct CountingAlloc;

thread_local! {
    static FREED: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

// SAFETY: forwards every call to the system allocator unchanged.
#[allow(unsafe_code)]
unsafe impl std::alloc::GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        // SAFETY: the caller's contract is the system allocator's.
        unsafe { std::alloc::System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        let _ = FREED.try_with(|f| {
            if let Some(n) = f.get() {
                f.set(Some(n + layout.size() as u64));
            }
        });
        // SAFETY: the caller's contract is the system allocator's.
        unsafe { std::alloc::System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

/// A3 spec §3.5, §5.3: the bytes an entry is charged (`PlanCacheValue.MemoryUsage`) are the
/// bytes freed when its last reference goes (the InfoSchema, whose metadata the plan shares
/// and is not charged for, stays alive), within 10% (spec §5.3).
#[test]
fn plan_cache_entry_memory_is_what_its_drop_frees() {
    let _serial = PREPARED_COUNT.lock();
    let store = MockStore::new(10);
    for (is, sql) in [
        (sbtest_info_schema(), SBTEST_POINT),
        (uk_info_schema(), "SELECT c FROM uk1 WHERE k = ?"),
    ] {
        let cache = new_test_cache();
        // The InfoSchema outlives the entry: its metadata (TableInfo, ...) is not the entry's.
        let mut s = cached_session(&store, Arc::clone(&is), &cache);
        let p = block_on(s.prepare_stmt(sql)).unwrap();
        execute_to_end(&mut s, p.stmt_id, tidb_types::datum::new_int_datum(1));
        let values = cache
            .as_any()
            .downcast_ref::<tidb_planner::core::plan_cache_instance::InstancePlanCacheImpl>()
            .unwrap()
            .values();
        assert_eq!(values.len(), 1);
        drop(s);
        cache.delete_all();
        let v = values.into_iter().next().unwrap();
        assert_eq!(Arc::strong_count(&v), 1, "{sql}");
        assert_eq!(Arc::strong_count(&v.plan), 1, "{sql}");
        let charged = v.memory_usage() as f64;
        FREED.with(|f| f.set(Some(0)));
        drop(v);
        let freed = FREED.with(|f| f.replace(None)).unwrap() as f64;
        eprintln!("{sql}: charged {charged} bytes, freed {freed} bytes");
        assert!(
            (charged - freed).abs() <= 0.10 * freed,
            "{sql}: charged {charged}, freed {freed}"
        );
    }
}

// ------------------------------------------------- expression column names (statement text)

/// Runs a statement through the session's steps up to and including `Compile` (the same
/// steps as `ExecuteStmt`), ends its transaction as a failed statement does, and returns the
/// names of the result columns (`ExecStmt.OutputNames`, whose `ColName` the result fields
/// take as their names). The names are fixed by the planner, before any executor is built;
/// the same names read from the record sets of these statements, which now run (the basic
/// executors, D12.2), are checked in `sbtest_results`.
fn compiled_column_names(s: &mut Session, node: ast::Stmt, text: &str) -> Vec<String> {
    block_on(s.finish_unfinished_stmt());
    block_on(s.core.prepare_txn_ctx()).unwrap();
    reset_context_of_stmt(&mut s.core, &node, text).unwrap();
    s.core
        .txn_manager
        .on_stmt_start(&mut s.core.sctx, Some(&node))
        .unwrap();
    let text = s.vars().stmt_ctx().original_sql.clone();
    let stmt = tidb_executor::compiler::Compiler
        .compile(&mut s.core, node, &text)
        .unwrap_or_else(|e| panic!("{text}: {e}"));
    s.core.rollback_on_error();
    s.core.txn_manager.on_stmt_end();
    assert!(s.is_idle(), "{text}: {}", s.state_snapshot());
    stmt.output_names
        .iter()
        .map(|n| n.col_name.o.clone())
        .collect()
}

/// The EXECUTE node of the prepared statement `stmt_id`, as `ExecutePreparedStmt` builds it.
fn execute_node(stmt_id: u32, s: &Session) -> ast::Stmt {
    let prep_stmt = s.vars().get_prepared_stmt_by_id(stmt_id).unwrap();
    ast::Stmt::Execute(Box::new(ast::ExecuteStmt {
        binary_args: Some(Arc::new(Vec::<tidb_types::datum::Datum>::new())),
        prep_stmt: Some(prep_stmt),
        prep_stmt_id: stmt_id,
        ..Default::default()
    }))
}

/// The name of an expression column is the text of its select field, as Go v8.5.8 sends it
/// (`buildProjectionFieldNameFromExpressions` takes `SelectField.Text()`,
/// `pkg/planner/core/logical_plan_builder.go:1462`): on the text protocol, at PREPARE, and
/// at EXECUTE both when the plan cache misses (the prepared statement is planned again from
/// its text) and when it hits (the names cached by the miss). The Rust AST keeps byte spans
/// into the statement's text, which the node wrapper carries to the planner (D09.13 spec
/// §5.1); without it the text protocol and EXECUTE gave empty names. The expected names are
/// what a v8.5.8 server sent for each statement (text, `COM_STMT_PREPARE`,
/// `COM_STMT_EXECUTE`); that server also missed the plan cache on the first EXECUTE and hit
/// it on the second (`@@last_plan_from_cache` 0, then 1), as the session does here.
#[test]
fn expression_columns_are_named_by_their_text() {
    let _serial = PREPARED_COUNT.lock();
    let store = MockStore::new(10);
    let cache = new_test_cache();
    let mut s = cached_session(&store, sbtest_info_schema(), &cache);
    for (sql, names) in [
        ("SELECT 1", vec!["1"]),
        ("SELECT -42", vec!["-42"]),
        ("SELECT 1, 2, 3", vec!["1", "2", "3"]),
        ("SELECT 1 + 1", vec!["1 + 1"]),
        // Leading whitespace and a comment are not part of the select field's text.
        ("  /* x */ SELECT 1 + 1", vec!["1 + 1"]),
        ("SELECT 1.5", vec!["1.5"]),
        ("SELECT 1 LIMIT 1", vec!["1"]),
        ("SELECT id + 1 FROM sbtest1 WHERE id = 1", vec!["id + 1"]),
        ("SELECT c, 1 FROM sbtest1 WHERE id = 1", vec!["c", "1"]),
    ] {
        let got = compiled_column_names(&mut s, parse(sql), sql);
        assert_eq!(got, names, "text: {sql}");

        let info = block_on(s.prepare_stmt(sql)).unwrap();
        let got: Vec<&str> = info
            .fields
            .iter()
            .map(|f| f.column_as_name.o.as_str())
            .collect();
        assert_eq!(got, names, "PREPARE: {sql}");

        let node = execute_node(info.stmt_id, &s);
        let got = compiled_column_names(&mut s, node, "");
        assert!(
            !s.vars().found_in_plan_cache,
            "the first EXECUTE misses: {sql}"
        );
        assert_eq!(got, names, "EXECUTE (miss): {sql}");

        let node = execute_node(info.stmt_id, &s);
        let got = compiled_column_names(&mut s, node, "");
        assert!(
            s.vars().found_in_plan_cache,
            "the second EXECUTE hits (as in Go): {sql}"
        );
        assert_eq!(got, names, "EXECUTE (hit): {sql}");
        s.drop_prepared_stmt(info.stmt_id).unwrap();
        s.clean_closed_prepared_stmts();
    }
}

/// Full `sbtest1` schema matching sysbench (`id INT NOT NULL AUTO_INCREMENT, k INT NOT NULL
/// DEFAULT '0', c CHAR(120), pad CHAR(60), PRIMARY KEY (id), KEY k_1 (k)`) plus a single-path
/// clustered composite-PK table `tc_pk (a INT, b INT, c INT, PRIMARY KEY (a, b) CLUSTERED)`.
fn sbtest_with_secondary_index_info_schema() -> Arc<tidb_infoschema::InfoSchemaV2> {
    use tidb_base::mysql::{
        AutoIncrementFlag, MultipleKeyFlag, NotNullFlag, PriKeyFlag, TypeLong, TypeString,
    };
    use tidb_model::{ColumnInfo, DBInfo, IndexColumn, IndexInfo, StatePublic, TableInfo};
    use tidb_parser::model::CIStr;
    use tidb_parser::types::{FieldType, UnspecifiedLength};

    let mk_int = |id: i64, name: &str, offset: isize, flags: usize| {
        let mut ft = FieldType::new(TypeLong);
        ft.set_flen(11);
        ft.set_decimal(0);
        ft.set_charset("binary");
        ft.set_collate("binary");
        ft.add_flag(flags);
        Arc::new(ColumnInfo {
            id,
            name: CIStr::new(name),
            offset,
            field_type: ft,
            state: StatePublic,
            ..Default::default()
        })
    };
    let mk_char = |id: i64, name: &str, offset: isize, flen: isize| {
        let mut ft = FieldType::new(TypeString);
        ft.set_flen(flen);
        ft.set_decimal(0);
        ft.set_charset("utf8mb4");
        ft.set_collate("utf8mb4_bin");
        ft.add_flag(NotNullFlag);
        Arc::new(ColumnInfo {
            id,
            name: CIStr::new(name),
            offset,
            field_type: ft,
            state: StatePublic,
            ..Default::default()
        })
    };

    let sbtest1 = TableInfo {
        id: 110,
        dbid: 2,
        name: CIStr::new("sbtest1"),
        columns: Some(vec![
            mk_int(1, "id", 0, NotNullFlag | PriKeyFlag | AutoIncrementFlag),
            mk_int(2, "k", 1, NotNullFlag | MultipleKeyFlag),
            mk_char(3, "c", 2, 120),
            mk_char(4, "pad", 3, 60),
        ]),
        indices: Some(vec![Arc::new(IndexInfo {
            id: 1,
            name: CIStr::new("k_1"),
            table: CIStr::new("sbtest1"),
            columns: Some(vec![IndexColumn {
                name: CIStr::new("k"),
                offset: 1,
                length: UnspecifiedLength,
                ..Default::default()
            }]),
            state: StatePublic,
            ..Default::default()
        })]),
        pk_is_handle: true,
        state: StatePublic,
        version: 5,
        max_column_id: 4,
        max_index_id: 1,
        ..Default::default()
    };

    let tc_pk = TableInfo {
        id: 111,
        dbid: 2,
        name: CIStr::new("tc_pk"),
        columns: Some(vec![
            mk_int(1, "a", 0, NotNullFlag | PriKeyFlag),
            mk_int(2, "b", 1, NotNullFlag | PriKeyFlag),
            mk_int(3, "c", 2, 0),
        ]),
        indices: Some(vec![Arc::new(IndexInfo {
            id: 1,
            name: CIStr::new("PRIMARY"),
            table: CIStr::new("tc_pk"),
            primary: true,
            unique: true,
            columns: Some(vec![
                IndexColumn {
                    name: CIStr::new("a"),
                    offset: 0,
                    length: UnspecifiedLength,
                    ..Default::default()
                },
                IndexColumn {
                    name: CIStr::new("b"),
                    offset: 1,
                    length: UnspecifiedLength,
                    ..Default::default()
                },
            ]),
            state: StatePublic,
            ..Default::default()
        })]),
        pk_is_handle: false,
        is_common_handle: true,
        common_handle_version: 1,
        state: StatePublic,
        version: 5,
        max_column_id: 3,
        max_index_id: 1,
        ..Default::default()
    };

    let mut db = DBInfo {
        id: 2,
        name: CIStr::new("test"),
        state: StatePublic,
        ..Default::default()
    };
    db.deprecated.tables = Some(vec![Arc::new(sbtest1), Arc::new(tc_pk)]);
    let mut b = tidb_infoschema::new_builder(None, None, tidb_infoschema::new_data());
    block_on(b.init_with_db_infos(vec![db], vec![], vec![], 1)).unwrap();
    Arc::new(block_on(b.build(1)))
}

/// Pinned field-by-field against live Go TiDB v8.5.8 `COM_STMT_PREPARE` column definitions
/// for single-read `IndexReader` queries on `sbtest1`.
#[test]
fn prepare_index_reader_column_metadata_matches_tidb_v858() {
    let _serial = PREPARED_COUNT.lock();
    let store = MockStore::new(10);
    let cache = new_test_cache();
    let mut s = cached_session(&store, sbtest_with_secondary_index_info_schema(), &cache);

    // Live Go TiDB v8.5.8 capture for `SELECT k FROM sbtest1 WHERE k BETWEEN ? AND ?`:
    //   num_params=2, num_cols=1
    //   col[0]: schema=test, table=sbtest1, org_table=sbtest1, name=k, org_name=k,
    //           charset=63 (binary), flen=11, type=3 (TypeLong), flags=9, decimal=0
    let info1 = block_on(s.prepare_stmt("SELECT k FROM sbtest1 WHERE k BETWEEN ? AND ?")).unwrap();
    assert_eq!(info1.param_count, 2);
    assert_eq!(info1.fields.len(), 1);
    let f = &info1.fields[0];
    assert_eq!(f.db_name.o, "test");
    assert_eq!(f.table_name.o, "sbtest1");
    assert_eq!(f.table_as_name.o, "sbtest1");
    assert_eq!(f.column_name.o, "k");
    assert_eq!(f.column_as_name.o, "k");
    assert_eq!(f.column_type.get_type(), 3);
    assert_eq!(f.column_type.get_flag(), 9);
    assert_eq!(f.column_type.get_flen(), 11);
    assert_eq!(f.column_type.get_decimal(), 0);
    assert_eq!(f.column_type.get_charset(), "binary");
    assert_eq!(f.column_type.get_collate(), "binary");
    s.drop_prepared_stmt(info1.stmt_id).unwrap();

    // Live Go TiDB v8.5.8 capture for `SELECT id, k FROM sbtest1 WHERE k BETWEEN ? AND ?`:
    //   num_params=2, num_cols=2
    //   col[0]: name=id, org_name=id, type=3, flags=515, flen=11, decimal=0, binary
    //   col[1]: name=k,  org_name=k,  type=3, flags=9,   flen=11, decimal=0, binary
    let info2 =
        block_on(s.prepare_stmt("SELECT id, k FROM sbtest1 WHERE k BETWEEN ? AND ?")).unwrap();
    assert_eq!(info2.param_count, 2);
    assert_eq!(info2.fields.len(), 2);
    let f0 = &info2.fields[0];
    assert_eq!(f0.db_name.o, "test");
    assert_eq!(f0.table_name.o, "sbtest1");
    assert_eq!(f0.table_as_name.o, "sbtest1");
    assert_eq!(f0.column_name.o, "id");
    assert_eq!(f0.column_as_name.o, "id");
    assert_eq!(f0.column_type.get_type(), 3);
    assert_eq!(f0.column_type.get_flag(), 515);
    assert_eq!(f0.column_type.get_flen(), 11);
    assert_eq!(f0.column_type.get_decimal(), 0);
    assert_eq!(f0.column_type.get_charset(), "binary");
    assert_eq!(f0.column_type.get_collate(), "binary");
    let f1 = &info2.fields[1];
    assert_eq!(f1.column_name.o, "k");
    assert_eq!(f1.column_as_name.o, "k");
    assert_eq!(f1.column_type.get_type(), 3);
    assert_eq!(f1.column_type.get_flag(), 9);
    s.drop_prepared_stmt(info2.stmt_id).unwrap();
    s.clean_closed_prepared_stmts();
    assert!(s.is_idle(), "{}", s.state_snapshot());
}

/// Executing a covering `IndexReader` query plans a `PhysicalIndexReader`, builds and runs
/// `IndexReaderExecutor` end-to-end on the Rust stack, populates `StatementContext.is_tikv`
/// and `index_names`, returns exact rows, and leaves the session idle.
/// Unported double-read queries (`SELECT c FROM sbtest1 WHERE k BETWEEN 1 AND 100`) still
/// stop before any executor opens without mutating `StatementContext`.
#[test]
fn index_reader_executes_and_double_read_stops_without_mutating_stmt_ctx() {
    use std::sync::atomic::Ordering;
    use tidb_planner::base::PhysicalKind;
    use tidb_types::datum::new_int_datum;

    let store = MockStore::new(10);
    // Seed secondary index k_1 (index_id=1) entries for sbtest1 (table_id=110): (k=10*id, id).
    for id in 1..=5_i64 {
        let mut encoded = Vec::new();
        tidb_codec::codec::encode_key(
            &tidb_base::gostd::time::utc(),
            &mut encoded,
            &[new_int_datum(id * 10), new_int_datum(id)],
        )
        .unwrap();
        let idx_key = tidb_tablecodec::encode_index_seek_key(110, 1, &encoded);
        store.rows.lock().insert(idx_key.0, vec![b'0']);
    }

    let cache = new_test_cache();
    let mut s = cached_session(&store, sbtest_with_secondary_index_info_schema(), &cache);
    let sql = "SELECT k FROM sbtest1 WHERE k BETWEEN 20 AND 40";

    // Compile produces a PhysicalIndexReader.
    block_on(s.finish_unfinished_stmt());
    block_on(s.core.prepare_txn_ctx()).unwrap();
    let node = parse(sql);
    reset_context_of_stmt(&mut s.core, &node, sql).unwrap();
    s.core
        .txn_manager
        .on_stmt_start(&mut s.core.sctx, Some(&node))
        .unwrap();
    let stmt = tidb_executor::compiler::Compiler
        .compile(&mut s.core, node, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    let plan = stmt.plan.as_ref().expect("compiled plan");
    assert_eq!(plan.kind(), PhysicalKind::IndexReader);
    s.core.rollback_on_error();
    s.core.txn_manager.on_stmt_end();
    assert!(s.is_idle(), "{}", s.state_snapshot());

    // End-to-end execute_stmt runs IndexReaderExecutor and returns exact rows [20, 30, 40].
    let mut rs = block_on(s.execute_stmt(parse(sql), sql))
        .unwrap()
        .expect("result set");
    assert!(s.vars().stmt_ctx().is_tikv.load(Ordering::Relaxed));
    assert_eq!(s.vars().stmt_ctx().index_names, vec!["sbtest1:k_1"]);
    let mut chk = rs.new_chunk().unwrap();
    let mut got = Vec::new();
    loop {
        block_on(rs.next(&mut s, &mut chk)).unwrap();
        if chk.num_rows() == 0 {
            break;
        }
        for i in 0..chk.num_rows() {
            got.push(chk.get_row(i).get_int64(0));
        }
    }
    block_on(rs.close(&mut s)).unwrap();
    assert_eq!(got, vec![20, 30, 40]);
    assert!(s.is_idle(), "{}", s.state_snapshot());

    // Unported double-read (needs column c not in index k_1) stops before any executor opens
    // without mutating StatementContext.
    let double_read_sql = "SELECT c FROM sbtest1 WHERE k BETWEEN 20 AND 40";
    let err = block_on(s.execute_stmt(parse(double_read_sql), double_read_sql))
        .err()
        .unwrap();
    assert!(blocked_by(&err).is_some(), "{err}");
    assert!(!s.vars().stmt_ctx().is_tikv.load(Ordering::Relaxed));
    assert!(s.vars().stmt_ctx().index_names.is_empty());
    assert!(s.is_idle(), "{}", s.state_snapshot());
}

/// Multi-group `GroupRangesByCols` on a clustered common-handle `TableReader` produces
/// grouped ranges in the planner and falls back cleanly at the executor builder boundary
/// (`D07.3`) without mutating `StatementContext` or leaving session state behind.
#[test]
fn multi_group_table_reader_fallback_leaves_stmt_ctx_untouched() {
    use std::sync::atomic::Ordering;
    use tidb_planner::base::{PhysicalKind, PhysicalPlan};
    use tidb_planner::core::physical_plans::{PhysicalTableReader, PhysicalTableScan};

    let store = MockStore::new(10);
    let cache = new_test_cache();
    let mut s = cached_session(&store, sbtest_with_secondary_index_info_schema(), &cache);
    // Use non-adjacent keys (1 and 3) so ranger keeps two separate point ranges instead of
    // coalescing adjacent integers [1,1] U [2,2] into a single range [1,2].
    let sql = "SELECT * FROM tc_pk WHERE (a = 1 OR a = 3) ORDER BY b LIMIT 3";

    block_on(s.finish_unfinished_stmt());
    block_on(s.core.prepare_txn_ctx()).unwrap();
    let node = parse(sql);
    reset_context_of_stmt(&mut s.core, &node, sql).unwrap();
    s.core
        .txn_manager
        .on_stmt_start(&mut s.core.sctx, Some(&node))
        .unwrap();
    let stmt = tidb_executor::compiler::Compiler
        .compile(&mut s.core, node, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    let plan = stmt.plan.as_ref().expect("compiled plan");

    fn find_table_reader(p: &dyn PhysicalPlan) -> Option<&PhysicalTableReader> {
        if let Some(tr) = p.as_any().downcast_ref::<PhysicalTableReader>() {
            return Some(tr);
        }
        p.children().iter().find_map(|c| find_table_reader(&**c))
    }
    let tr = find_table_reader(&**plan).expect("expected TableReader in plan");
    let ts = tr.table_plans[0]
        .as_any()
        .downcast_ref::<PhysicalTableScan>()
        .expect("expected TableScan inside TableReader");
    assert_eq!(ts.group_by_col_idxs, vec![0]);
    assert_eq!(ts.grouped_ranges.len(), 2);
    assert_eq!(plan.kind(), PhysicalKind::Limit);

    let warnings_before = s.vars().stmt_ctx().warning_count();
    let table_ids_before = s.vars().stmt_ctx().related_table_ids.clone();
    let is_tikv_before = s.vars().stmt_ctx().is_tikv.load(Ordering::Relaxed);
    let mut b = tidb_executor::builder::new_executor_builder(
        &s.core,
        None,
        Arc::new(tidb_executor::adapter::TelemetryInfo::default()),
    );
    assert!(block_on(b.build(&mut s.core, Some(plan))).is_none());
    let err = b.err.expect("executor boundary error");
    assert_eq!(blocked_by(&err), Some("D07.3"), "{err}");
    assert_eq!(s.vars().stmt_ctx().warning_count(), warnings_before);
    assert_eq!(s.vars().stmt_ctx().related_table_ids, table_ids_before);
    assert_eq!(
        s.vars().stmt_ctx().is_tikv.load(Ordering::Relaxed),
        is_tikv_before
    );
    s.core.rollback_on_error();
    s.core.txn_manager.on_stmt_end();
    assert!(s.is_idle(), "{}", s.state_snapshot());

    let err = block_on(s.execute_stmt(parse(sql), sql)).err().unwrap();
    assert_eq!(blocked_by(&err), Some("D07.3"), "{err}");
    assert!(s.is_idle(), "{}", s.state_snapshot());
}

mod sbtest_results;
