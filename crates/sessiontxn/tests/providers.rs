//! Scenario tests of the read path of the isolation providers (D05.4 part 1), on a mock
//! session, store and TSO: the shapes of Go's `pkg/sessiontxn/isolation/*_test.go`
//! (`TestOptimisticTxnContextProvider*`, `TestPessimisticRRTxnContextProvider*`,
//! `TestPessimisticRCTxnContextProvider*`, `TestTidbSnapshotVar*`) that do not need a store:
//! when a TSO is requested and waited, which ts each statement reads, the point-get MaxTS,
//! explicit transactions, `tidb_snapshot`, and the `tidb_rc_read_check_ts` rejection.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering::SeqCst;
use std::sync::atomic::{AtomicU64, AtomicUsize};

use futures::FutureExt;
use futures::executor::block_on;
use futures::future::BoxFuture;
use parking_lot::Mutex;
use tidb_base::Error;
use tidb_kv::Context;
use tidb_kv::key::Key;
use tidb_kv::kv::{
    BatchGetOption, BatchGetter, Client, GetOption, Getter, Iterator as KvIterator, KvFuture,
    OptionValue, Retriever, Snapshot, Storage, Transaction, TxnOption, ValueEntry,
};
use tidb_kv::version::Version;
use tidb_sessionctx::variable::session::SessionVars;
use tidb_sessionctx::variable::session_state::KVVars;
use tidb_sessiontxn::isolation::{
    BaseTxnContextProvider, new_pessimistic_rc_txn_context_provider,
    new_pessimistic_rr_txn_context_provider,
};
use tidb_sessiontxn::sctx::{
    InfoSchemaRef, LazyTxn, Oracle, PlanInfo, PlanKind, ReadTsSource, SessionCtx, StmtInfo,
    TsFuture, TxnTsFuture,
};
use tidb_sessiontxn::{EnterNewTxnType, TxnContextProvider};

// ---------------------------------------------------------------- mocks

#[derive(Default)]
struct MockSnapshot {
    options: Mutex<Vec<isize>>,
}

impl Getter for MockSnapshot {
    fn get<'a>(
        &'a self,
        _: &'a Context,
        _: &'a Key,
        _: &'a [GetOption],
    ) -> KvFuture<'a, ValueEntry> {
        unimplemented!()
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
    fn set_option(&self, opt: isize, _val: OptionValue) {
        self.options.lock().push(opt);
    }
}

#[derive(Default)]
struct MockStore {
    snapshots: Mutex<Vec<u64>>,
}

