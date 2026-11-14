//! The client-rust implementation of the copr boundary ([`TiKVRegionCache`],
//! [`TiKVStore`], [`ClientHelper`], [`Backoffer`]): ADR 0008, client-rust#22
//! (`tikv_client::CopSender`).
//!
//! Errors from client-rust become TiDB errors here: the D07.1 port of
//! `pkg/store/driver/error.ToTiDBErr` does not exist yet, so they are plain errors carrying
//! client-rust's message (spec §6).

use std::sync::Arc;
use std::time::Duration;

use tidb_base::errors::Error;
use tidb_kv::KeyRange;
use tidb_kv::option::ReplicaReadType;
use tikv_client::proto::{coprocessor, kvrpcpb};
use tikv_client::{
    Backoff, CopSender, Timestamp, TimestampExt, TransactionClient, TransactionOptions,
};

use super::tikv::{
    Backoffer, ClientHelper, KeyLocation, RPCContext, RegionVerID, TiKVRegionCache, TiKVStore,
    Variables,
};

/// `derr.ToTiDBErr` until D07.1.
// glue: client-rust error to TiDB error (D07.1 driver/error)
fn to_tidb_err(e: tikv_client::Error) -> Error {
    Error::plain(e.to_string())
}

// glue: RegionVerID conversion between the copr boundary and client-rust
fn to_rust_ver(r: RegionVerID) -> tikv_client::RegionVerId {
    tikv_client::RegionVerId {
        id: r.id,
        conf_ver: r.conf_ver,
        ver: r.ver,
    }
}

// glue: RegionVerID conversion between client-rust and the copr boundary
fn from_rust_ver(r: &tikv_client::RegionVerId) -> RegionVerID {
    RegionVerID::new(r.id, r.conf_ver, r.ver)
}

// glue: client-rust CopRpcContext to the copr boundary's RPCContext
fn from_rust_ctx(c: tikv_client::transaction::CopRpcContext) -> RPCContext {
    RPCContext {
        region: from_rust_ver(&c.ver_id),
        peer: c.peer,
        addr: c.addr,
        store_id: c.store_id,
        store_estimated_wait_time: c.store_estimated_wait_time,
    }
}

/// TiDB's `backoff.Backoffer` over client-rust's time-budget [`Backoff`].
pub struct ClientRustBackoffer(pub Backoff);

impl Backoffer for ClientRustBackoffer {
    // glue: backoff.Backoffer.Backoff(BoRegionMiss) on client-rust's Backoff (D07.1)
    async fn backoff_region_miss(&mut self, err: Error) -> Result<(), Error> {
        self.0
            .backoff(
                tikv_client::metrics::BackoffType::RegionMiss,
                tikv_client::Error::StringError(err.to_string()),
            )
            .await
            .map_err(to_tidb_err)
    }

    // glue: backoff.Backoffer.BackoffWithMaxSleepTxnLockFast on client-rust's Backoff (D07.1)
    async fn backoff_with_max_sleep_txn_lock_fast(
        &mut self,
        max_sleep_ms: i64,
        err: Error,
    ) -> Result<(), Error> {
        self.0
            .backoff_with_max_sleep_txn_lock_fast(
                max_sleep_ms.max(0) as u64,
                tikv_client::Error::StringError(err.to_string()),
            )
            .await
            .map_err(to_tidb_err)
    }

    // glue: backoff.Backoffer.GetTotalSleep on client-rust's Backoff (D07.1)
    fn get_total_sleep(&self) -> i64 {
        self.0.total_sleep_ms() as i64
    }
}

/// The TiKV store as copr sees it: a client-rust transaction client.
pub struct ClientRustStore {
    client: Arc<TransactionClient>,
    /// The sender used for region-cache calls (any timestamp: they do not read data).
    cache_sender: CopSender,
}

impl ClientRustStore {
    // glue: constructs the boundary over a client-rust TransactionClient
    pub fn new(client: Arc<TransactionClient>) -> Result<ClientRustStore, Error> {
        let cache_sender = client
            .snapshot(
                Timestamp::from_version(0),
                TransactionOptions::new_optimistic().read_only(),
            )
            .cop_sender()
            .map_err(to_tidb_err)?;
        Ok(ClientRustStore {
            client,
            cache_sender,
        })
    }
}

impl TiKVRegionCache for ClientRustStore {
    type Backoffer = ClientRustBackoffer;

