//! go-file: pkg/store/driver/txn/txn_driver.go
//!
//! `tikvTxn`: TiDB's `kv.Transaction` over client-go's `KVTxn` (here the client-rust boundary
//! [`KVTxn`]), the read part a read-only autocommit statement needs (VS1-20): begin at a
//! start ts, point reads through the snapshot, `SetOption` / `GetOption`, `Commit` and
//! `Rollback` of a transaction that wrote nothing.
//!
//! The transaction's MemDB is always empty: tidb-rs is a read-only node (constitution §4.3),
//! the Rust `kv.Transaction` has no `Set` / `Delete` / `LockKeys`, and the union store, the
//! committer, the mutation checker and fair locking are D07.1 part 2. So `Get` / `BatchGet`
//! find nothing in the buffer and read the snapshot, as Go does for a buffer without the key.
//! Iterators need the scanner (D07.1 part 2): boundary stubs.

use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_kv::kv::{
    BatchGetOption, BatchGetter, GetOption, Getter, IsoLevel, Iterator, KvFuture, OptionValue,
    Retriever, Snapshot, SnapshotInterceptor, Transaction, ValueEntry,
};
use tidb_kv::option::ReplicaReadType;
use tidb_kv::{Context, Key};
use tikv_client::proto::metapb;

use super::batch_getter::{BatchBufferGetter, new_buffer_batch_getter};
use super::error::extract_key_err;
use super::snapshot::{TikvSnapshot, assert_type, get_tikv_isolation_level, get_tikv_priority};
use crate::driver::tikv::KVTxn;

/// tikvTxn implements kv.Transaction.
///
/// Go's `idxNameCache`, `columnMapsCache`, `isCommitterWorking` and `memBuffer` serve the
/// write path (the mutation checker, the committer, the MemDB wrapper) and are not kept;
/// `GetOption(kv.TableToColumnMaps)` therefore answers nil.
pub struct TikvTxn {
    kv_txn: KVTxn,
    snapshot_interceptor: Option<Arc<dyn SnapshotInterceptor>>,
    /// The `GetOption` answers Go reads from the transaction (`RequestSourceInternal`,
    /// `RequestSourceType`: `KVTxn.RequestSource`, shared with its snapshot).
    request_source_internal: bool,
    request_source_type: String,
}

/// NewTiKVTxn returns a new Transaction.
///
/// `SetKVFilter(TiDBKVFilter{})` and the entry / total size limits only apply to the MemDB
/// of a writing transaction: not set (module doc).
// go: NewTiKVTxn L2 (the KV filter and size limits are write options of the MemDB)
pub fn new_tikv_txn(txn: KVTxn) -> TikvTxn {
    TikvTxn {
        kv_txn: txn,
        snapshot_interceptor: None,
        request_source_internal: false,
        request_source_type: String::new(),
    }
}

/// The empty MemDB of a read-only transaction (module doc): `Get` finds nothing, `BatchGet`
/// returns nothing.
// glue: the empty MemBuffer of the read-only transaction (unionstore, D07.1 part 2)
struct EmptyMemBuffer;

impl Getter for EmptyMemBuffer {
    // glue: MemBuffer.Get of an empty buffer
    fn get<'a>(
        &'a self,
        _: &'a Context,
        _: &'a Key,
        _: &'a [GetOption],
    ) -> KvFuture<'a, ValueEntry> {
        Box::pin(async { Err(tidb_kv::error::ErrNotExist.clone()) })
    }
}

impl BatchBufferGetter for EmptyMemBuffer {
    // glue: MemBuffer.Len of an empty buffer
    fn len(&self) -> isize {
        0
    }
    // glue: MemBuffer.BatchGet of an empty buffer
    fn batch_get<'a>(
        &'a self,
        _: &'a Context,
        _: &'a [Vec<u8>],
        _: &'a [BatchGetOption],
    ) -> BoxFuture<'a, Result<HashMap<Vec<u8>, ValueEntry>, Error>> {
        Box::pin(async { Ok(HashMap::new()) })
    }
}

