//! The client-go surface `pkg/store/driver` calls for point reads (`tikv.KVStore`,
//! `txnsnapshot.KVSnapshot`), over the client-rust fork.
//!
//! client-go is replaced by client-rust (L3, ADR 0008) and is not ported function by
//! function; this module is the boundary, as `copr::tikv` is for the coprocessor client. The
//! snapshot keeps client-go's options (`SetXxx`) and reads through one client-rust
//! [`tikv_client::Snapshot`] at its timestamp, so the lock sets of the reads stay attached to
//! the snapshot's `start_ts` (N11 §4.5).
//!
//! What client-rust does not have, compared with client-go's `KVSnapshot`
//! (`specs/D07.1-store/client-go-parity.md`):
//!
//! - priority, not-fill-cache, replica read, sample step, task ID, staleness, store labels,
//!   resource group tag and name, request source and the load-based replica read threshold
//!   are kept but not sent: client-rust has no per-request knob for them (MaxTS point gets
//!   are sent at high priority by client-rust itself);
//! - the RC-check-TS isolation level (TiKV reports a newer version as a write conflict) is
//!   refused at read time, so that a read with it is never silently an SI read (N11 §4.4,
//!   constitution §5); RC reads are sent as SI reads, which resolve locks instead of
//!   ignoring them (same result, possibly slower);
//! - no commit TS in the result (`WithReturnCommitTS` fails as client-go fails when TiKV
//!   returns none);
//! - no GC safe-point check after the read (`KVStore.CheckVisibility`, as `copr`'s adapter);
//! - the reads of one snapshot are serialized (client-go allows concurrent ones).

// client-go identifiers for the constants (M-GONAME).
#![allow(non_upper_case_globals)]

use std::sync::Arc;
use std::time::Duration;

use tidb_base::Error;
use tidb_kv::Context;
use tidb_kv::context::RequestSource;
use tidb_kv::kv::{BatchGetOption, GetOption, ValueEntry};
use tidb_kv::option::ReplicaReadType;
use tikv_client::proto::metapb;
use tikv_client::{
    Backoff, RetryOptions, Timestamp, TimestampExt, TransactionClient, TransactionOptions,
};

use super::error::TiKVError;
use crate::copr::tikv::Variables;

/// client-go `txnsnapshot.getMaxBackoff` / `batchGetMaxBackoff` (ms).
const getMaxBackoff: i64 = 20_000;

/// client-go `txnsnapshot.IsoLevel`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IsoLevel {
    #[default]
    SI,
    RC,
    RCCheckTS,
}

/// client-go `txnutil.Priority`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Priority {
    #[default]
    Normal,
    Low,
    High,
}

/// client-go `tikv.KVStore`: the client-rust transaction client.
pub struct KVStore {
    client: Arc<TransactionClient>,
}

impl KVStore {
    // glue: constructs the boundary over a client-rust TransactionClient
    pub fn new(client: Arc<TransactionClient>) -> Arc<KVStore> {
        Arc::new(KVStore { client })
    }

    /// The client-rust client.
    // glue: accessor
    pub fn client(&self) -> &Arc<TransactionClient> {
        &self.client
    }

    /// `KVStore.GetSnapshot(ts)`: a snapshot reading at `ts` (`txnsnapshot.NewTiKVSnapshot`).
    // go-l3: ADR-0008 client-go tikv KVStore.GetSnapshot
    pub fn get_snapshot(self: &Arc<Self>, ts: u64) -> Arc<KVSnapshot> {
        // Sanity check for snapshot version.
        check_snapshot_ts(ts);
        Arc::new(KVSnapshot {
            store: self.clone(),
            opts: parking_lot::Mutex::new(SnapshotOptions {
                version: ts,
                ..SnapshotOptions::default()
            }),
            inner: tokio::sync::Mutex::new(None),
        })
    }

    /// `KVStore.CurrentTimestamp(txnScope)`: a TSO from PD. Local transaction scopes are
    /// deprecated in Go and not in client-rust: every scope reads the global TSO.
    // go-l3: ADR-0008 client-go tikv KVStore.CurrentTimestamp
    pub async fn current_timestamp(&self, _txn_scope: &str) -> Result<u64, TiKVError> {
        Ok(self.client.current_timestamp().await?.version())
    }

