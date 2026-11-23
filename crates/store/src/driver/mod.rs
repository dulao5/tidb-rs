//! go-file: pkg/store/driver/tikv_driver.go
//!
//! `pkg/store/driver` (block D07.1): TiDB's `kv.Storage` over the TiKV client. Part 1
//! (VS1-18, `specs/VS1-vertical-slice/plan.md` §5): the snapshot's point reads
//! (`Get` / `BatchGet` / `SetOption`), the error conversion and the backoffer, which the
//! point get executor needs. The store as `kv.Storage` with `Begin` / `GetOracle` and the
//! read part of the transaction (VS1-20, a read-only autocommit statement on the new
//! stack). The rest of the store (the driver's `Open`, writing transactions, iterators,
//! etcd, the GC worker, `ShowStatus`) is part 2.
//!
//! client-go is the client-rust fork (L3, ADR 0008); [`tikv`] is the boundary.

pub mod backoff;
pub mod error;
pub mod tikv;
pub mod txn;

use std::sync::Arc;

use tidb_base::Error;
use tidb_kv::kv::{Client, KvFuture, Snapshot, Storage, Transaction, TxnOption};
use tidb_kv::version::{Version, new_version};
use tidb_runtime::pool::ExecPool;
use tikv_client::TransactionClient;
use tikv_client::proto::kvrpcpb;

use crate::copr::client_rust::ClientRustStore;
use crate::copr::coprocessor::CopClient;
use crate::copr::store::{Store as CoprStore, new_store as new_copr_store};
use error::to_tidb_err;
use tikv::{KVStore, PdOracle, TxnOptions};

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

impl TikvStore {
    /// Begin a global transaction.
    // go: tikvStore.Begin L1
    pub async fn begin(&self, opts: &[TxnOption]) -> Result<Box<dyn Transaction>, Error> {
        let mut options = TxnOptions::default();
        for opt in opts {
            match opt {
                TxnOption::StartTS(ts) => options.start_ts = Some(*ts),
                TxnOption::TxnScope(scope) => options.txn_scope = scope.clone(),
            }
        }
        let txn = self.kv_store.begin(options).await.map_err(to_tidb_err)?;
        Ok(Box::new(txn::txn_driver::new_tikv_txn(txn)))
    }

    /// GetOracle gets a timestamp oracle client.
    // glue: dispatch (client-go KVStore.GetOracle, promoted through the embedding)
    pub fn get_oracle(&self) -> PdOracle {
        self.kv_store.get_oracle()
    }
}

impl Storage for TikvStore {
    // glue: dispatch (Go tikvStore.Begin)
    fn begin<'a>(&'a self, opts: &'a [TxnOption]) -> KvFuture<'a, Box<dyn Transaction>> {
        Box::pin(TikvStore::begin(self, opts))
    }
    // glue: dispatch (Go tikvStore.GetSnapshot)
    fn get_snapshot(&self, ver: Version) -> Arc<dyn Snapshot> {
        TikvStore::get_snapshot(self, ver)
    }
    // glue: dispatch (Go tikvStore.GetClient)
    fn get_client(&self) -> Arc<dyn Client> {
        Arc::new(TikvStore::get_client(self))
    }
    /// Go's `Close` closes the coprocessor store, the client-go store (PD client, region
    /// cache, TSO), etcd and the GC worker. The client-rust client here is shared with the
    /// PoC's reads and is released when its last handle drops; there is no etcd or GC worker
    /// (the driver's `Open`, part 2). Nothing on the new stack closes the store.
    // go: tikvStore.Close L2 (the client-rust client is shared with the PoC and released on drop; no etcd or GC worker)
    fn close(&self) -> Result<(), Error> {
        Ok(())
    }
    /// client-go's `UUID()` is `tikv-<cluster id>`; client-rust does not expose the cluster
    /// ID. Nothing on the read path calls it. The trait returns a `String`, so the boundary is
    /// a panic carrying the `blocked` text: the connection loop catches it as Go's
    /// `clientConn.Run` does (1105 to the client, the connection closed; `tidb-mysql-protocol`
    /// `conn.rs`), it never reaches the router as a fallback.
    // glue: client-go KVStore.UUID (promoted) needs the PD cluster ID, blocked-by:D07.1 (client-rust does not expose it)
    fn uuid(&self) -> String {
        panic!(
            "{}",
            tidb_base::blocked::blocked("D07.1", "the store UUID (the PD cluster ID)")
        )
    }
    // glue: dispatch (Go tikvStore.CurrentVersion)
    fn current_version<'a>(&'a self, txn_scope: &'a str) -> KvFuture<'a, Version> {
        Box::pin(TikvStore::current_version(self, txn_scope))
    }
    // glue: dispatch (Go tikvStore.Name)
    fn name(&self) -> String {
        TikvStore::name(self)
    }
    // glue: dispatch (Go tikvStore.Describe)
    fn describe(&self) -> String {
        TikvStore::describe(self)
    }
    /// client-go keeps the minimal safe ts of each store, refreshed in the background
    /// (`updateSafeTS`); client-rust has none. Only stale reads ask, and those never reach
    /// the new stack (VS1 plan §4.3 step 0). As for `uuid`, the trait returns a value, so the
    /// boundary is a panic that the connection loop turns into 1105 and a closed connection.
    // glue: client-go KVStore.GetMinSafeTS (promoted) needs the stores' safe ts, blocked-by:D07.1 (part 2)
    fn get_min_safe_ts(&self, _txn_scope: &str) -> u64 {
        panic!(
            "{}",
            tidb_base::blocked::blocked("D07.1", "the minimal safe ts of the stores (stale read)")
        )
    }
}