impl Storage for MockStore {
    fn begin<'a>(&'a self, _: &'a [TxnOption]) -> KvFuture<'a, Box<dyn Transaction>> {
        unimplemented!()
    }
    fn get_snapshot(&self, ver: Version) -> Arc<dyn Snapshot> {
        self.snapshots.lock().push(ver.ver);
        Arc::new(MockSnapshot::default())
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

/// A TSO counting the requests and waits; each request gets the next ts.
#[derive(Default)]
struct MockOracle {
    next: Arc<AtomicU64>,
    requests: AtomicUsize,
    low_res: AtomicUsize,
    waits: Arc<AtomicUsize>,
}

impl MockOracle {
    fn future(&self) -> TsFuture {
        let ts = self.next.fetch_add(10, SeqCst) + 10;
        let waits = self.waits.clone();
        async move {
            waits.fetch_add(1, SeqCst);
            Ok(ts)
        }
        .boxed()
    }
}

impl Oracle for MockOracle {
    fn get_timestamp_async(&self, _: &str) -> TsFuture {
        self.requests.fetch_add(1, SeqCst);
        self.future()
    }
    fn get_low_resolution_timestamp_async(&self, _: &str) -> TsFuture {
        self.low_res.fetch_add(1, SeqCst);
        self.future()
    }
}

#[derive(Default)]
struct MockTxn {
    valid: bool,
    start_ts: u64,
    options: Vec<isize>,
    snapshot: Arc<MockSnapshot>,
}

impl LazyTxn for MockTxn {
    fn valid(&self) -> bool {
        self.valid
    }
    fn start_ts(&self) -> u64 {
        self.start_ts
    }
    fn get_snapshot(&self) -> Arc<dyn Snapshot> {
        self.snapshot.clone()
    }
    fn set_option(&mut self, opt: isize, _val: OptionValue) {
        self.options.push(opt);
    }
    fn set_vars(&mut self, _vars: &KVVars) {}
    fn is_pipelined(&self) -> bool {
        false
    }
}

struct MockSession {
    vars: SessionVars,
    txn: MockTxn,
    future: Option<TxnTsFuture>,
    commits: usize,
    is: InfoSchemaRef,
    store: Arc<MockStore>,
    oracle: Arc<MockOracle>,
}

struct NoGlobals;

impl tidb_sessionctx::variable::session::GlobalVarAccessor for NoGlobals {
    fn get_global_sys_var(&self, _: &str) -> Result<String, Error> {
        unimplemented!()
    }
    fn set_global_sys_var_only(&mut self, _: &str, _: &str, _: bool) -> Result<(), Error> {
        unimplemented!()
    }
    fn get_tidb_table_value(&self, _: &str) -> Result<String, Error> {
        unimplemented!()
    }
    fn set_tidb_table_value(&mut self, _: &str, _: &str, _: &str) -> Result<(), Error> {
        unimplemented!()
    }
}

impl MockSession {
    fn new() -> MockSession {
        let mut vars = SessionVars::new(Box::new(NoGlobals));
        vars.set_status_flag(tidb_base::mysql::ServerStatusAutocommit, true);
        vars.connection_id = 1;
        MockSession {
            vars,
            txn: MockTxn::default(),
            future: None,
            commits: 0,
            is: Arc::new(tidb_infoschema::infoschema_v2::new_info_schema_v2(
                None,
                None,
                tidb_infoschema::new_data(),
            )),
            store: Arc::new(MockStore::default()),
            oracle: Arc::new(MockOracle::default()),
        }
    }

    /// The end of a transaction (the session's `txnManager.OnTxnEnd` / `LazyTxn` reset).
    fn end_txn(&mut self) {
        self.txn = MockTxn::default();
        self.future = None;
        self.vars.set_in_txn(false);
    }
}

impl SessionCtx for MockSession {
    fn get_session_vars(&self) -> &SessionVars {
        &self.vars
    }
    fn get_session_vars_mut(&mut self) -> &mut SessionVars {
        &mut self.vars
    }
    fn txn(&mut self) -> &mut dyn LazyTxn {
        &mut self.txn
    }
    fn has_prepared_txn_future(&self) -> bool {
        self.future.is_some() || self.txn.valid
    }
    fn wait_prepared_txn_future(&mut self) -> BoxFuture<'_, Result<(), Error>> {
        async move {
            if self.txn.valid {
                return Ok(());
            }
            let ts = match self.future.take().expect("a prepared future") {
                TxnTsFuture::Constant(ts) => ts,
                TxnTsFuture::Oracle(f) => f.await?,
            };
            self.txn = MockTxn {
                valid: true,
                start_ts: ts,
                ..Default::default()
            };
            Ok(())
        }
        .boxed()
    }
    fn prepare_ts_future(&mut self, future: TxnTsFuture, _scope: &str) -> Result<(), Error> {
        self.future = Some(future);
        Ok(())
    }
    fn commit_txn(&mut self) -> BoxFuture<'_, Result<(), Error>> {
        async move {
            self.commits += 1;
            self.end_txn();
            Ok(())
        }
        .boxed()
    }
    fn get_domain_info_schema(&self) -> InfoSchemaRef {
        self.is.clone()
    }
    fn get_store(&self) -> Arc<dyn Storage> {
        self.store.clone()
    }
    fn get_oracle(&self) -> Arc<dyn Oracle> {
        self.oracle.clone()
    }
}

struct PointGet;

impl PlanInfo for PointGet {
    fn plan_kind(&self) -> PlanKind<'_> {
        PlanKind::PointGet { lock: false }
    }
    fn is_point_get_with_pk_or_unique_key_by_auto_commit(&self, vars: &SessionVars) -> bool {
        vars.is_autocommit() && !vars.in_txn()
    }
}

struct ReadOnlyStmt;

impl StmtInfo for ReadOnlyStmt {
    fn is_read_only(&self, _: &SessionVars) -> bool {
        true
    }
}