    /// `KVStore.Begin(opts...)`: a transaction at the given start ts, or at a new TSO
    /// (client-go retries the TSO with `TsoMaxBackoff`; client-rust's PD client retries its
    /// TSO stream itself). The global scope is the default.
    // go-l3: ADR-0008 client-go tikv KVStore.Begin
    pub async fn begin(self: &Arc<Self>, mut options: TxnOptions) -> Result<KVTxn, TiKVError> {
        if options.txn_scope.is_empty() {
            options.txn_scope = GlobalTxnScope.to_owned();
        }
        let start_ts = match options.start_ts {
            Some(ts) => ts,
            None => self.current_timestamp(&options.txn_scope).await?,
        };
        let snapshot = self.get_snapshot(start_ts);
        Ok(new_tikv_txn(snapshot, start_ts, &options))
    }

    /// `KVStore.GetOracle()`: the PD oracle (the TSO client of the store).
    // go-l3: ADR-0008 client-go tikv KVStore.GetOracle
    pub fn get_oracle(self: &Arc<Self>) -> PdOracle {
        PdOracle {
            store: self.clone(),
        }
    }
}

/// client-go `oracle.GlobalTxnScope`.
pub const GlobalTxnScope: &str = "global";

/// client-go `transaction.TxnOptions` (the options `tikv.WithStartTS` / `WithTxnScope` set;
/// the pipelined MemDB is the write path).
#[derive(Clone, Debug, Default)]
pub struct TxnOptions {
    pub txn_scope: String,
    pub start_ts: Option<u64>,
}

/// client-go `oracles.pdOracle` as `KVStore.GetOracle()` returns it.
#[derive(Clone)]
pub struct PdOracle {
    store: Arc<KVStore>,
}

impl PdOracle {
    /// `GetTimestamp(ctx, &oracle.Option{TxnScope})`: a TSO from PD.
    // go-l3: ADR-0008 client-go oracle/oracles pdOracle.GetTimestamp
    pub async fn get_timestamp(&self, txn_scope: &str) -> Result<u64, TiKVError> {
        self.store.current_timestamp(txn_scope).await
    }

    /// `GetLowResolutionTimestamp`: client-go keeps the last TSO, refreshed in the
    /// background every `tidb_low_resolution_tso_update_interval`; client-rust has no such
    /// cache, so a read with `tidb_low_resolution_tso = ON` cannot get Go's timestamp. The
    /// variable is not in the router's synced set, so such a session never reaches the new
    /// stack (VS1 plan §4.3 step 0); a caller that does gets this error, never a newer TSO.
    // glue: client-go pdOracle.GetLowResolutionTimestamp (L3 boundary) needs the last TSO, blocked-by:D07.1 (client-rust keeps none)
    pub async fn get_low_resolution_timestamp(&self, _txn_scope: &str) -> Result<u64, TiKVError> {
        let why = "the low-resolution TSO of the PD oracle (client-rust keeps no last TSO)";
        Err(TiKVError::Plain(tidb_base::blocked::blocked("D07.1", why)))
    }
}

/// client-go `transaction.NewTiKVTxn`: a transaction over `snapshot` at `start_ts`. Its
/// MemDB stays empty: tidb-rs runs no writes (the read-only node, constitution §4.3), so the
/// union store, the committer and the write options are not kept (D07.1 part 2 when writes
/// come). `IsReadOnly`, `Len` and `Size` read that empty buffer.
// go-l3: ADR-0008 client-go txnkv/transaction NewTiKVTxn
pub fn new_tikv_txn(snapshot: Arc<KVSnapshot>, start_ts: u64, options: &TxnOptions) -> KVTxn {
    KVTxn {
        snapshot,
        start_ts,
        valid: true,
        scope: options.txn_scope.clone(),
        causal_consistency: false,
        is_pessimistic: false,
    }
}

/// client-go `transaction.KVTxn` without a MemDB (see [`new_tikv_txn`]). The options only
/// the committer reads (priority, resource group tag and name, async commit, 1PC, txn
/// source, session id, ...) are not kept: there is no commit of a write; the ones the
/// snapshot reads are passed to it as client-go does.
pub struct KVTxn {
    snapshot: Arc<KVSnapshot>,
    start_ts: u64,
    valid: bool,
    scope: String,
    causal_consistency: bool,
    is_pessimistic: bool,
}

