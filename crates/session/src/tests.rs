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
use tidb_executor::testutil::mock_plan;
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
        unimplemented!()
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
        unimplemented!()
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
    block_on(s.finish_unfinished_stmt());
    block_on(s.core.prepare_txn_ctx()).unwrap();
    let node = parse(sql);
    reset_context_of_stmt(&mut s.core, &node, sql).unwrap();
    s.core
        .txn_manager
        .on_stmt_start(&mut s.core.sctx, Some(Arc::clone(&node)))
        .unwrap();
    let (plan, _) = mock_plan(rows);
    s.core
        .advise_optimize_with_plan_and_then_warm_up(&plan)
        .unwrap();
    let mut stmt = ExecStmt::new(sql.to_owned(), Some(plan), None);
    stmt.stmt_node = Some(node);
    stmt
}

/// Reads every row of a result.
fn drain(s: &mut Session, rs: &mut crate::ExecStmtResult) -> Vec<i64> {
    let mut out = Vec::new();
    let mut chk = rs.new_chunk();
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
    // `finishStmt` asks `IsReadOnly`, still a boundary stub: its error is the statement's,
    // and the autocommit transaction is rolled back.
    let e = block_on(rs.close(&mut s)).unwrap_err();
    assert_eq!(blocked(&e), Some("D09.13"));
    assert!(s.is_idle(), "{}", s.state_snapshot());
    assert!(!s.in_txn());
    assert_eq!(s.vars().last_query_info.start_ts, 10);
    assert_eq!(store.log.lock().as_slice(), ["begin 10", "rollback 10"]);
    assert_eq!(store.requests.load(SeqCst), 1);
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
    assert_eq!(blocked(&e), Some("D09.13"));
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
    let mut chk = rs.new_chunk();
    block_on(rs.next(&mut s, &mut chk)).unwrap();
    drop(rs);
    assert!(s.has_unfinished_stmt());
    assert!(
        s.core.sctx.lazy_txn().valid(),
        "the transaction is still open"
    );

    // The next statement (it fails at Compile) finishes the dropped one first.
    let _ = block_on(s.execute_stmt(parse("SELECT 1"), "SELECT 1"));
    assert!(!s.has_unfinished_stmt());
    assert!(s.is_idle());
    assert_eq!(s.vars().last_query_info.start_ts, 10);
    assert_eq!(store.log.lock().as_slice(), ["begin 10", "rollback 10"]);

    // Same with an explicit call, as the router does before a checkpoint.
    let stmt = compile(&mut s, "SELECT a FROM t", 5);
    block_on(s.core.get_stmt_read_ts()).unwrap();
    let rs = block_on(run_stmt(&mut s, stmt)).unwrap().expect("rows");
    drop(rs);
    block_on(s.finish_unfinished_stmt());
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
            assert_eq!(blocked(&e), Some("D09.13"));
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
