//! go-file: pkg/store/driver/tikv_driver.go
//!
//! `pkg/store/driver` (block D07.1): TiDB's `kv.Storage` over the TiKV client. Part 1
//! (VS1-18, `specs/VS1-vertical-slice/plan.md` §5): the snapshot's point reads
//! (`Get` / `BatchGet` / `SetOption`), the error conversion and the backoffer, which the
//! point get executor needs. The rest of the store (the driver's `Open`, transactions,
//! iterators, etcd, the GC worker, `ShowStatus`) is part 2.
//!
//! client-go is the client-rust fork (L3, ADR 0008); [`tikv`] is the boundary.

pub mod backoff;
pub mod error;
pub mod tikv;
pub mod txn;

use std::sync::Arc;

use tidb_base::Error;
use tidb_kv::kv::Snapshot;
use tidb_kv::version::{Version, new_version};
use tidb_runtime::pool::ExecPool;
use tikv_client::TransactionClient;
use tikv_client::proto::kvrpcpb;

use crate::copr::client_rust::ClientRustStore;
use crate::copr::coprocessor::CopClient;
use crate::copr::store::{Store as CoprStore, new_store as new_copr_store};
use error::to_tidb_err;
use tikv::KVStore;

/// client-go `tikv.Codec` for the API version tidb-rs runs on: API v1 without a keyspace
/// (keyspaces are #128 item 6). Keys and requests are not rewritten.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Codec {
    /// `GetAPIVersion()`.
    pub api_version: kvrpcpb::ApiVersion,
}

/// The TiKV store (Go `tikvStore`): the client-go `KVStore` and the coprocessor store.
/// Only the fields part 1 reads; etcd, TLS, the GC worker and the memory cache are part 2.
pub struct TikvStore {
    kv_store: Arc<KVStore>,
    copr_store: Arc<CoprStore<ClientRustStore>>,
    codec: Codec,
}

impl TikvStore {
    /// A store over a connected client-rust client; Go's `TiKVDriver.OpenWithOptions`
    /// (part 2) builds it from a path.
    // glue: stand-in for TiKVDriver.OpenWithOptions (D07.1 part 2)
    pub fn new(client: Arc<TransactionClient>, pool: Option<ExecPool>) -> Result<TikvStore, Error> {
        let copr = ClientRustStore::new(client.clone())?;
        Ok(TikvStore {
            kv_store: KVStore::new(client),
            copr_store: Arc::new(new_copr_store(Arc::new(copr), pool, 0)),
            codec: Codec {
                api_version: kvrpcpb::ApiVersion::V1,
            },
        })
    }

    /// Name gets the name of the storage engine
    // go: tikvStore.Name L1
    pub fn name(&self) -> String {
        "TiKV".to_owned()
    }

    /// Describe returns of brief introduction of the storage
    // go: tikvStore.Describe L1
    pub fn describe(&self) -> String {
        "TiKV is a distributed transactional key-value database".to_owned()
    }

    // go: tikvStore.GetClient L1
    pub fn get_client(&self) -> CopClient<ClientRustStore> {
        self.copr_store.get_client()
    }

    /// GetSnapshot gets a snapshot that is able to read any data which data is <= ver.
    /// if ver is MaxVersion or > current max committed version, we will use current version
    /// for this snapshot.
    // go: tikvStore.GetSnapshot L1
    pub fn get_snapshot(&self, ver: Version) -> Arc<dyn Snapshot> {
        Arc::new(txn::snapshot::new_snapshot(
            self.kv_store.get_snapshot(ver.ver),
        ))
    }

    /// CurrentVersion returns current max committed version with the given txnScope (local
    /// or global).
    // go: tikvStore.CurrentVersion L1 [M-ERRVAL]
    pub async fn current_version(&self, txn_scope: &str) -> Result<Version, Error> {
        let ver = self.kv_store.current_timestamp(txn_scope).await;
        ver.map(new_version).map_err(to_tidb_err)
    }

    // go: tikvStore.GetCodec L1
    pub fn get_codec(&self) -> Codec {
        self.codec
    }

    /// The client-go `KVStore` (Go's embedded `*tikv.KVStore`).
    // glue: accessor of the embedded KVStore
    pub fn kv_store(&self) -> &Arc<KVStore> {
        &self.kv_store
    }
}
