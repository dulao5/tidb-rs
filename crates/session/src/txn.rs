//! go-file: pkg/session/txn.go
//!
//! `LazyTxn`: the session's transaction, lazy until a statement needs its start ts (VS1-14,
//! the read-only part; spec `specs/D05.3-session/spec.md` §2.2).
//!
//! tidb-rs is a read-only node (N11 D8): the statement buffer (`stagingHandle`, the mem
//! buffer), locking and fair locking are the write path and not ported (spec §1.3). A
//! transaction here never has a mutation, so `cleanup` has nothing to clean.

use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;
use tidb_base::Error;
use tidb_kv::kv::{OptionValue, Snapshot, Storage, Transaction, TxnOption};
use tidb_sessionctx::variable::session::SessionVars;
use tidb_sessionctx::variable::session_state::KVVars;
use tidb_sessiontxn::sctx::{LazyTxn as SctxLazyTxn, TxnTsFuture};

/// `txninfo.TxnRunningState`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TxnRunningState {
    /// TxnIdle means the transaction is idle, i.e. waiting for the user's next statement
    #[default]
    TxnIdle,
    /// TxnRunning means the transaction is running, i.e. executing a statement
    TxnRunning,
    /// TxnLockAcquiring means the transaction is trying to acquire a lock
    TxnLockAcquiring,
    /// TxnCommitting means`the transaction is (at least trying to) committing
    TxnCommitting,
    /// TxnRollingBack means the transaction is rolling back
    TxnRollingBack,
}

/// The fields of `txninfo.TxnInfo` that `LazyTxn` maintains (the rest, the session's user,
/// host and the related tables, are filled by the `information_schema.tidb_trx` reader, D05.4
/// part 2).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TxnInfo {
    pub start_ts: u64,
    /// Digest of SQL currently running
    pub current_sql_digest: String,
    /// Digests of all SQLs executed in the transaction.
    pub all_sql_digests: Vec<String>,
    /// Current execution state of the transaction.
    pub state: TxnRunningState,
    /// When last time `State` changes, for metrics
    pub last_state_change_time: Option<Instant>,
    /// Last trying to block start time. Invalid if State is not TxnLockAcquiring.
    pub block_start_time: Option<Instant>,
    /// How many entries are in MemDB
    pub entries_count: u64,
}

/// `txnFuture`: a start ts being fetched, and what to begin the transaction with.
pub struct TxnFuture {
    future: TxnTsFuture,
    store: Arc<dyn Storage>,
    txn_scope: String,
    /// Pipelined DML is the write path: always false.
    pipelined: bool,
}

impl TxnFuture {
    /// A future for `PrepareTSFuture`.
    // glue: Go struct literal &txnFuture{...}
    pub fn new(
        future: TxnTsFuture,
        store: Arc<dyn Storage>,
        txn_scope: String,
        pipelined: bool,
    ) -> TxnFuture {
        TxnFuture {
            future,
            store,
            txn_scope,
            pipelined,
        }
    }

    /// The transaction is begun at the waited ts. Go logs a TSO error and begins without a
    /// start ts (the store then fetches one; `unistore` returns the error).
    // go: txnFuture.wait L2 (no pipelined MemDB: write path)
    async fn wait(self) -> Result<Box<dyn Transaction>, Error> {
        let mut options = vec![TxnOption::TxnScope(self.txn_scope)];
        let start_ts = match self.future {
            TxnTsFuture::Constant(ts) => Ok(ts),
            TxnTsFuture::Oracle(f) => f.await,
        };
        match start_ts {
            Ok(ts) => options.push(TxnOption::StartTS(ts)),
            Err(err) => {
                if tidb_base::config::get_global_config().store == "unistore" {
                    return Err(err);
                }
                tidb_base::log_warn!("wait tso failed", "error" => &err);
            }
        }
        // tf.pipelined: tikv.WithPipelinedMemDB() (write path, never set).
        let _ = self.pipelined;
        self.store.begin(&options).await
    }

    /// The TSO request still in flight (VS1 plan §4.6 item 2: the router may hand it over
    /// to the PoC).
    // glue: VS1 plan §4.6 item 2
    pub fn into_ts_future(self) -> TxnTsFuture {
        self.future
    }
}

