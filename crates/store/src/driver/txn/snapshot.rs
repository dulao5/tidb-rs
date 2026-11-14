//! go-file: pkg/store/driver/txn/snapshot.go
//!
//! `tikvSnapshot`: TiDB's `kv.Snapshot` over client-go's `txnsnapshot.KVSnapshot` (here the
//! client-rust boundary [`KVSnapshot`]). Iterators (`Iter`, `IterReverse`) are D07.1 part 2
//! (the scanner, T11).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tidb_base::blocked::blocked;
use tidb_kv::kv::{
    BatchGetOption, BatchGetter, GetOption, Getter, IsoLevel, Iterator, KvFuture, OptionValue,
    Retriever, Snapshot, SnapshotInterceptor, ValueEntry,
};
use tidb_kv::option::ReplicaReadType;
use tidb_kv::{Context, Key};
use tikv_client::proto::metapb;

use super::error::extract_key_err;
use crate::driver::tikv::{self, KVSnapshot};

/// tikvSnapshot implements kv.Snapshot.
pub struct TikvSnapshot {
    kv_snapshot: Arc<KVSnapshot>,
    /// `interceptor` (set by `SetOption(kv.SnapInterceptor, ...)`, so behind a lock: Go's
    /// `SetOption` takes a pointer receiver, the Rust trait `&self`).
    interceptor: parking_lot::RwLock<Option<Arc<dyn SnapshotInterceptor>>>,
}

/// NewSnapshot creates a kv.Snapshot with txnsnapshot.KVSnapshot.
// go: NewSnapshot L1
pub fn new_snapshot(snapshot: Arc<KVSnapshot>) -> TikvSnapshot {
    TikvSnapshot {
        kv_snapshot: snapshot,
        interceptor: parking_lot::RwLock::new(None),
    }
}

impl TikvSnapshot {
    /// The client-go snapshot (Go's embedded `*txnsnapshot.KVSnapshot`).
    // glue: accessor of the embedded KVSnapshot
    pub fn kv_snapshot(&self) -> &Arc<KVSnapshot> {
        &self.kv_snapshot
    }

    // glue: reads the interceptor field
    fn interceptor(&self) -> Option<Arc<dyn SnapshotInterceptor>> {
        self.interceptor.read().clone()
    }
}

impl BatchGetter for TikvSnapshot {
    /// BatchGet gets all the keys' value from kv-server and returns a map contains key/value
    /// pairs. The map will not contain nonexistent keys.
    // go: tikvSnapshot.BatchGet L1 [M-BYTES]
    fn batch_get<'a>(
        &'a self,
        ctx: &'a Context,
        keys: &'a [Key],
        options: &'a [BatchGetOption],
    ) -> KvFuture<'a, HashMap<Vec<u8>, ValueEntry>> {
        Box::pin(async move {
            if let Some(interceptor) = self.interceptor() {
                let snap = new_snapshot(self.kv_snapshot.clone());
                return interceptor.on_batch_get(ctx, &snap, keys, options).await;
            }
            let data = self
                .kv_snapshot
                .batch_get(ctx, &to_tikv_keys(keys), options)
                .await;
            data.map_err(extract_key_err)
        })
    }
}

impl Getter for TikvSnapshot {
    /// Get gets the value for key k from snapshot.
    // go: tikvSnapshot.Get L1
    fn get<'a>(
        &'a self,
        ctx: &'a Context,
        k: &'a Key,
        options: &'a [GetOption],
    ) -> KvFuture<'a, ValueEntry> {
        Box::pin(async move {
            if let Some(interceptor) = self.interceptor() {
                let snap = new_snapshot(self.kv_snapshot.clone());
                return interceptor.on_get(ctx, &snap, k, options).await;
            }

            let data = self.kv_snapshot.get(ctx, k, options).await;
            data.map_err(extract_key_err)
        })
    }
}