impl TikvTxn {
    /// The snapshot as `kv.Snapshot` with the transaction's interceptor.
    // glue: GetSnapshot's concrete value (BatchGet / Get read through it)
    fn tikv_snapshot(&self) -> TikvSnapshot {
        let snap = super::snapshot::new_snapshot(Arc::clone(self.kv_txn.get_snapshot()));
        if let Some(i) = &self.snapshot_interceptor {
            snap.set_option(tidb_kv::option::SnapInterceptor, Box::new(Arc::clone(i)));
        }
        snap
    }
}

impl Retriever for TikvTxn {
    /// Iter creates an Iterator positioned on the first entry that k <= entry's key.
    /// If such entry is not found, it returns an invalid Iterator with no error.
    /// It yields only keys that < upperBound. If upperBound is nil, it means the upperBound is unbounded.
    /// The Iterator must be Closed after use.
    // glue: stub of tikvTxn.Iter, blocked-by:D07.1 (the union store's iterator over the scanner, part 2)
    fn iter<'a: 'f, 'f>(
        &'a self,
        _k: &'f Key,
        _upper_bound: &'f Key,
    ) -> KvFuture<'f, Box<dyn Iterator + 'a>> {
        Box::pin(async { Err(blocked("D07.1", "transaction iterators (D07.1 part 2)")) })
    }

    /// IterReverse creates a reversed Iterator positioned on the first entry which key is less
    /// than k. The returned iterator will iterate from greater key to smaller key.
    /// If k is nil, the returned iterator will be positioned at the last key.
    // glue: stub of tikvTxn.IterReverse, blocked-by:D07.1 (the union store's iterator over the scanner, part 2)
    fn iter_reverse<'a: 'f, 'f>(
        &'a self,
        _k: &'f Key,
        _lower_bound: &'f Key,
    ) -> KvFuture<'f, Box<dyn Iterator + 'a>> {
        Box::pin(async { Err(blocked("D07.1", "transaction iterators (D07.1 part 2)")) })
    }
}

impl BatchGetter for TikvTxn {
    /// BatchGet gets kv from the memory buffer of statement and transaction, and the kv
    /// storage. Do not use len(value) == 0 or value == nil to represent non-exist. If a key
    /// doesn't exist, there shouldn't be any corresponding entry in the result map.
    // go: tikvTxn.BatchGet L2 (the MemBuffer of a read-only transaction is always empty, module doc)
    fn batch_get<'a>(
        &'a self,
        ctx: &'a Context,
        keys: &'a [Key],
        options: &'a [BatchGetOption],
    ) -> KvFuture<'a, HashMap<Vec<u8>, ValueEntry>> {
        Box::pin(async move {
            let snapshot = self.tikv_snapshot();
            new_buffer_batch_getter(&EmptyMemBuffer, None, &snapshot)
                .batch_get(ctx, keys, options)
                .await
        })
    }
}

impl Getter for TikvTxn {
    // go: tikvTxn.Get L2 (the MemBuffer of a read-only transaction is always empty, module doc)
    fn get<'a>(
        &'a self,
        ctx: &'a Context,
        k: &'a Key,
        options: &'a [GetOption],
    ) -> KvFuture<'a, ValueEntry> {
        Box::pin(async move {
            let mut r = EmptyMemBuffer.get(ctx, k, options).await;
            if let Err(e) = &r
                && tidb_kv::error::ErrNotExist.equal(e)
            {
                r = self.tikv_snapshot().get(ctx, k, options).await;
            }
            match r {
                Ok(val) if val.value.is_empty() => Err(tidb_kv::error::ErrNotExist.clone()),
                r => r,
            }
        })
    }
}

impl Transaction for TikvTxn {
    // glue: dispatch (client-go KVTxn.Size, promoted through the embedding)
    fn size(&self) -> isize {
        self.kv_txn.size()
    }