    // glue: RegionCache.BatchLocateKeyRanges over client-rust (no buckets, parity §2)
    async fn batch_locate_key_ranges(
        &self,
        bo: &mut ClientRustBackoffer,
        key_ranges: &[KeyRange],
        _need_leader: bool,
        _need_buckets: bool,
    ) -> Result<Vec<KeyLocation>, Error> {
        let ranges: Vec<(Vec<u8>, Vec<u8>)> = key_ranges
            .iter()
            .map(|r| (r.start_key.0.clone(), r.end_key.0.clone()))
            .collect();
        let locs = self
            .cache_sender
            .locate_key_ranges(&mut bo.0, &ranges)
            .await
            .map_err(to_tidb_err)?;
        Ok(locs
            .into_iter()
            .map(|l| KeyLocation {
                region: from_rust_ver(&l.ver_id),
                start_key: l.start_key,
                end_key: l.end_key,
                buckets: None,
            })
            .collect())
    }

    // glue: RegionCache.GetTiKVRPCContext over client-rust (leader only)
    async fn get_tikv_rpc_context(
        &self,
        _bo: &mut ClientRustBackoffer,
        id: RegionVerID,
        _replica_read: ReplicaReadType,
        _follower_store_seed: u32,
    ) -> Result<Option<RPCContext>, Error> {
        Ok(self
            .cache_sender
            .rpc_context(&to_rust_ver(id))
            .await
            .map_err(to_tidb_err)?
            .map(from_rust_ctx))
    }
}

/// One iterator's client helper: a client-rust [`CopSender`] at the iterator's timestamp,
/// whose lock hints are the iterator's resolved / committed sets.
pub struct ClientRustHelper(CopSender);

impl ClientHelper for ClientRustHelper {
    type Backoffer = ClientRustBackoffer;

    // glue: ClientHelper.SendReqCtx over client-rust CopSender::send
    async fn send_req_ctx(
        &self,
        bo: &mut ClientRustBackoffer,
        req: coprocessor::Request,
        region: RegionVerID,
        timeout: Duration,
    ) -> Result<(coprocessor::Response, Option<RPCContext>, String), Error> {
        let r = self
            .0
            .send(&mut bo.0, &to_rust_ver(region), req, timeout)
            .await
            .map_err(to_tidb_err)?;
        let ctx = r.rpc_ctx.map(from_rust_ctx);
        let addr = ctx.as_ref().map(|c| c.addr.clone()).unwrap_or_default();
        Ok((r.resp, ctx, addr))
    }

    // glue: ClientHelper.ResolveLocksWithOpts + BackoffWithMaxSleepTxnLockFast over client-rust
    async fn resolve_lock_and_wait(
        &self,
        bo: &mut ClientRustBackoffer,
        lock_info: kvrpcpb::LockInfo,
    ) -> Result<(), Error> {
        self.0
            .resolve_lock(&mut bo.0, lock_info)
            .await
            .map_err(to_tidb_err)
    }
}

impl TiKVStore for ClientRustStore {
    type Helper = ClientRustHelper;

    /// client-rust's lock hints start empty: a lock of the reader's own `start_ts` (only
    /// pipelined DML writes those, a write path) is not ignored (spec §6).
    // glue: NewClientHelper over a client-rust snapshot at start_ts
    fn new_client_helper(&self, start_ts: u64, _resolved_start_ts: bool) -> ClientRustHelper {
        let sender = self
            .client
            .snapshot(
                Timestamp::from_version(start_ts),
                TransactionOptions::new_optimistic().read_only(),
            )
            .cop_sender()
            .expect("keyspace disabled");
        ClientRustHelper(sender)
    }

    // glue: backoff.NewBackofferWithVars over client-rust's time budget (D07.1)
    fn new_backoffer(&self, max_sleep_ms: i64, vars: &Variables) -> ClientRustBackoffer {
        let ms = (max_sleep_ms * vars.backoff_weight.max(1)).max(0) as u64;
        ClientRustBackoffer(Backoff::time_budget(ms).start())
    }

    /// client-rust has no GC safe-point check yet: always visible (spec §6).
    // glue: KVStore.CheckVisibility (no safe-point cache in client-rust)
    fn check_visibility(&self, _start_ts: u64) -> Result<(), Error> {
        Ok(())
    }
}