#[allow(clippy::len_without_is_empty)] // client-go has Len and no IsEmpty
impl KVTxn {
    // go-l3: ADR-0008 client-go txnkv/transaction KVTxn.GetSnapshot
    pub fn get_snapshot(&self) -> &Arc<KVSnapshot> {
        &self.snapshot
    }

    // go-l3: ADR-0008 client-go txnkv/transaction KVTxn.String
    pub fn string(&self) -> String {
        format!("{}", self.start_ts())
    }

    // go-l3: ADR-0008 client-go txnkv/transaction KVTxn.StartTS
    pub fn start_ts(&self) -> u64 {
        self.start_ts
    }

    // go-l3: ADR-0008 client-go txnkv/transaction KVTxn.Valid
    pub fn valid(&self) -> bool {
        self.valid
    }

    /// The MemDB is empty (see [`new_tikv_txn`]).
    // go-l3: ADR-0008 client-go txnkv/transaction KVTxn.Len
    pub fn len(&self) -> isize {
        0
    }

    /// The MemDB is empty (see [`new_tikv_txn`]).
    // go-l3: ADR-0008 client-go txnkv/transaction KVTxn.Size
    pub fn size(&self) -> isize {
        0
    }

    /// The MemDB is never dirty and there is no aggressive locking (see [`new_tikv_txn`]).
    // go-l3: ADR-0008 client-go txnkv/transaction KVTxn.IsReadOnly
    pub fn is_read_only(&self) -> bool {
        true
    }

    // go-l3: ADR-0008 client-go txnkv/transaction KVTxn.IsPessimistic
    pub fn is_pessimistic(&self) -> bool {
        self.is_pessimistic
    }

    // go-l3: ADR-0008 client-go txnkv/transaction KVTxn.SetPessimistic
    pub fn set_pessimistic(&mut self, b: bool) {
        self.is_pessimistic = b;
    }

    // go-l3: ADR-0008 client-go txnkv/transaction KVTxn.SetPriority
    pub fn set_priority(&mut self, pri: Priority) {
        self.snapshot.set_priority(pri);
    }

    // go-l3: ADR-0008 client-go txnkv/transaction KVTxn.SetResourceGroupTag
    pub fn set_resource_group_tag(&mut self, tag: Vec<u8>) {
        self.snapshot.set_resource_group_tag(tag);
    }

    // go-l3: ADR-0008 client-go txnkv/transaction KVTxn.SetResourceGroupName
    pub fn set_resource_group_name(&mut self, name: String) {
        self.snapshot.set_resource_group_name(name);
    }

    // go-l3: ADR-0008 client-go txnkv/transaction KVTxn.SetCausalConsistency
    pub fn set_causal_consistency(&mut self, b: bool) {
        self.causal_consistency = b;
    }

    // go-l3: ADR-0008 client-go txnkv/transaction KVTxn.IsCasualConsistency
    pub fn is_casual_consistency(&self) -> bool {
        self.causal_consistency
    }

    // go-l3: ADR-0008 client-go txnkv/transaction KVTxn.SetScope
    pub fn set_scope(&mut self, scope: String) {
        self.scope = scope;
    }

    // go-l3: ADR-0008 client-go txnkv/transaction KVTxn.GetScope
    pub fn get_scope(&self) -> &String {
        &self.scope
    }

    // go-l3: ADR-0008 client-go txnkv/transaction KVTxn.SetRequestSourceInternal
    pub fn set_request_source_internal(&mut self, internal: bool) {
        self.snapshot.set_request_source_internal(internal);
    }

    // go-l3: ADR-0008 client-go txnkv/transaction KVTxn.SetRequestSourceType
    pub fn set_request_source_type(&mut self, tp: String) {
        self.snapshot.set_request_source_type(tp);
    }

    // go-l3: ADR-0008 client-go txnkv/transaction KVTxn.SetExplicitRequestSourceType
    pub fn set_explicit_request_source_type(&mut self, tp: String) {
        self.snapshot.set_explicit_request_source_type(tp);
    }