/// LazyTxn wraps kv.Transaction to provide a new kv.Transaction.
/// 1. It holds all statement related modification in the buffer before flush to the txn,
///    so if execute statement meets error, the txn won't be made dirty.
/// 2. It's a lazy transaction, that means it's a txnFuture before StartTS() is really need.
#[derive(Default)]
pub struct LazyTxn {
    // States of a LazyTxn should be one of the followings:
    // Invalid: kv.Transaction == nil && txnFuture == nil
    // Pending: kv.Transaction == nil && txnFuture != nil
    // Valid:	kv.Transaction != nil && txnFuture == nil
    transaction: Option<Box<dyn Transaction>>,
    txn_future: Option<TxnFuture>,

    /// `kv.Transaction.SetVars`: the `tidb-kv` transaction has no `SetVars` (the read path
    /// gives the variables to the coprocessor client directly, spec §2.5), so they are kept
    /// here.
    kv_vars: Option<KVVars>,

    /// TxnInfo is added for the lock view feature, the data is frequent modified but
    /// rarely read (just in query select * from information_schema.tidb_trx).
    /// The data in this session would be query by other sessions, so Mutex is necessary.
    /// Since read is rare, the reader can copy-on-read to get a data snapshot.
    mu: Mutex<TxnInfo>,
}

/// `""`, what `GetOption(kv.TxnScope)` returns without a transaction.
static EMPTY_TXN_SCOPE: String = String::new();

impl LazyTxn {
    // go: LazyTxn.init L1
    pub fn init(&mut self) {
        *self.mu.lock() = TxnInfo::default();
    }

    /// call this under lock!
    ///
    /// The duration histogram and the state counter are `txninfo` metrics (D05.4 part 2).
    // go: LazyTxn.updateState L2 (txninfo metrics are D05.4 part 2)
    fn update_state(info: &mut TxnInfo, state: TxnRunningState) {
        if info.state != state {
            info.state = state;
            info.last_state_change_time = Some(Instant::now());
            // txninfo.TxnDurationHistogram(lastState, hasLockLbl).Observe(...),
            // txninfo.TxnStatusEnteringCounter(state).Inc(): D05.4 part 2.
        }
    }

    /// resetTxnInfo resets the transaction info.
    /// Note: call it under lock!
    // go: LazyTxn.resetTxnInfo L2 (txninfo metrics and Recorder.OnTrxEnd are D05.4 part 2)
    fn reset_txn_info(
        info: &mut TxnInfo,
        start_ts: u64,
        state: TxnRunningState,
        entries_count: u64,
        current_sql_digest: String,
        all_sql_digests: Vec<String>,
    ) {
        // TxnDurationHistogram of the last state; txninfo.Recorder.OnTrxEnd when
        // StartTS != 0: D05.4 part 2.
        *info = TxnInfo {
            start_ts,
            state,
            // txninfo.TxnStatusEnteringCounter(state).Inc(): D05.4 part 2.
            last_state_change_time: Some(Instant::now()),
            entries_count,
            current_sql_digest,
            all_sql_digests,
            block_start_time: None,
        };
    }

    /// Size implements the MemBuffer interface.
    // go: LazyTxn.Size L1
    pub fn size(&self) -> isize {
        match &self.transaction {
            None => 0,
            Some(t) => t.size(),
        }
    }

    /// Valid implements the kv.Transaction interface.
    // go: LazyTxn.Valid L1
    pub fn valid(&self) -> bool {
        self.transaction.as_ref().is_some_and(|t| t.valid())
    }

    /// `pending`.
    // glue: Go method LazyTxn.pending (one line)
    pub fn pending(&self) -> bool {
        self.transaction.is_none() && self.txn_future.is_some()
    }

    /// `validOrPending`.
    // glue: Go method LazyTxn.validOrPending (one line)
    pub fn valid_or_pending(&self) -> bool {
        self.txn_future.is_some() || self.valid()
    }

    // go: LazyTxn.String L2 (no fair locking: write path)
    pub fn string(&self) -> String {
        if let Some(t) = &self.transaction {
            return t.string();
        }
        if self.txn_future.is_some() {
            // " (pending fair locking)": fair locking is the write path.
            return "txnFuture".to_owned();
        }
        "invalid transaction".to_owned()
    }