impl Retriever for TikvSnapshot {
    /// Iter return a list of key-value pair after `k`.
    ///
    /// The scanner (`txnsnapshot.Scanner`, `tikvScanner`) is D07.1 part 2.
    // go: tikvSnapshot.Iter L1
    fn iter<'a: 'f, 'f>(
        &'a self,
        k: &'f Key,
        upper_bound: &'f Key,
    ) -> KvFuture<'f, Box<dyn Iterator + 'a>> {
        Box::pin(async move {
            if let Some(interceptor) = self.interceptor() {
                // Go passes a new tikvSnapshot over the same KVSnapshot; the interceptor's
                // iterator would borrow it beyond this call. Both paths need the scanner.
                let _ = (interceptor, k, upper_bound);
                return Err(blocked("D07.1", "snapshot iterators with an interceptor"));
            }
            Err(blocked("D07.1", "snapshot iterators (D07.1 part 2)"))
        })
    }

    /// IterReverse creates a reversed Iterator positioned on the first entry which key is less
    /// than k.
    // go: tikvSnapshot.IterReverse L1
    fn iter_reverse<'a: 'f, 'f>(
        &'a self,
        k: &'f Key,
        lower_bound: &'f Key,
    ) -> KvFuture<'f, Box<dyn Iterator + 'a>> {
        Box::pin(async move {
            if let Some(interceptor) = self.interceptor() {
                let _ = (interceptor, k, lower_bound);
                return Err(blocked("D07.1", "snapshot iterators with an interceptor"));
            }
            Err(blocked("D07.1", "snapshot iterators (D07.1 part 2)"))
        })
    }
}

/// `val.(T)`: Go's type assertion, which panics on another type.
// glue: Go type assertion on SetOption's `any`
fn assert_type<T: Clone + 'static>(val: &OptionValue, opt: &str) -> T {
    match val.downcast_ref::<T>() {
        Some(v) => v.clone(),
        None => panic!(
            "interface conversion: interface {{}} is not {} (option {opt})",
            std::any::type_name::<T>()
        ),
    }
}

impl Snapshot for TikvSnapshot {
    /// The option values are the Rust types of Go's: `kv.IsoLevel`, `isize` for Go's `int`,
    /// `bool`, `u64`, `kv.ReplicaReadType`, `u32`, `Option<()>` for the runtime stats (client-
    /// rust has no `SnapshotRuntimeStats`; `None` is Go's nil), `Vec<metapb::StoreLabel>`,
    /// `Vec<u8>`, `String`, `Arc<dyn SnapshotInterceptor>`, `Duration`. The RPC interceptor,
    /// the resource group tagger and the replica read adjuster have no Rust type yet (no
    /// caller): they are accepted and not applied.
    // go: tikvSnapshot.SetOption L1
    fn set_option(&self, opt: isize, val: OptionValue) {
        use tidb_kv::option as o;
        match opt {
            o::IsolationLevel => {
                let level =
                    get_tikv_isolation_level(assert_type::<IsoLevel>(&val, "IsolationLevel"));
                self.kv_snapshot.set_isolation_level(level);
            }
            o::Priority => {
                self.kv_snapshot
                    .set_priority(get_tikv_priority(assert_type::<isize>(&val, "Priority")));
            }
            o::NotFillCache => {
                self.kv_snapshot
                    .set_not_fill_cache(assert_type::<bool>(&val, "NotFillCache"));
            }
            o::SnapshotTS => {
                self.kv_snapshot
                    .set_snapshot_ts(assert_type::<u64>(&val, "SnapshotTS"));
            }
            o::ReplicaRead => {
                // Go converts with options.GetTiKVReplicaReadType (D07.1, not this part);
                // client-rust has no replica read, the snapshot keeps TiDB's type.
                let t = assert_type::<ReplicaReadType>(&val, "ReplicaRead");
                self.kv_snapshot.set_replica_read(t);
            }
            o::SampleStep => {
                self.kv_snapshot
                    .set_sample_step(assert_type::<u32>(&val, "SampleStep"));
            }
            o::TaskID => {
                self.kv_snapshot
                    .set_task_id(assert_type::<u64>(&val, "TaskID"));
            }
            o::CollectRuntimeStats => {
                let stats = assert_type::<Option<()>>(&val, "CollectRuntimeStats");
                if stats.is_none() {
                    self.kv_snapshot.set_runtime_stats(false);
                } else {
                    self.kv_snapshot.set_runtime_stats(true);
                }
            }
            o::IsStalenessReadOnly => {
                self.kv_snapshot
                    .set_is_staleness_read_only(assert_type::<bool>(&val, "IsStalenessReadOnly"));
            }
            o::MatchStoreLabels => {
                self.kv_snapshot
                    .set_match_store_labels(assert_type::<Vec<metapb::StoreLabel>>(
                        &val,
                        "MatchStoreLabels",
                    ));
            }
            o::ResourceGroupTag => {
                self.kv_snapshot
                    .set_resource_group_tag(assert_type::<Vec<u8>>(&val, "ResourceGroupTag"));
            }
            o::ResourceGroupTagger => {
                // tikvrpc.ResourceGroupTagger / *kv.ResourceGroupTagBuilder: no Rust type yet.
            }
            o::ReadReplicaScope => {
                self.kv_snapshot
                    .set_read_replica_scope(assert_type::<String>(&val, "ReadReplicaScope"));
            }
            o::SnapInterceptor => {
                *self.interceptor.write() = Some(assert_type::<Arc<dyn SnapshotInterceptor>>(
                    &val,
                    "SnapInterceptor",
                ));
            }
            o::RPCInterceptor => {
                // interceptor.RPCInterceptor: client-rust has no RPC interceptors.
            }
            o::RequestSourceInternal => {
                self.kv_snapshot
                    .set_request_source_internal(assert_type::<bool>(
                        &val,
                        "RequestSourceInternal",
                    ));
            }
            o::RequestSourceType => {
                self.kv_snapshot
                    .set_request_source_type(assert_type::<String>(&val, "RequestSourceType"));
            }
            o::ExplicitRequestSourceType => {
                self.kv_snapshot
                    .set_explicit_request_source_type(assert_type::<String>(
                        &val,
                        "ExplicitRequestSourceType",
                    ));
            }
            o::ReplicaReadAdjuster => {
                // txnkv.ReplicaReadAdjuster: no replica read in client-rust.
            }
            o::ScanBatchSize => {
                let size = assert_type::<isize>(&val, "ScanBatchSize");
                if size > 0 {
                    self.kv_snapshot.set_scan_batch_size(size);
                }
            }
            o::ResourceGroupName => {
                self.kv_snapshot
                    .set_resource_group_name(assert_type::<String>(&val, "ResourceGroupName"));
            }
            o::LoadBasedReplicaReadThreshold => {
                self.kv_snapshot
                    .set_load_based_replica_read_threshold(assert_type::<Duration>(
                        &val,
                        "LoadBasedReplicaReadThreshold",
                    ));
            }
            o::TiKVClientReadTimeout => {
                let ms = assert_type::<u64>(&val, "TiKVClientReadTimeout");
                self.kv_snapshot
                    .set_kv_read_timeout(Duration::from_millis(ms));
            }
            _ => {}
        }
    }
}

