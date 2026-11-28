//! The client-rust implementation of the copr boundary ([`TiKVRegionCache`],
//! [`TiKVStore`], [`ClientHelper`], [`Backoffer`]): ADR 0008, client-rust#22
//! (`tikv_client::CopSender`).
//!
//! Errors from client-rust become TiDB errors here: the D07.1 port of
//! `pkg/store/driver/error.ToTiDBErr` does not exist yet, so they are plain errors carrying
//! client-rust's message (spec §6).

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tidb_base::errors::Error;
use tidb_kv::KeyRange;
use tidb_kv::option::ReplicaReadType;
use tidb_memory::sqlkiller::SqlKiller;
use tidb_runtime::KillCancel;
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

/// TiDB's `backoff.Backoffer` over client-rust's time-budget [`Backoff`] with session
/// cancellation support.
pub struct ClientRustBackoffer {
    pub bo: Backoff,
    pub kill_cancel: Option<KillCancel>,
}

impl Backoffer for ClientRustBackoffer {
    // glue: backoff.Backoffer.Backoff(BoRegionMiss) on client-rust's Backoff (D07.1)
    async fn backoff_region_miss(&mut self, err: Error) -> Result<(), Error> {
        let kill_cancel = self.kill_cancel.clone();
        let fut = self.bo.backoff(
            tikv_client::metrics::BackoffType::RegionMiss,
            tikv_client::Error::StringError(err.to_string()),
        );
        run_with_cancel(kill_cancel.as_ref(), fut).await
    }

    // glue: backoff.Backoffer.BackoffWithMaxSleepTxnLockFast on client-rust's Backoff (D07.1)
    async fn backoff_with_max_sleep_txn_lock_fast(
        &mut self,
        max_sleep_ms: i64,
        err: Error,
    ) -> Result<(), Error> {
        let kill_cancel = self.kill_cancel.clone();
        let fut = self.bo.backoff_with_max_sleep_txn_lock_fast(
            max_sleep_ms.max(0) as u64,
            tikv_client::Error::StringError(err.to_string()),
        );
        run_with_cancel(kill_cancel.as_ref(), fut).await
    }

    // glue: backoff.Backoffer.GetTotalSleep on client-rust's Backoff (D07.1)
    fn get_total_sleep(&self) -> i64 {
        self.bo.total_sleep_ms() as i64
    }
}

/// Awaits `fut`, dropping the underlying `client-rust` RPC or backoff future immediately
/// when `kill_cancel` triggers (`Entry::is_canceled()` + tonic stream drop).
// glue: N3 §3.3 cancellation wrapper around client-rust async calls
async fn run_with_cancel<T, F>(kill_cancel: Option<&KillCancel>, fut: F) -> Result<T, Error>
where
    F: Future<Output = Result<T, tikv_client::Error>>,
{
    match kill_cancel {
        Some(kc) => {
            tokio::select! {
                biased;
                _ = kc.cancelled() => Err(SqlKiller::signal_to_error(kc.signal(), 0)),
                res = fut => res.map_err(to_tidb_err),
            }
        }
        None => fut.await.map_err(to_tidb_err),
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
        let kill_cancel = bo.kill_cancel.clone();
        let fut = self.cache_sender.locate_key_ranges(&mut bo.bo, &ranges);
        let locs = run_with_cancel(kill_cancel.as_ref(), fut).await?;
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
        bo: &mut ClientRustBackoffer,
        id: RegionVerID,
        _replica_read: ReplicaReadType,
        _follower_store_seed: u32,
    ) -> Result<Option<RPCContext>, Error> {
        let ver = to_rust_ver(id);
        let kill_cancel = bo.kill_cancel.clone();
        let fut = self.cache_sender.rpc_context(&ver);
        let ctx = run_with_cancel(kill_cancel.as_ref(), fut).await?;
        Ok(ctx.map(from_rust_ctx))
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
        let ver = to_rust_ver(region);
        let kill_cancel = bo.kill_cancel.clone();
        let fut = self.0.send(&mut bo.bo, &ver, req, timeout);
        let r = run_with_cancel(kill_cancel.as_ref(), fut).await?;
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
        let kill_cancel = bo.kill_cancel.clone();
        let fut = self.0.resolve_lock(&mut bo.bo, lock_info);
        run_with_cancel(kill_cancel.as_ref(), fut).await
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
        ClientRustBackoffer {
            bo: Backoff::time_budget(ms).start(),
            kill_cancel: vars.kill_cancel.clone(),
        }
    }

    /// Boundary stub: client-rust has no GC safe-point cache yet, so a read below the safe
    /// point does not fail with `ErrGCTooEarly` (client-go parity table §4, `CheckVisibility`
    /// row, P1).
    // glue: stub, blocked-by:client-rust GC safe point (client-go-parity.md §4 CheckVisibility)
    fn check_visibility(&self, _start_ts: u64) -> Result<(), Error> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn client_rust_backoffer_cancels_on_kill() {
        let killer = SqlKiller::new();
        let kc = KillCancel::new(killer.signal_handle());
        let kc2 = kc.clone();
        let k2 = Arc::clone(&killer);
        killer.set_on_kill(Some(Box::new(move || kc2.notify())));
        let mut bo = ClientRustBackoffer {
            bo: Backoff::time_budget(60_000).start(),
            kill_cancel: Some(kc),
        };
        // Advance past initial 1-2ms sleeps so the tested backoff sleeps long enough to race kill.
        for _ in 0..7 {
            let _ = bo.bo.next_delay_duration();
        }
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            k2.send_kill_signal(tidb_memory::sqlkiller::QueryInterrupted);
        });
        let start = std::time::Instant::now();
        let mut err = None;
        for _ in 0..32 {
            if let Err(e) = bo.backoff_region_miss(Error::plain("region miss")).await {
                err = Some(e);
                break;
            }
        }
        let err = err.expect("backoff loop must be interrupted by KillCancel");
        assert_eq!(err.code(), tidb_base::errno::ErrQueryInterrupted as isize);
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