// ---------------------------------------------------------------- scenarios

/// The RC tests read the global `tidb_rc_read_check_ts`, which one of them sets.
static RC_GLOBAL: Mutex<()> = Mutex::new(());

/// An autocommit read: the provider enters lazily, `AdviseWarmup` issues the TSO before
/// planning (prefetch), the first `GetStmtReadTS` waits for it and activates the txn.
#[test]
fn optimistic_autocommit_read_prefetches_tso() {
    let mut s = MockSession::new();
    let mut p = BaseTxnContextProvider::reset_for_new_txn(false);
    block_on(p.on_initialize(&mut s, EnterNewTxnType::EnterNewTxnBeforeStmt)).unwrap();
    assert!(!s.txn.valid);
    assert_eq!(s.oracle.requests.load(SeqCst), 0);
    p.on_stmt_start(&mut s, Some(&ReadOnlyStmt)).unwrap();
    p.advise_warmup(&mut s).unwrap();
    assert_eq!(s.oracle.requests.load(SeqCst), 1, "TSO issued at warmup");
    assert_eq!(s.oracle.waits.load(SeqCst), 0, "not waited yet");
    let ts = block_on(p.get_stmt_read_ts(&mut s)).unwrap();
    assert_eq!(ts, 10);
    assert!(s.txn.valid);
    assert_eq!(s.vars.state.txn_ctx.start_ts, 10);
    assert_eq!(s.oracle.waits.load(SeqCst), 1);
    // A second warmup / read in the statement does not issue another TSO.
    p.advise_warmup(&mut s).unwrap();
    assert_eq!(block_on(p.get_stmt_read_ts(&mut s)).unwrap(), 10);
    assert_eq!(s.oracle.requests.load(SeqCst), 1);
    assert!(!s.vars.in_txn(), "autocommit: not in a transaction");
    assert_eq!(p.read_ts_source(&s), ReadTsSource::Tso);
}

/// The autocommit point get reads at MaxUint64 without any TSO and without activating.
#[test]
fn optimistic_point_get_uses_max_ts() {
    let mut s = MockSession::new();
    let mut p = BaseTxnContextProvider::reset_for_new_txn(false);
    block_on(p.on_initialize(&mut s, EnterNewTxnType::EnterNewTxnBeforeStmt)).unwrap();
    p.on_stmt_start(&mut s, Some(&ReadOnlyStmt)).unwrap();
    p.advise_optimize_with_plan(&mut s, Some(&PointGet))
        .unwrap();
    p.advise_warmup(&mut s).unwrap();
    assert_eq!(block_on(p.get_stmt_read_ts(&mut s)).unwrap(), u64::MAX);
    assert_eq!(s.oracle.requests.load(SeqCst), 0);
    assert!(!s.txn.valid);
    assert_eq!(p.read_ts_source(&s), ReadTsSource::MaxTs);
    // The statement priority becomes high (Go: kv.PriorityHigh).
    assert_eq!(
        s.vars.stmt_ctx().priority,
        tidb_base::mysql::PriorityEnum(2)
    );
}

/// An explicit read-only transaction (BEGIN, then two SELECTs) under RR: one start ts for
/// both statements; BEGIN activates at once and sets InTxn; no MaxTS inside a transaction.
#[test]
fn pessimistic_rr_explicit_txn_reads_one_ts() {
    let mut s = MockSession::new();
    let mut p = new_pessimistic_rr_txn_context_provider(false);
    block_on(p.on_initialize(&mut s, EnterNewTxnType::EnterNewTxnWithBeginStmt)).unwrap();
    assert!(s.vars.in_txn());
    assert!(s.txn.valid);
    assert_eq!(s.vars.state.txn_ctx.isolation, "REPEATABLE-READ");
    assert!(s.vars.state.txn_ctx.is_pessimistic);
    assert!(s.txn.options.contains(&tidb_kv::option::Pessimistic));
    for _ in 0..2 {
        p.on_stmt_start(&mut s, Some(&ReadOnlyStmt)).unwrap();
        p.advise_optimize_with_plan(&mut s, Some(&PointGet))
            .unwrap();
        p.advise_warmup(&mut s).unwrap();
        assert_eq!(block_on(p.get_stmt_read_ts(&mut s)).unwrap(), 10);
    }
    assert_eq!(s.oracle.requests.load(SeqCst), 1);
    assert_eq!(p.read_ts_source(&s), ReadTsSource::TxnStart(10));
    // The snapshot of the statement is the transaction's (start ts == for-update ts == ts).
    block_on(p.get_snapshot_with_stmt_read_ts(&mut s)).unwrap();
    assert!(s.store.snapshots.lock().is_empty());
    // BEGIN again when the txn is not the first statement: the old txn is committed first.
    s.vars.state.txn_ctx.is_staleness = true;
    let mut p2 = new_pessimistic_rr_txn_context_provider(false);
    block_on(p2.on_initialize(&mut s, EnterNewTxnType::EnterNewTxnWithBeginStmt)).unwrap();
    assert_eq!(s.commits, 1);
    assert_eq!(block_on(p2.get_stmt_read_ts(&mut s)).unwrap(), 20);
}

