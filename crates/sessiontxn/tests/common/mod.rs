//! The mock session, store and TSO of the sessiontxn tests.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering::SeqCst;
use std::sync::atomic::{AtomicU64, AtomicUsize};

use futures::FutureExt;
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
use tidb_sessiontxn::sctx::{
    InfoSchemaRef, LazyTxn, Oracle, PlanInfo, PlanKind, SessionCtx, StmtInfo, TsFuture, TxnTsFuture,
};

// ---------------------------------------------------------------- mocks

#[derive(Default)]
pub struct MockSnapshot {
    pub options: Mutex<Vec<isize>>,
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
pub struct MockStore {
    pub snapshots: Mutex<Vec<u64>>,
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
pub struct MockOracle {
    pub next: Arc<AtomicU64>,
    pub requests: AtomicUsize,
    pub low_res: AtomicUsize,
    pub waits: Arc<AtomicUsize>,
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
pub struct MockTxn {
    pub valid: bool,
    pub start_ts: u64,
    pub options: Vec<isize>,
    pub snapshot: Arc<MockSnapshot>,
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

pub struct MockSession {
    pub vars: SessionVars,
    pub txn: MockTxn,
    pub future: Option<TxnTsFuture>,
    pub commits: usize,
    pub is: InfoSchemaRef,
    pub store: Arc<MockStore>,
    pub oracle: Arc<MockOracle>,
}

pub struct NoGlobals;

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
    pub fn new() -> MockSession {
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
    pub fn end_txn(&mut self) {
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

pub struct PointGet;

impl PlanInfo for PointGet {
    fn plan_kind(&self) -> PlanKind<'_> {
        PlanKind::PointGet { lock: false }
    }
    fn is_point_get_with_pk_or_unique_key_by_auto_commit(&self, vars: &SessionVars) -> bool {
        vars.is_autocommit() && !vars.in_txn()
    }
}

pub struct ReadOnlyStmt;

impl StmtInfo for ReadOnlyStmt {
    fn is_read_only(&self, _: &SessionVars) -> bool {
        true
    }
}