    /// GetOption implements the GetOption
    // go: LazyTxn.GetOption L1
    pub fn get_option(&self, opt: isize) -> Option<&(dyn std::any::Any + Send + Sync)> {
        match &self.transaction {
            None => {
                if opt == tidb_kv::option::TxnScope {
                    return Some(&EMPTY_TXN_SCOPE);
                }
                None
            }
            Some(t) => t.get_option(opt),
        }
    }

    // go: LazyTxn.changeToPending L1
    pub fn change_to_pending(&mut self, future: TxnFuture) {
        self.transaction = None;
        self.txn_future = Some(future);
    }

    /// The pending future waits for its ts and becomes a transaction.
    ///
    /// The statement buffer (`initStmtBuf`) and fair locking are the write path; the size
    /// limits (`kv.SizeLimits`) only bound writes.
    // go-part: LazyTxn.Wait (changePendingToValid)
    async fn change_pending_to_valid(&mut self, resource_group_name: String) -> Result<(), Error> {
        let Some(future) = self.txn_future.take() else {
            return Err(tidb_base::errors::new("transaction future is not set"));
        };

        let t = match future.wait().await {
            Ok(t) => t,
            Err(err) => {
                self.transaction = None;
                return Err(err);
            }
        };
        let start_ts = t.start_ts();
        let len = t.len();
        self.transaction = Some(t);

        // The txnInfo may already recorded the first statement (usually "begin") when it's
        // pending, so keep them.
        let mut info = self.mu.lock();
        let current = std::mem::take(&mut info.current_sql_digest);
        let all = std::mem::take(&mut info.all_sql_digests);
        Self::reset_txn_info(
            &mut info,
            start_ts,
            TxnRunningState::TxnIdle,
            len as u64,
            current,
            all,
        );
        drop(info);

        // set resource group name for kv request such as lock pessimistic keys.
        if let Some(t) = self.transaction.as_mut() {
            t.set_option(
                tidb_kv::option::ResourceGroupName,
                Box::new(resource_group_name),
            );
        }
        // TxnEntrySizeLimit -> kv.SizeLimits: limits of writes (write path).
        Ok(())
    }

    // go: LazyTxn.changeToInvalid L2 (no staging buffer: write path; txninfo metrics are D05.4 part 2)
    pub fn change_to_invalid(&mut self) {
        self.transaction = None;
        self.txn_future = None;
        self.kv_vars = None;

        let mut info = self.mu.lock();
        // txninfo.Recorder.OnTrxEnd when StartTS != 0 and the duration histogram: D05.4
        // part 2.
        *info = TxnInfo::default();
    }

    // go: LazyTxn.onStmtStart L1
    pub fn on_stmt_start(&self, current_sql_digest: &str) {
        if current_sql_digest.is_empty() {
            return;
        }

        let mut info = self.mu.lock();
        Self::update_state(&mut info, TxnRunningState::TxnRunning);
        info.current_sql_digest = current_sql_digest.to_owned();
        // Keeps at most 50 history sqls to avoid consuming too much memory.
        const MAX_TRANSACTION_STMT_HISTORY: usize = 50;
        if info.all_sql_digests.len() < MAX_TRANSACTION_STMT_HISTORY {
            info.all_sql_digests.push(current_sql_digest.to_owned());
        }
    }

    // go: LazyTxn.onStmtEnd L1
    pub fn on_stmt_end(&self) {
        let mut info = self.mu.lock();
        info.current_sql_digest = String::new();
        Self::update_state(&mut info, TxnRunningState::TxnIdle);
    }

    /// Commit overrides the Transaction interface.
    // go: LazyTxn.Commit L2 (no failpoints)
    pub async fn commit(&mut self) -> Result<(), Error> {
        Self::update_state(&mut self.mu.lock(), TxnRunningState::TxnCommitting);
        let ctx = tidb_kv::Context::background();
        let r = match self.transaction.as_mut() {
            Some(t) => t.commit(&ctx).await,
            None => Err(tidb_kv::error::ErrInvalidTxn.clone()),
        };
        self.reset();
        r
    }

