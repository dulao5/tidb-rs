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
        let generation = self.opts.lock().generation;
        if inner.as_ref().is_none_or(|(g, _)| *g != generation) {
            *inner = Some((generation, self.new_client_snapshot()));
        }
        &mut inner.as_mut().expect("just built").1
    }
}