/// RC: after BEGIN (the txn is already prepared) every statement reads at a new TSO,
/// prefetched at warmup; in a lazy transaction (`autocommit=0`) the first statement reads at
/// the start ts.
#[test]
fn pessimistic_rc_per_statement_ts() {
    let _g = RC_GLOBAL.lock();
    let mut s = MockSession::new();
    let mut p = new_pessimistic_rc_txn_context_provider(false);
    block_on(p.on_initialize(&mut s, EnterNewTxnType::EnterNewTxnWithBeginStmt)).unwrap();
    // BEGIN already activated the txn (ts 10): `prepareStmt(!isTxnPrepared)` does not reuse it.
    p.on_stmt_start(&mut s, Some(&ReadOnlyStmt)).unwrap();
    p.advise_warmup(&mut s).unwrap();
    let ts1 = block_on(p.get_stmt_read_ts(&mut s)).unwrap();
    p.on_stmt_start(&mut s, Some(&ReadOnlyStmt)).unwrap();
    p.advise_warmup(&mut s).unwrap();
    let before = s.oracle.waits.load(SeqCst);
    let ts2 = block_on(p.get_stmt_read_ts(&mut s)).unwrap();
    assert_eq!(s.oracle.waits.load(SeqCst), before + 1);
    p.on_stmt_start(&mut s, Some(&ReadOnlyStmt)).unwrap();
    let ts3 = block_on(p.get_stmt_read_ts(&mut s)).unwrap();
    assert_eq!(ts1, 20);
    assert!(ts2 > ts1 && ts3 > ts2, "{ts1} {ts2} {ts3}");
    assert_eq!(s.vars.state.txn_ctx.get_for_update_ts(), ts3);
    assert_eq!(s.vars.state.txn_ctx.isolation, "READ-COMMITTED");
    // The RC read snapshot is at the statement ts, not the txn's snapshot.
    block_on(p.get_snapshot_with_stmt_read_ts(&mut s)).unwrap();
    assert_eq!(*s.store.snapshots.lock(), vec![ts3]);
}

/// RC in a lazy transaction: the first statement reads at the start ts.
#[test]
fn pessimistic_rc_lazy_first_statement_uses_start_ts() {
    let _g = RC_GLOBAL.lock();
    let mut s = MockSession::new();
    s.vars
        .set_status_flag(tidb_base::mysql::ServerStatusAutocommit, false);
    let mut p = new_pessimistic_rc_txn_context_provider(false);
    block_on(p.on_initialize(&mut s, EnterNewTxnType::EnterNewTxnBeforeStmt)).unwrap();
    p.on_stmt_start(&mut s, Some(&ReadOnlyStmt)).unwrap();
    p.advise_warmup(&mut s).unwrap();
    assert_eq!(block_on(p.get_stmt_read_ts(&mut s)).unwrap(), 10);
    assert_eq!(s.txn.start_ts, 10);
    assert_eq!(s.oracle.requests.load(SeqCst), 1);
    p.on_stmt_start(&mut s, Some(&ReadOnlyStmt)).unwrap();
    assert_eq!(block_on(p.get_stmt_read_ts(&mut s)).unwrap(), 20);
}