    /// `Commit`: an invalid transaction is `ErrInvalidTxn`; otherwise the transaction is
    /// closed and, its MemDB being empty, the committer has no mutation and returns at once
    /// (`committer.mutations.Len() == 0`). There are no pessimistic locks to roll back: the
    /// read-only node takes none.
    // go-l3: ADR-0008 client-go txnkv/transaction KVTxn.Commit
    pub async fn commit(&mut self, _ctx: &Context) -> Result<(), TiKVError> {
        end_txn(&mut self.valid)
    }

    /// `Rollback`: an invalid transaction is `ErrInvalidTxn`; otherwise it is closed. There
    /// is no committer (no pessimistic locks, no pipelined flush) to clean up.
    // go-l3: ADR-0008 client-go txnkv/transaction KVTxn.Rollback
    pub fn rollback(&mut self) -> Result<(), TiKVError> {
        end_txn(&mut self.valid)
    }
}

/// The part `Commit` and `Rollback` of a transaction without mutations share: an invalid
/// transaction is `tikverr.ErrInvalidTxn` (TiDB's `kv.ErrInvalidTxn` after `ToTiDBErr`),
/// otherwise the transaction is closed (`KVTxn.close`: `valid = false`; the disk-full
/// option it also clears is a write option, not kept). A transaction ends once: a second
/// commit or rollback fails. Split out so that the state machine is tested without a TiKV
/// client.
// go-l3: ADR-0008 client-go txnkv/transaction KVTxn.close (with the valid check of Commit / Rollback)
fn end_txn(valid: &mut bool) -> Result<(), TiKVError> {
    if !*valid {
        return Err(TiKVError::Plain(tidb_kv::error::ErrInvalidTxn.clone()));
    }
    *valid = false;
    Ok(())
}

/// `txnsnapshot.NewTiKVSnapshot` / `SetSnapshotTS`'s sanity check.
// go-l3: ADR-0008 client-go txnkv/txnsnapshot NewTiKVSnapshot (sanity check)
fn check_snapshot_ts(ts: u64) {
    if ts >= i64::MAX as u64 && ts != u64::MAX {
        panic!("try to get snapshot with a large ts {ts}");
    }
}

/// The fields of client-go's `KVSnapshot` that its setters write.
#[derive(Clone, Debug)]
struct SnapshotOptions {
    version: u64,
    isolation_level: IsoLevel,
    priority: Priority,
    not_fill_cache: bool,
    replica_read_type: ReplicaReadType,
    sample_step: u32,
    task_id: u64,
    collect_runtime_stats: bool,
    is_staleness: bool,
    match_store_labels: Vec<metapb::StoreLabel>,
    resource_group_tag: Vec<u8>,
    read_replica_scope: String,
    request_source: RequestSource,
    scan_batch_size: isize,
    resource_group_name: String,
    load_based_replica_read_threshold: Duration,
    read_timeout: Duration,
    vars: Variables,
    /// Bumped by the setters that need a new client-rust snapshot.
    generation: u64,
}

impl Default for SnapshotOptions {
    // go-l3: ADR-0008 client-go txnkv/txnsnapshot NewTiKVSnapshot (defaults)
    fn default() -> Self {
        SnapshotOptions {
            version: 0,
            isolation_level: IsoLevel::SI,
            priority: Priority::Normal,
            not_fill_cache: false,
            replica_read_type: tidb_kv::option::ReplicaReadLeader,
            sample_step: 0,
            task_id: 0,
            collect_runtime_stats: false,
            is_staleness: false,
            match_store_labels: Vec::new(),
            resource_group_tag: Vec::new(),
            read_replica_scope: String::new(),
            request_source: RequestSource::default(),
            // DefaultScanBatchSize
            scan_batch_size: 256,
            resource_group_name: String::new(),
            load_based_replica_read_threshold: Duration::ZERO,
            read_timeout: Duration::ZERO,
            // kv.DefaultVars
            vars: Variables::default(),
            generation: 0,
        }
    }
}