/// `toTiKVKeys`: Go reinterprets `[]kv.Key` as `[][]byte`; here the keys are copied.
// go: toTiKVKeys L2 (copies the keys; Go reinterprets the slice with unsafe)
fn to_tikv_keys(keys: &[Key]) -> Vec<Vec<u8>> {
    keys.iter().map(|k| k.0.clone()).collect()
}

// go: getTiKVIsolationLevel L1
fn get_tikv_isolation_level(level: IsoLevel) -> tikv::IsoLevel {
    match level {
        tidb_kv::kv::SI => tikv::IsoLevel::SI,
        tidb_kv::kv::RC => tikv::IsoLevel::RC,
        tidb_kv::kv::RCCheckTS => tikv::IsoLevel::RCCheckTS,
        _ => tikv::IsoLevel::SI,
    }
}

// go: getTiKVPriority L1
fn get_tikv_priority(pri: isize) -> tikv::Priority {
    match pri {
        tidb_kv::kv::PriorityHigh => tikv::Priority::High,
        tidb_kv::kv::PriorityLow => tikv::Priority::Low,
        _ => tikv::Priority::Normal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolation_level_and_priority() {
        assert_eq!(
            get_tikv_isolation_level(tidb_kv::kv::RC),
            tikv::IsoLevel::RC
        );
        assert_eq!(
            get_tikv_isolation_level(tidb_kv::kv::RCCheckTS),
            tikv::IsoLevel::RCCheckTS
        );
        assert_eq!(get_tikv_isolation_level(IsoLevel(9)), tikv::IsoLevel::SI);
        assert_eq!(
            get_tikv_priority(tidb_kv::kv::PriorityHigh),
            tikv::Priority::High
        );
        assert_eq!(
            get_tikv_priority(tidb_kv::kv::PriorityLow),
            tikv::Priority::Low
        );
        assert_eq!(get_tikv_priority(7), tikv::Priority::Normal);
    }
}