    /// Rollback overrides the Transaction interface.
    // go: LazyTxn.Rollback L2 (no memory footprint hook: write path)
    pub fn rollback(&mut self) -> Result<(), Error> {
        Self::update_state(&mut self.mu.lock(), TxnRunningState::TxnRollingBack);
        let r = match self.transaction.as_mut() {
            Some(t) => t.rollback(),
            None => Err(tidb_kv::error::ErrInvalidTxn.clone()),
        };
        self.reset();
        r
    }

    // go: LazyTxn.reset L1
    fn reset(&mut self) {
        self.cleanup();
        self.change_to_invalid();
    }

    /// `cleanupStmtBuf` + `initStmtBuf`: a read-only transaction has no statement buffer.
    // go: LazyTxn.cleanup L2 (no statement buffer: write path)
    pub fn cleanup(&mut self) {}

    /// Wait converts pending txn to valid
    // go: LazyTxn.Wait L2 (lazyUniquenessCheckEnabled is the write path)
    pub async fn wait(&mut self, vars: &mut SessionVars) -> Result<(), Error> {
        if !self.valid_or_pending() {
            return Err(tidb_kv::error::ErrInvalidTxn.clone());
        }
        if self.pending() {
            let begin = Instant::now();
            let rg = vars.stmt_ctx().resource_group_name.clone();
            let r = self.change_pending_to_valid(rg).await;
            vars.duration_wait_ts = begin.elapsed().as_nanos() as i64;
            if let Err(err) = r {
                tidb_base::log_warn!("active transaction fail", "error" => &err);
                self.cleanup();
                vars.state.txn_ctx.start_ts = 0;
                return Err(err);
            }
        }
        Ok(())
    }

    /// The pending TSO request, if the transaction is pending (VS1 plan §4.6 item 2); the
    /// transaction becomes invalid.
    // glue: VS1 plan §4.6 item 2
    pub fn take_pending_future(&mut self) -> Option<TxnFuture> {
        if !self.pending() {
            return None;
        }
        let f = self.txn_future.take();
        self.change_to_invalid();
        f
    }

    /// The transaction info (`mu.TxnInfo`), copied.
    // glue: accessor of LazyTxn.mu.TxnInfo
    pub fn txn_info(&self) -> TxnInfo {
        self.mu.lock().clone()
    }

    /// `IsReadOnly()` of the transaction.
    // glue: kv.Transaction.IsReadOnly through the embedded transaction
    pub fn is_read_only(&self) -> bool {
        self.transaction.as_ref().is_none_or(|t| t.is_read_only())
    }

    /// `IsPessimistic()` of the transaction.
    // glue: kv.Transaction.IsPessimistic through the embedded transaction
    pub fn is_pessimistic(&self) -> bool {
        self.transaction
            .as_ref()
            .is_some_and(|t| t.is_pessimistic())
    }
}

impl SctxLazyTxn for LazyTxn {
    // glue: interface method kv.Transaction.Valid (LazyTxn)
    fn valid(&self) -> bool {
        LazyTxn::valid(self)
    }
    /// Go calls `StartTS` on the embedded transaction (a nil transaction panics); sessiontxn
    /// only asks a valid one. 0 otherwise.
    // glue: interface method kv.Transaction.StartTS (LazyTxn)
    fn start_ts(&self) -> u64 {
        self.transaction.as_ref().map_or(0, |t| t.start_ts())
    }
    // glue: interface method kv.Transaction.GetSnapshot (LazyTxn)
    fn get_snapshot(&self) -> Arc<dyn Snapshot> {
        self.transaction
            .as_ref()
            .expect("GetSnapshot of a valid transaction")
            .get_snapshot()
    }
    // glue: interface method kv.Transaction.SetOption (LazyTxn)
    fn set_option(&mut self, opt: isize, val: OptionValue) {
        if let Some(t) = self.transaction.as_mut() {
            t.set_option(opt, val);
        }
    }
    // glue: interface method kv.Transaction.SetVars (LazyTxn)
    fn set_vars(&mut self, vars: &KVVars) {
        self.kv_vars = Some(vars.clone());
    }
    /// Pipelined DML is the write path.
    // glue: interface method kv.Transaction.IsPipelined (LazyTxn)
    fn is_pipelined(&self) -> bool {
        false
    }
}