/// client-go `txnsnapshot.KVSnapshot` over a client-rust snapshot.
pub struct KVSnapshot {
    store: Arc<KVStore>,
    opts: parking_lot::Mutex<SnapshotOptions>,
    /// The client-rust snapshot, built at the first read after the timestamp or the read
    /// timeout changed. Held across the read (an async lock, M-LOCK).
    inner: tokio::sync::Mutex<Option<(u64, tikv_client::Snapshot)>>,
}

impl KVSnapshot {
    /// A client-rust snapshot for the current options: client-go's budget per read
    /// (`NewBackofferWithVars(getMaxBackoff, s.vars)`, client-rust#12), for region errors and
    /// lock waits alike, and `tikv_client_read_timeout`.
    // glue: builds the client-rust snapshot the reads go through
    fn new_client_snapshot(&self) -> tikv_client::Snapshot {
        let o = self.opts.lock().clone();
        let mut max_sleep = getMaxBackoff;
        if o.vars.backoff_weight > 0 {
            max_sleep = max_sleep.wrapping_mul(o.vars.backoff_weight);
        }
        let budget = Backoff::time_budget(max_sleep.max(0) as u64);
        let mut options = TransactionOptions::new_optimistic()
            .read_only()
            .retry_options(RetryOptions::new(budget.clone(), budget));
        if !o.read_timeout.is_zero() {
            options = options.read_timeout(Some(o.read_timeout));
        }
        self.store
            .client
            .snapshot(Timestamp::from_version(o.version), options)
    }

    /// Refuses what client-rust cannot read faithfully (see the module comment).
    // glue: refuses the RC-check-TS isolation level client-rust does not have
    fn check_supported(&self) -> Result<(), TiKVError> {
        if self.opts.lock().isolation_level == IsoLevel::RCCheckTS {
            return Err(TiKVError::Plain(Error::plain(
                "the RC check-ts read (tidb_rc_read_check_ts) is not supported by client-rust",
            )));
        }
        Ok(())
    }

    /// `KVSnapshot.checkCommitTSRequired` for the snapshot tier: client-rust returns no commit
    /// TS, so a required one is always missing.
    // go-l3: ADR-0008 client-go txnkv/txnsnapshot KVSnapshot.checkCommitTSRequired
    fn check_commit_ts_required(
        &self,
        require_commit_ts: bool,
        commit_ts: u64,
    ) -> Result<(), TiKVError> {
        if !require_commit_ts || commit_ts > 0 {
            return Ok(());
        }
        let err_msg = "commit timestamp is required but not returned";
        log::error!("{err_msg} txnStartTS={}", self.opts.lock().version);
        Err(TiKVError::Plain(Error::plain(err_msg)))
    }

    /// `KVSnapshot.Get`: the value of `k`, `NotExist` when there is none (or it is empty).
    // go-l3: ADR-0008 client-go txnkv/txnsnapshot KVSnapshot.Get
    pub async fn get(
        &self,
        _ctx: &Context,
        k: &[u8],
        options: &[GetOption],
    ) -> Result<ValueEntry, TiKVError> {
        let return_commit_ts = options.contains(&GetOption::ReturnCommitTS);
        self.check_supported()?;
        let value = {
            let mut inner = self.inner.lock().await;
            self.client_snapshot(&mut inner).get(k.to_vec()).await?
        };
        let entry = match value {
            Some(v) if !v.is_empty() => tidb_kv::kv::new_value_entry(v, 0),
            _ => return Err(TiKVError::NotExist),
        };
        self.check_commit_ts_required(return_commit_ts, entry.commit_ts)?;
        Ok(entry)
    }

    /// `KVSnapshot.BatchGet`: the values of the keys that exist (and are not empty).
    // go-l3: ADR-0008 client-go txnkv/txnsnapshot KVSnapshot.BatchGet
    pub async fn batch_get(
        &self,
        _ctx: &Context,
        keys: &[Vec<u8>],
        options: &[BatchGetOption],
    ) -> Result<std::collections::HashMap<Vec<u8>, ValueEntry>, TiKVError> {
        let return_commit_ts = options.contains(&BatchGetOption::ReturnCommitTS);
        let mut m = std::collections::HashMap::new();
        if keys.is_empty() {
            return Ok(m);
        }
        self.check_supported()?;
        let pairs: Vec<tikv_client::KvPair> = {
            let mut inner = self.inner.lock().await;
            self.client_snapshot(&mut inner)
                .batch_get(keys.to_vec())
                .await?
                .collect()
        };
        for pair in pairs {
            let (k, v) = (Vec::<u8>::from(pair.0), pair.1);
            if v.is_empty() {
                continue;
            }
            self.check_commit_ts_required(return_commit_ts, 0)?;
            m.insert(k, tidb_kv::kv::new_value_entry(v, 0));
        }
        Ok(m)
    }