/// `tidb_rc_read_check_ts` is rejected where Go would read with RCCheckTS (N11 D11).
#[test]
fn pessimistic_rc_read_check_ts_is_rejected() {
    let _g = RC_GLOBAL.lock();
    let mut s = MockSession::new();
    let mut p = new_pessimistic_rc_txn_context_provider(false);
    block_on(p.on_initialize(&mut s, EnterNewTxnType::EnterNewTxnWithBeginStmt)).unwrap();
    tidb_sessionctx::variable::tidb_vars::EnableRCReadCheckTS.store(true, SeqCst);
    let err = p.on_stmt_start(&mut s, Some(&ReadOnlyStmt)).unwrap_err();
    tidb_sessionctx::variable::tidb_vars::EnableRCReadCheckTS.store(false, SeqCst);
    assert_eq!(err.code(), 1235);
    assert!(String::from_utf8_lossy(&err.get_msg()).contains("tidb_rc_read_check_ts"));
    // Outside a transaction Go does not use RCCheckTS either.
    s.end_txn();
    tidb_sessionctx::variable::tidb_vars::EnableRCReadCheckTS.store(true, SeqCst);
    let r = p.on_stmt_start(&mut s, Some(&ReadOnlyStmt));
    tidb_sessionctx::variable::tidb_vars::EnableRCReadCheckTS.store(false, SeqCst);
    r.unwrap();
}

/// `tidb_snapshot`: the txn is prepared with the snapshot ts (no TSO), statements read it,
/// the MaxTS optimization is off, and InTxn stays false.
#[test]
fn tidb_snapshot_reads_snapshot_ts() {
    let mut s = MockSession::new();
    s.vars.snapshot_ts = 777;
    s.vars
        .set_status_flag(tidb_base::mysql::ServerStatusAutocommit, false);
    let mut p = BaseTxnContextProvider::reset_for_new_txn(false);
    block_on(p.on_initialize(&mut s, EnterNewTxnType::EnterNewTxnBeforeStmt)).unwrap();
    p.on_stmt_start(&mut s, Some(&ReadOnlyStmt)).unwrap();
    p.advise_optimize_with_plan(&mut s, Some(&PointGet))
        .unwrap();
    p.advise_warmup(&mut s).unwrap();
    assert_eq!(block_on(p.get_stmt_read_ts(&mut s)).unwrap(), 777);
    assert_eq!(s.txn.start_ts, 777);
    assert_eq!(s.oracle.requests.load(SeqCst), 0);
    assert!(!s.vars.in_txn());
    assert_eq!(p.read_ts_source(&s), ReadTsSource::Snapshot(777));
}

/// `autocommit=0`: the lazy txn sets InTxn when it is activated; `tidb_low_resolution_tso`
/// takes the low-resolution oracle.
#[test]
fn autocommit_off_sets_in_txn_and_low_resolution_tso() {
    let mut s = MockSession::new();
    s.vars
        .set_status_flag(tidb_base::mysql::ServerStatusAutocommit, false);
    s.vars.low_resolution_tso = true;
    let mut p = BaseTxnContextProvider::reset_for_new_txn(false);
    block_on(p.on_initialize(&mut s, EnterNewTxnType::EnterNewTxnBeforeStmt)).unwrap();
    p.on_stmt_start(&mut s, Some(&ReadOnlyStmt)).unwrap();
    assert_eq!(block_on(p.get_stmt_read_ts(&mut s)).unwrap(), 10);
    assert!(s.vars.in_txn());
    assert_eq!(s.oracle.low_res.load(SeqCst), 1);
    assert_eq!(s.oracle.requests.load(SeqCst), 0);
}

/// The staleness provider skeleton: its read ts is the stale ts; entering is part 2.
#[test]
fn staleness_skeleton() {
    let mut s = MockSession::new();
    let mut p = tidb_sessiontxn::staleread::new_staleness_txn_context_provider(42, None);
    assert_eq!(block_on(p.get_stmt_read_ts(&mut s)).unwrap(), 42);
    assert_eq!(p.read_ts_source(&s), ReadTsSource::Stale(42));
    let err =
        block_on(p.on_initialize(&mut s, EnterNewTxnType::EnterNewTxnBeforeStmt)).unwrap_err();
    assert_eq!(err.code(), 1235);
}
