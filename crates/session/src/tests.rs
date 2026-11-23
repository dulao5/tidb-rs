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

struct NoSnapshot;

impl Getter for NoSnapshot {
    fn get<'a>(
        &'a self,
        _: &'a Context,
        _: &'a Key,
        _: &'a [GetOption],
    ) -> KvFuture<'a, ValueEntry> {
        // The mock store has no rows: a point get finds nothing.
        Box::pin(async { Err(tidb_kv::error::ErrNotExist.clone()) })
    }
}
impl Retriever for NoSnapshot {
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
impl BatchGetter for NoSnapshot {
    fn batch_get<'a>(
        &'a self,
        _: &'a Context,
        _: &'a [Key],
        _: &'a [BatchGetOption],
    ) -> KvFuture<'a, HashMap<Vec<u8>, ValueEntry>> {
        unimplemented!()
    }
}
impl Snapshot for NoSnapshot {
    fn set_option(&self, _: isize, _: OptionValue) {}
}

/// A read-only transaction.
struct MockTxn {
    start_ts: u64,
    log: Arc<Mutex<Vec<String>>>,
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
        Arc::new(NoSnapshot)
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
        Box::pin(async move { Ok(Box::new(MockTxn { start_ts, log }) as Box<dyn Transaction>) })
    }
    fn get_snapshot(&self, _: Version) -> Arc<dyn Snapshot> {
        Arc::new(NoSnapshot)
    }
    fn get_client(&self) -> Arc<dyn Client> {
        // The planning context takes the store's client (GetPlanCtx); nothing sends with it.
        Arc::new(MockClient)
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

fn parse(sql: &str) -> Arc<ast::Stmt> {
    Arc::new(
        tidb_parser::parse(sql, &Default::default())
            .unwrap()
            .stmts
            .remove(0),
    )
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
        .on_stmt_start(&mut s.core.sctx, Some(Arc::clone(&node)))
        .unwrap();
    let (plan, counters) = mock_plan(rows);
    s.core
        .advise_optimize_with_plan_and_then_warm_up(&plan)
        .unwrap();
    let mut stmt = ExecStmt::new(sql.to_owned(), Some(plan), None);
    stmt.stmt_node = Some(node);
    stmt.output_names = vec![tidb_types::field_name::FieldName {
        col_name: tidb_parser::model::CIStr::new("a"),
        ..Default::default()
    }];
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
/// stub (here the table dual executor, `blocked-by:D12.2`; the cached-table, locking-read
/// and txn-scope stubs of the builder are the same path) is a normal fallback.
#[test]
fn executed_stmts_counts_only_opened_executors() {
    let store = MockStore::new(10);
    let mut s = session(&store);
    assert_eq!(s.executed_stmts(), 0);
    let mut stmt = compile(&mut s, "SELECT a FROM t", 0);
    stmt.plan = Some(mock_plan_of_kind(0, tidb_planner::PhysicalKind::TableDual).0);
    let Err(e) = block_on(run_stmt(&mut s, stmt)) else {
        panic!("the table dual executor is a boundary stub");
    };
    assert_eq!(blocked(&e), Some("D12.2"));
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

/// `Compile` fails (the planner is a boundary stub): `rollbackOnError` ends the lazy
/// transaction, so the next statement starts a new one; no TSO was requested.
#[test]
fn compile_error_rolls_back() {
    let store = MockStore::new(10);
    let mut s = session(&store);
    let e = block_on(s.execute_stmt(parse("SELECT a FROM t"), "SELECT a FROM t"))
        .err()
        .unwrap();
    // A plain statement stops in Compile before the preprocessor (compiler.rs
    // `planner_stmt_node`).
    assert_eq!(blocked(&e), Some("D05.3"));
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
    let _ = block_on(s.execute_stmt(parse("SELECT 1"), "SELECT 1"));
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
        ("in Compile (planner stub)", |s| {
            let e = block_on(s.execute_stmt(parse("SELECT a FROM t"), "SELECT a FROM t"))
                .err()
                .unwrap();
            assert_eq!(blocked(&e), Some("D05.3"));
            "D05.3"
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
        let mut s = session(&store);
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
fn unported() -> Arc<ast::Stmt> {
    let node = parse("INSERT INTO t VALUES (1)");
    assert!(matches!(*node, ast::Stmt::Unported(_)));
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
        tidb_planner::core::plan_cache_utils::new_plan_cache_key(&st)
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
    use tidb_model::{ColumnInfo, DBInfo, StatePublic, TableInfo};
    use tidb_parser::model::CIStr;
    use tidb_parser::types::FieldType;
    let col = |id: i64, name: &str, tp: u8| {
        let mut field_type = FieldType::new(tp);
        if id == 1 {
            // The handle: `id INT PRIMARY KEY` (pk_is_handle).
            field_type.add_flag(tidb_base::mysql::PriKeyFlag | tidb_base::mysql::NotNullFlag);
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
    block_on(b.init_with_db_infos(vec![db], vec![], vec![], 1)).unwrap();
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

/// `PREPARE` + `EXECUTE` of the sysbench point select on the real session over the sbtest
/// InfoSchema (mock store): PREPARE passes the metadata lock and builds the plan (the column
/// `c` of `sbtest1`); EXECUTE plans the point get on the fast path with the parameter's
/// value, `Compile` keeps the Execute plan (no reuse of the prepared point get executor:
/// the plan cache's), `Exec` replaces it with the point get inside and builds and opens a
/// new point get executor, which reads the mock store (no row). The statement ends with
/// its autocommit transaction and the session is idle.
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
    let mut rs =
        block_on(s.execute_prepared_stmt(info.stmt_id, vec![tidb_types::datum::new_int_datum(1)]))
            .unwrap()
            .expect("a result set");
    assert_eq!(params_of(&s), vec!["1".to_owned()]);
    assert_eq!(
        s.executed_stmts(),
        1,
        "the point get executor was built and opened"
    );
    assert!(
        s.vars().stmt_ctx().get_plan().is_some_and(|p| p
            .downcast_ref::<tidb_planner::PhysicalPlanRef>()
            .is_some_and(|p| p.kind() == tidb_planner::PhysicalKind::PointGet)),
        "inheritContextFromExecuteStmt put the point get into the statement context"
    );
    let mut chk = rs.new_chunk().unwrap();
    block_on(rs.next(&mut s, &mut chk)).unwrap();
    assert_eq!(chk.num_rows(), 0);
    block_on(rs.close(&mut s)).unwrap();
    assert!(s.is_idle(), "{}", s.state_snapshot());
    s.drop_prepared_stmt(info.stmt_id).unwrap();
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
    let ast::Stmt::Select(sel) = select_marker.as_ref() else {
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
        let e = block_on(s.execute_prepared_stmt(info.stmt_id, vec![param]))
            .err()
            .unwrap();
        assert!(
            PHYSICAL_BOUNDARIES.contains(&blocked(&e).unwrap_or_default()),
            "{text}: {e}"
        );
        assert_eq!(s.executed_stmts(), 0, "{text}");
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
            .on_stmt_start(&mut s.core.sctx, Some(node))
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
    ps.prepared_ast.stmt = Box::new(Arc::try_unwrap(parse(sql)).unwrap());
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
/// not ported yet in `Exec` (the table reader D12.1, VS1-34; the projection or table dual
/// D12.2, VS1-35). Every one of them comes before an executor opened, so the caller falls
/// back. The plan replayer capture (`recordTableRuntimeStats`) no longer stops it (see
/// `plan_replayer_capture_records_the_table_stats_and_goes_on`).
const PHYSICAL_BOUNDARIES: [&str; 6] = ["D10.6", "D10.2", "D10.9", "D10.5", "D12.1", "D12.2"];

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
    let e =
        block_on(s.execute_prepared_stmt(info.stmt_id, vec![tidb_types::datum::new_int_datum(1)]))
            .err()
            .unwrap();
    assert_eq!(blocked(&e), Some("D12.1"), "{e}");
    assert_eq!(s.executed_stmts(), 0);
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
    let sql = "SELECT c FROM sbtest1 WHERE id > ?";
    let info = block_on(s.prepare_stmt(sql)).unwrap();
    s.vars().stmt_ctx().append_warning(Error::plain("earlier"));
    let before = s.state_snapshot();
    let cp = s.checkpoint();
    let e =
        block_on(s.execute_prepared_stmt(info.stmt_id, vec![tidb_types::datum::new_int_datum(1)]))
            .err()
            .unwrap();
    assert!(
        PHYSICAL_BOUNDARIES.contains(&blocked(&e).unwrap_or_default()),
        "{e}"
    );
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
struct MockClient;

impl Client for MockClient {
    fn send(
        &self,
        _: &tidb_kv::Context,
        _: &tidb_kv::kv::Request,
        _: Option<tidb_kv::kv::OptionValue>,
        _: &tidb_kv::kv::ClientSendOption,
    ) -> Box<dyn tidb_kv::kv::Response> {
        unimplemented!("no test sends a coprocessor request")
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