    /// The snapshot's timestamp (`KVSnapshot.version`).
    // glue: accessor
    pub fn version(&self) -> u64 {
        self.opts.lock().version
    }

    // go-l3: ADR-0008 client-go txnkv/txnsnapshot KVSnapshot.SetIsolationLevel
    pub fn set_isolation_level(&self, level: IsoLevel) {
        self.opts.lock().isolation_level = level;
    }

    // go-l3: ADR-0008 client-go txnkv/txnsnapshot KVSnapshot.SetPriority
    pub fn set_priority(&self, priority: Priority) {
        self.opts.lock().priority = priority;
    }

    // go-l3: ADR-0008 client-go txnkv/txnsnapshot KVSnapshot.SetNotFillCache
    pub fn set_not_fill_cache(&self, b: bool) {
        self.opts.lock().not_fill_cache = b;
    }

    /// `SetSnapshotTS` resets the timestamp for reads (and the lock sets with it).
    // go-l3: ADR-0008 client-go txnkv/txnsnapshot KVSnapshot.SetSnapshotTS
    pub fn set_snapshot_ts(&self, ts: u64) {
        // Sanity check for snapshot version.
        check_snapshot_ts(ts);
        self.opts.lock().version = ts;
        self.invalidate();
    }

    // go-l3: ADR-0008 client-go txnkv/txnsnapshot KVSnapshot.SetReplicaRead
    pub fn set_replica_read(&self, read_type: ReplicaReadType) {
        self.opts.lock().replica_read_type = read_type;
    }

    // go-l3: ADR-0008 client-go txnkv/txnsnapshot KVSnapshot.SetSampleStep
    pub fn set_sample_step(&self, step: u32) {
        self.opts.lock().sample_step = step;
    }

    // go-l3: ADR-0008 client-go txnkv/txnsnapshot KVSnapshot.SetTaskID
    pub fn set_task_id(&self, id: u64) {
        self.opts.lock().task_id = id;
    }

    /// `SetRuntimeStats`: whether stats are collected (client-rust has no
    /// `SnapshotRuntimeStats`).
    // go-l3: ADR-0008 client-go txnkv/txnsnapshot KVSnapshot.SetRuntimeStats
    pub fn set_runtime_stats(&self, collect: bool) {
        self.opts.lock().collect_runtime_stats = collect;
    }

    // go-l3: ADR-0008 client-go txnkv/txnsnapshot KVSnapshot.SetIsStalenessReadOnly
    pub fn set_is_staleness_read_only(&self, b: bool) {
        self.opts.lock().is_staleness = b;
    }

    // go-l3: ADR-0008 client-go txnkv/txnsnapshot KVSnapshot.SetMatchStoreLabels
    pub fn set_match_store_labels(&self, labels: Vec<metapb::StoreLabel>) {
        self.opts.lock().match_store_labels = labels;
    }

    // go-l3: ADR-0008 client-go txnkv/txnsnapshot KVSnapshot.SetResourceGroupTag
    pub fn set_resource_group_tag(&self, tag: Vec<u8>) {
        self.opts.lock().resource_group_tag = tag;
    }

    // go-l3: ADR-0008 client-go txnkv/txnsnapshot KVSnapshot.SetReadReplicaScope
    pub fn set_read_replica_scope(&self, scope: String) {
        self.opts.lock().read_replica_scope = scope;
    }

    // go-l3: ADR-0008 client-go txnkv/txnsnapshot KVSnapshot.SetRequestSourceInternal
    pub fn set_request_source_internal(&self, internal: bool) {
        self.opts.lock().request_source.request_source_internal = internal;
    }