    // glue: dispatch (client-go KVTxn.Len, promoted through the embedding)
    fn len(&self) -> isize {
        self.kv_txn.len()
    }

    /// The `intest` committer flag is not kept (module doc).
    // go: tikvTxn.Commit L1
    fn commit<'a>(&'a mut self, ctx: &'a Context) -> KvFuture<'a, ()> {
        Box::pin(async move {
            let err = self.kv_txn.commit(ctx).await;
            err.map_err(|e| self.extract_key_err(e))
        })
    }

    // glue: dispatch (client-go KVTxn.Rollback, promoted through the embedding; its error through derr.ToTiDBErr)
    fn rollback(&mut self) -> Result<(), Error> {
        self.kv_txn
            .rollback()
            .map_err(crate::driver::error::to_tidb_err)
    }

    // glue: dispatch (client-go KVTxn.String, promoted through the embedding)
    fn string(&self) -> String {
        self.kv_txn.string()
    }

    /// The option values have the Rust types of [`TikvSnapshot`]'s `SetOption` (its doc),
    /// plus `bool` / `String` / `u64` for the transaction's own. The options of the commit
    /// and of the MemDB (`SchemaChecker`, `InfoSchema`, `CommitHook`, `EnableAsyncCommit`,
    /// `Enable1PC`, `KVFilter`, `CommitTSUpperBoundCheck`, `AssertionLevel`,
    /// `TableToColumnMaps`, `TxnSource`, `SizeLimits`, `SessionID`, the lifecycle hooks and
    /// the prewrite lock policy) are accepted and not kept: there is no commit of a write
    /// (module doc). The RPC interceptor, the resource group tagger and the replica read
    /// adjuster have no Rust type (as in the snapshot's `SetOption`).
    // go: tikvTxn.SetOption L2 (the commit and MemDB options are not kept: no writes)
    fn set_option(&mut self, opt: isize, val: OptionValue) {
        use tidb_kv::option as o;
        let snap = Arc::clone(self.kv_txn.get_snapshot());
        match opt {
            o::IsolationLevel => {
                let level =
                    get_tikv_isolation_level(assert_type::<IsoLevel>(&val, "IsolationLevel"));
                snap.set_isolation_level(level);
            }
            o::Priority => {
                self.kv_txn
                    .set_priority(get_tikv_priority(assert_type::<isize>(&val, "Priority")));
            }
            o::NotFillCache => snap.set_not_fill_cache(assert_type::<bool>(&val, "NotFillCache")),
            o::Pessimistic => self
                .kv_txn
                .set_pessimistic(assert_type::<bool>(&val, "Pessimistic")),
            o::SnapshotTS => snap.set_snapshot_ts(assert_type::<u64>(&val, "SnapshotTS")),
            o::ReplicaRead => {
                // options.GetTiKVReplicaReadType: client-rust has no replica read, the
                // snapshot keeps TiDB's type (as the snapshot's SetOption).
                snap.set_replica_read(assert_type::<ReplicaReadType>(&val, "ReplicaRead"));
            }
            o::TaskID => snap.set_task_id(assert_type::<u64>(&val, "TaskID")),
            o::CollectRuntimeStats => {
                let stats = assert_type::<Option<()>>(&val, "CollectRuntimeStats");
                snap.set_runtime_stats(stats.is_some());
            }
            o::SampleStep => snap.set_sample_step(assert_type::<u32>(&val, "SampleStep")),
            o::GuaranteeLinearizability => {
                let b = assert_type::<bool>(&val, "GuaranteeLinearizability");
                self.kv_txn.set_causal_consistency(!b);
            }
            o::TxnScope => self
                .kv_txn
                .set_scope(assert_type::<String>(&val, "TxnScope")),
            o::IsStalenessReadOnly => {
                snap.set_is_staleness_read_only(assert_type::<bool>(&val, "IsStalenessReadOnly"));
            }
            o::MatchStoreLabels => snap.set_match_store_labels(assert_type::<
                Vec<metapb::StoreLabel>,
            >(
                &val, "MatchStoreLabels"
            )),
            o::ResourceGroupTag => self
                .kv_txn
                .set_resource_group_tag(assert_type::<Vec<u8>>(&val, "ResourceGroupTag")),
            o::SnapInterceptor => {
                self.snapshot_interceptor = Some(assert_type::<Arc<dyn SnapshotInterceptor>>(
                    &val,
                    "SnapInterceptor",
                ));
            }
            o::RequestSourceInternal => {
                let b = assert_type::<bool>(&val, "RequestSourceInternal");
                self.request_source_internal = b;
                self.kv_txn.set_request_source_internal(b);
            }
            o::RequestSourceType => {
                let tp = assert_type::<String>(&val, "RequestSourceType");
                self.request_source_type = tp.clone();
                self.kv_txn.set_request_source_type(tp);
            }
            o::ExplicitRequestSourceType => {
                self.kv_txn
                    .set_explicit_request_source_type(assert_type::<String>(
                        &val,
                        "ExplicitRequestSourceType",
                    ))
            }
            o::ResourceGroupName => self
                .kv_txn
                .set_resource_group_name(assert_type::<String>(&val, "ResourceGroupName")),
            o::LoadBasedReplicaReadThreshold => snap.set_load_based_replica_read_threshold(
                assert_type::<Duration>(&val, "LoadBasedReplicaReadThreshold"),
            ),
            o::TiKVClientReadTimeout => {
                let ms = assert_type::<u64>(&val, "TiKVClientReadTimeout");
                snap.set_kv_read_timeout(Duration::from_millis(ms));
            }
            _ => {}
        }
    }