    // go-l3: ADR-0008 client-go txnkv/txnsnapshot KVSnapshot.SetRequestSourceType
    pub fn set_request_source_type(&self, tp: String) {
        self.opts.lock().request_source.request_source_type = tp;
    }

    // go-l3: ADR-0008 client-go txnkv/txnsnapshot KVSnapshot.SetExplicitRequestSourceType
    pub fn set_explicit_request_source_type(&self, tp: String) {
        self.opts.lock().request_source.explicit_request_source_type = tp;
    }

    // go-l3: ADR-0008 client-go txnkv/txnsnapshot KVSnapshot.SetScanBatchSize
    pub fn set_scan_batch_size(&self, batch_size: isize) {
        self.opts.lock().scan_batch_size = batch_size;
    }

    // go-l3: ADR-0008 client-go txnkv/txnsnapshot KVSnapshot.SetResourceGroupName
    pub fn set_resource_group_name(&self, name: String) {
        self.opts.lock().resource_group_name = name;
    }

    // go-l3: ADR-0008 client-go txnkv/txnsnapshot KVSnapshot.SetLoadBasedReplicaReadThreshold
    pub fn set_load_based_replica_read_threshold(&self, threshold: Duration) {
        self.opts.lock().load_based_replica_read_threshold = threshold;
    }

    /// `SetKVReadTimeout`: `tikv_client_read_timeout` (client-rust's `read_timeout`).
    // go-l3: ADR-0008 client-go txnkv/txnsnapshot KVSnapshot.SetKVReadTimeout
    pub fn set_kv_read_timeout(&self, read_timeout: Duration) {
        self.opts.lock().read_timeout = read_timeout;
        self.invalidate();
    }

    /// Makes the next read build a client-rust snapshot with the new options (a read in
    /// flight keeps its snapshot; Go's setters race with reads the same way).
    // glue: rebuild after SetSnapshotTS / SetKVReadTimeout
    fn invalidate(&self) {
        let mut o = self.opts.lock();
        o.generation = o.generation.wrapping_add(1);
    }

    /// The client-rust snapshot for the current options, built when there is none or the
    /// options it was built with changed.
    // glue: lazily (re)builds the client-rust snapshot
    fn client_snapshot<'s>(
        &self,
        inner: &'s mut Option<(u64, tikv_client::Snapshot)>,
    ) -> &'s mut tikv_client::Snapshot {
        let (generation, version) = {
            let o = self.opts.lock();
            (o.generation, o.version)
        };
        // A MaxTS read reads the latest committed value every time, and client-go caches no
        // read (`s.mu.cached` is filled only by pessimistic locks). The client-rust snapshot
        // keeps every value it read in its buffer, so a MaxTS snapshot that is read again (the
        // point get executor a prepared statement keeps, `ExecStmt.PointGet`) would return
        // the first value forever: build a new one for every MaxTS read.
        if version == u64::MAX || inner.as_ref().is_none_or(|(g, _)| *g != generation) {
            *inner = Some((generation, self.new_client_snapshot()));
        }
        &mut inner.as_mut().expect("just built").1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::error::to_tidb_err;

    /// `KVTxn`'s valid state: a new transaction is valid; commit or rollback ends it; any
    /// later commit or rollback is `ErrInvalidTxn` (also after conversion) and leaves it
    /// ended.
    #[test]
    fn transaction_ends_once() {
        let invalid = |r: Result<(), TiKVError>| {
            let e = to_tidb_err(r.unwrap_err());
            assert!(tidb_kv::error::ErrInvalidTxn.equal(&e), "{e}");
        };
        // commit, then commit / rollback
        let mut valid = true;
        end_txn(&mut valid).unwrap();
        assert!(!valid);
        invalid(end_txn(&mut valid));
        invalid(end_txn(&mut valid));
        assert!(!valid);
        // rollback first
        let mut valid = true;
        end_txn(&mut valid).unwrap();
        invalid(end_txn(&mut valid));
        // TxnOptions: the scope defaults to empty (Begin then uses the global scope).
        let o = TxnOptions::default();
        assert!(o.txn_scope.is_empty() && o.start_ts.is_none());
        assert_eq!(GlobalTxnScope, "global");
    }
}