    /// `GuaranteeLinearizability` and `TxnScope` are answered from the transaction;
    /// `TableToColumnMaps` is the mutation checker's cache (not kept, nil). The Rust trait
    /// returns a reference, so the bools are static values.
    // go: tikvTxn.GetOption L2 (TableToColumnMaps is never set: no writes)
    fn get_option(&self, opt: isize) -> Option<&(dyn Any + Send + Sync)> {
        use tidb_kv::option as o;
        static TRUE: bool = true;
        static FALSE: bool = false;
        let b = |v: bool| -> &'static (dyn Any + Send + Sync) { if v { &TRUE } else { &FALSE } };
        match opt {
            o::GuaranteeLinearizability => Some(b(!self.kv_txn.is_casual_consistency())),
            o::TxnScope => Some(self.kv_txn.get_scope()),
            o::TableToColumnMaps => None,
            o::RequestSourceInternal => Some(b(self.request_source_internal)),
            o::RequestSourceType => Some(&self.request_source_type),
            _ => None,
        }
    }

    // glue: dispatch (client-go KVTxn.IsReadOnly, promoted through the embedding)
    fn is_read_only(&self) -> bool {
        self.kv_txn.is_read_only()
    }

    // glue: dispatch (client-go KVTxn.StartTS, promoted through the embedding)
    fn start_ts(&self) -> u64 {
        self.kv_txn.start_ts()
    }

    // glue: dispatch (client-go KVTxn.Valid, promoted through the embedding)
    fn valid(&self) -> bool {
        self.kv_txn.valid()
    }

    /// GetSnapshot returns the Snapshot binding to this transaction.
    // go: tikvTxn.GetSnapshot L1
    fn get_snapshot(&self) -> Arc<dyn Snapshot> {
        Arc::new(self.tikv_snapshot())
    }

    // glue: dispatch (client-go KVTxn.IsPessimistic, promoted through the embedding)
    fn is_pessimistic(&self) -> bool {
        self.kv_txn.is_pessimistic()
    }
}

impl TikvTxn {
    /// `ErrKeyExist` (the write path's duplicate key) cannot come from a read-only commit;
    /// every other error goes through `derr.ToTiDBErr`.
    // go: tikvTxn.extractKeyErr L1
    fn extract_key_err(&self, err: crate::driver::error::TiKVError) -> Error {
        extract_key_err(err)
    }
}
