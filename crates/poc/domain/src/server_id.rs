//! Cluster-unique `server_id` lease management over PD's embedded etcd
//! (`/tidb/server_id/<id>`, Go `pkg/domain/domain.go:3123-3415`).

use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use etcd_client::{Client as EtcdClient, Compare, CompareOp, GetOptions, PutOptions, Txn, TxnOp};
use tidb_sessionctx::globalconn::{MAX_SERVER_ID_32, MAX_SERVER_ID_64};
use tokio::sync::Mutex;

use crate::{Error, Result, ServerInfo, keys};

/// Default TTL of `/tidb/server_id/<id>` lease (Go `serverIDTTL = 12 * time.Hour`).
pub const SERVER_ID_TTL: Duration = Duration::from_secs(12 * 3600);
/// Default keepalive interval (Go `serverIDTimeToKeepAlive = 5 * time.Minute`).
pub const SERVER_ID_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// Default PD reconnect check interval (Go `serverIDTimeToCheckPDConnectionRestored = 10s`).
pub const SERVER_ID_CHECK_RESTORED_INTERVAL: Duration = Duration::from_secs(10);
/// Disconnect threshold before kicking connections (Go `lostConnectionToPDTimeout = 6h`).
pub const LOST_CONNECTION_TO_PD_TIMEOUT: Duration = Duration::from_secs(6 * 3600);

const ACQUIRE_SERVER_ID_TIMEOUT: Duration = Duration::from_secs(10);
const ACQUIRE_SERVER_ID_RETRY_INTERVAL: Duration = Duration::from_millis(300);
const RETRIEVE_SERVER_ID_SESSION_TIMEOUT: Duration = Duration::from_secs(10);
const ACQUIRE_32BITS_SERVER_ID_RETRY_CNT: usize = 3;
const REFRESH_SERVER_ID_RETRY_CNT: usize = 3;
const LEASE_KEEPALIVE_STREAM_TIMEOUT: Duration = Duration::from_secs(5);

/// Timing parameters for `/tidb/server_id/<id>` lease maintenance.
#[derive(Clone, Copy, Debug)]
pub struct ServerIdTiming {
    pub ttl: Duration,
    pub keepalive_interval: Duration,
    pub check_restored_interval: Duration,
    pub lost_connection_to_pd_timeout: Duration,
}

impl Default for ServerIdTiming {
    fn default() -> Self {
        Self {
            ttl: SERVER_ID_TTL,
            keepalive_interval: SERVER_ID_KEEPALIVE_INTERVAL,
            check_restored_interval: SERVER_ID_CHECK_RESTORED_INTERVAL,
            lost_connection_to_pd_timeout: LOST_CONNECTION_TO_PD_TIMEOUT,
        }
    }
}

/// Callback notified whenever the acquired `server_id` or its PD lease validity changes.
pub type ServerIdChangeFn = Arc<dyn Fn(u64, bool) + Send + Sync>;
/// Callback invoked to terminate all client connections when PD lease connectivity is lost.
pub type KillAllConnectionsFn = Arc<dyn Fn() + Send + Sync>;

/// Result of one PD keepalive probe in the `server_id` keeper state machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeeperAction {
    /// Keepalive succeeded; validity stays active.
    Healthy,
    /// Keepalive failed, still within `lost_connection_to_pd_timeout`.
    TransientFailure,
    /// PD connectivity exceeded `lost_connection_to_pd_timeout`; connections killed.
    LostConnectionToPd,
    /// Connectivity restored and a new `server_id` lease was acquired.
    RestoredConnectionToPd,
}

/// Pure timeout state machine for Go's `Domain.serverIDKeeper`.
pub struct ServerIdWatchdog {
    lost_connection_to_pd: bool,
    last_succeed: Instant,
    lost_timeout: Duration,
}

impl ServerIdWatchdog {
    pub fn new(now: Instant, lost_timeout: Duration) -> Self {
        Self {
            lost_connection_to_pd: false,
            last_succeed: now,
            lost_timeout,
        }
    }

    pub fn is_lost(&self) -> bool {
        self.lost_connection_to_pd
    }

    /// Updates state after a keepalive attempt completes.
    pub fn on_keepalive_result(&mut self, ok: bool, now: Instant) -> KeeperAction {
        if self.lost_connection_to_pd {
            if ok {
                self.lost_connection_to_pd = false;
                self.last_succeed = now;
                return KeeperAction::RestoredConnectionToPd;
            }
            return KeeperAction::TransientFailure;
        }
        if ok {
            self.last_succeed = now;
            return KeeperAction::Healthy;
        }
        if !self.lost_timeout.is_zero()
            && now.saturating_duration_since(self.last_succeed) > self.lost_timeout
        {
            self.lost_connection_to_pd = true;
            return KeeperAction::LostConnectionToPd;
        }
        KeeperAction::TransientFailure
    }
}

/// Chooses the next `server_id` candidate matching Go's `Domain.proposeServerID`.
// go: Domain.proposeServerID L1
pub fn propose_server_id_from_used(
    used_32: &HashSet<u64>,
    conflict_cnt: usize,
    enable_32bits: bool,
) -> u64 {
    if !enable_32bits {
        return random_u64_in(1, MAX_SERVER_ID_64);
    }
    if conflict_cnt < ACQUIRE_32BITS_SERVER_ID_RETRY_CNT {
        if (used_32.len() as f32) <= 0.9 * (MAX_SERVER_ID_32 as f32) {
            for _ in 0..15 {
                let candidate = random_u64_in(1, MAX_SERVER_ID_32);
                if !used_32.contains(&candidate) {
                    return candidate;
                }
            }
        }
        tidb_base::log_info!(
            "upgrade to 64 bits server ID due to used up",
            "len(allServerInfo)" => used_32.len(),
        );
    } else {
        tidb_base::log_info!(
            "upgrade to 64 bits server ID due to conflict",
            "conflictCnt" => conflict_cnt,
        );
    }
    random_u64_in(MAX_SERVER_ID_32 + 1, MAX_SERVER_ID_64)
}

fn random_u64_in(min: u64, max: u64) -> u64 {
    debug_assert!(min <= max);
    let span = max - min + 1;
    let rand_val = (uuid::Uuid::new_v4().as_u128() >> 64) as u64;
    min + (rand_val % span)
}

/// Manages this node's `/tidb/server_id/<id>` etcd lease lifecycle.
pub struct ServerIdLease {
    etcd: EtcdClient,
    server_id: AtomicU64,
    lease_id: Mutex<Option<i64>>,
    lost_connection_to_pd: AtomicBool,
    enable_32bits: bool,
    timing: ServerIdTiming,
    on_server_id_change: Option<ServerIdChangeFn>,
    kill_all_connections: Option<KillAllConnectionsFn>,
}

impl fmt::Debug for ServerIdLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerIdLease")
            .field("server_id", &self.server_id.load(Ordering::SeqCst))
            .field(
                "lost_connection_to_pd",
                &self.lost_connection_to_pd.load(Ordering::SeqCst),
            )
            .field("enable_32bits", &self.enable_32bits)
            .finish()
    }
}

impl ServerIdLease {
    pub fn new(
        etcd: EtcdClient,
        enable_32bits: bool,
        timing: ServerIdTiming,
        on_server_id_change: Option<ServerIdChangeFn>,
        kill_all_connections: Option<KillAllConnectionsFn>,
    ) -> Self {
        Self {
            etcd,
            server_id: AtomicU64::new(0),
            lease_id: Mutex::new(None),
            lost_connection_to_pd: AtomicBool::new(false),
            enable_32bits,
            timing,
            on_server_id_change,
            kill_all_connections,
        }
    }

    pub fn server_id(&self) -> u64 {
        self.server_id.load(Ordering::SeqCst)
    }

    pub fn is_lost_connection_to_pd(&self) -> bool {
        self.lost_connection_to_pd.load(Ordering::SeqCst)
    }

    fn notify_change(&self, id: u64, valid: bool) {
        if let Some(cb) = &self.on_server_id_change {
            cb(id, valid);
        }
    }

    // go: Domain.retrieveServerIDSession L2 (uses raw etcd LeaseGrant without concurrency.Session wrapper)
    async fn retrieve_lease_id(&self) -> Result<i64> {
        // log-skip: retrieveServerIDSession.NewSession fail (raw etcd lease grant used instead)
        if let Some(id) = *self.lease_id.lock().await {
            return Ok(id);
        }
        let ttl_secs = i64::try_from(self.timing.ttl.as_secs().max(1)).unwrap_or(12 * 3600);
        let resp = match etcd_timeout(
            RETRIEVE_SERVER_ID_SESSION_TIMEOUT,
            self.etcd.clone().lease_grant(ttl_secs, None),
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                let err_str = e.to_string();
                tidb_base::log_error!("retrieveServerIDSession.Grant fail", "error" => err_str.as_str());
                return Err(e);
            }
        };
        let id = resp.id();
        *self.lease_id.lock().await = Some(id);
        Ok(id)
    }

    async fn collect_used_32bit_server_ids(&self) -> Result<HashSet<u64>> {
        let mut used = HashSet::new();
        let info_resp = etcd_timeout(
            ACQUIRE_SERVER_ID_TIMEOUT,
            self.etcd.clone().get(
                keys::SERVER_INFO_PREFIX,
                Some(GetOptions::new().with_prefix()),
            ),
        )
        .await?;
        for kv in info_resp.kvs() {
            if let Ok(info) = serde_json::from_slice::<ServerInfo>(kv.value())
                && info.server_id > 0
                && info.server_id <= MAX_SERVER_ID_32
            {
                used.insert(info.server_id);
            }
        }
        let id_resp = etcd_timeout(
            ACQUIRE_SERVER_ID_TIMEOUT,
            self.etcd.clone().get(
                keys::SERVER_ID_PREFIX,
                Some(GetOptions::new().with_prefix().with_keys_only()),
            ),
        )
        .await?;
        for kv in id_resp.kvs() {
            if let Ok(key) = kv.key_str()
                && let Some(suffix) = key.strip_prefix(keys::SERVER_ID_PREFIX)
                && let Ok(id) = suffix.parse::<u64>()
                && id > 0
                && id <= MAX_SERVER_ID_32
            {
                used.insert(id);
            }
        }
        Ok(used)
    }

    /// Acquires a cluster-unique `server_id` in `/tidb/server_id/<id>` bound to the lease.
    // go: Domain.acquireServerID L1
    pub async fn acquire_server_id(&self) -> Result<u64> {
        self.server_id.store(0, Ordering::SeqCst);
        let lease_id = self.retrieve_lease_id().await?;
        let mut conflict_cnt = 0usize;
        loop {
            let propose_id = if self.enable_32bits {
                let used = self.collect_used_32bit_server_ids().await?;
                propose_server_id_from_used(&used, conflict_cnt, true)
            } else {
                random_u64_in(1, MAX_SERVER_ID_64)
            };
            let key = keys::server_id_key(propose_id);
            let txn = Txn::new()
                .when([Compare::create_revision(key.clone(), CompareOp::Equal, 0)])
                .and_then([TxnOp::put(
                    key,
                    "0",
                    Some(PutOptions::new().with_lease(lease_id)),
                )]);
            let resp = etcd_timeout(ACQUIRE_SERVER_ID_TIMEOUT, self.etcd.clone().txn(txn)).await?;
            if !resp.succeeded() {
                tidb_base::log_info!(
                    "propose serverID exists, try again",
                    "proposeServerID" => propose_id,
                );
                tokio::time::sleep(ACQUIRE_SERVER_ID_RETRY_INTERVAL).await;
                conflict_cnt += 1;
                continue;
            }
            self.server_id.store(propose_id, Ordering::SeqCst);
            self.notify_change(propose_id, true);
            let lease_hex = format!("{lease_id:x}");
            tidb_base::log_info!(
                "acquireServerID",
                "serverID" => propose_id,
                "lease id" => lease_hex.as_str(),
            );
            return Ok(propose_id);
        }
    }

    /// Refreshes `/tidb/server_id/<id>` TTL and sends a keepalive ping on the etcd lease.
    // go: Domain.refreshServerIDTTL L2 (also pings etcd lease keepalive)
    pub async fn refresh_server_id_ttl(&self) -> Result<()> {
        let server_id = self.server_id();
        if server_id == 0 {
            return Err(Error::Other("server_id is 0".to_owned()));
        }
        let lease_id = self.retrieve_lease_id().await?;
        let res = async {
            let (mut keeper, mut stream) = etcd_timeout(
                LEASE_KEEPALIVE_STREAM_TIMEOUT,
                self.etcd.clone().lease_keep_alive(lease_id),
            )
            .await?;
            keeper.keep_alive().await?;
            match tokio::time::timeout(LEASE_KEEPALIVE_STREAM_TIMEOUT, stream.message()).await {
                Ok(Ok(Some(resp))) if resp.ttl() > 0 => {}
                Ok(Ok(Some(_))) => {
                    return Err(Error::Other("server_id etcd lease expired".to_owned()));
                }
                Ok(Ok(None)) => {
                    return Err(Error::Other("server_id keepalive stream ended".to_owned()));
                }
                Ok(Err(e)) => return Err(e.into()),
                Err(_) => return Err(Error::Timeout),
            }
            let key = keys::server_id_key(server_id);
            put_with_retry(&self.etcd, &key, "0", lease_id).await
        }
        .await;
        match res {
            Ok(()) => {
                let lease_hex = format!("{lease_id:x}");
                tidb_base::log_info!(
                    "refreshServerIDTTL succeed",
                    "serverID" => server_id,
                    "lease id" => lease_hex.as_str(),
                );
                Ok(())
            }
            Err(e) => {
                let err_str = e.to_string();
                tidb_base::log_error!(
                    "refreshServerIDTTL fail",
                    "serverID" => server_id,
                    "error" => err_str.as_str(),
                );
                Err(e)
            }
        }
    }

    /// Releases `/tidb/server_id/<id>` and revokes its lease on clean shutdown.
    // go: Domain.releaseServerID L1
    pub async fn release_server_id(&self) {
        let server_id = self.server_id.swap(0, Ordering::SeqCst);
        if server_id == 0 {
            return;
        }
        self.notify_change(server_id, false);
        let key = keys::server_id_key(server_id);
        let mut last_err = None;
        for _ in 0..REFRESH_SERVER_ID_RETRY_CNT {
            match etcd_timeout(
                ACQUIRE_SERVER_ID_TIMEOUT,
                self.etcd.clone().delete(key.as_str(), None),
            )
            .await
            {
                Ok(_) => {
                    last_err = None;
                    break;
                }
                Err(e) => last_err = Some(e),
            }
            tokio::time::sleep(ACQUIRE_SERVER_ID_RETRY_INTERVAL).await;
        }
        if let Some(lease) = self.lease_id.lock().await.take() {
            let _ = etcd_timeout(
                ACQUIRE_SERVER_ID_TIMEOUT,
                self.etcd.clone().lease_revoke(lease),
            )
            .await;
        }
        if let Some(e) = last_err {
            let err_str = e.to_string();
            tidb_base::log_error!(
                "releaseServerID fail",
                "serverID" => server_id,
                "error" => err_str.as_str(),
            );
        } else {
            tidb_base::log_info!("releaseServerID succeed", "serverID" => server_id);
        }
    }

    /// Runs Go's `serverIDKeeper` loop until `stop` transitions to `true`.
    // go: Domain.serverIDKeeper L2 (no panic recover loop; republishes ServerInfo via callback)
    pub async fn run_keeper(
        self: Arc<Self>,
        mut stop: tokio::sync::watch::Receiver<bool>,
        on_restored: Arc<dyn Fn() + Send + Sync>,
    ) {
        // log-skip: Domain.serverIDKeeper recover (tokio task panics propagate to shutdown)
        let mut watchdog =
            ServerIdWatchdog::new(Instant::now(), self.timing.lost_connection_to_pd_timeout);
        let mut keepalive_tick = tokio::time::interval(self.timing.keepalive_interval);
        keepalive_tick.tick().await;
        let mut restored_tick = tokio::time::interval(self.timing.check_restored_interval);
        restored_tick.tick().await;

        loop {
            tokio::select! {
                _ = stop.changed() => {
                    if *stop.borrow() {
                        tidb_base::log_info!("serverIDKeeper exited.");
                        return;
                    }
                }
                _ = keepalive_tick.tick() => {
                    if !self.is_lost_connection_to_pd() {
                        let ok = self.refresh_server_id_ttl().await.is_ok();
                        if !ok {
                            *self.lease_id.lock().await = None;
                        }
                        if watchdog.on_keepalive_result(ok, Instant::now()) == KeeperAction::LostConnectionToPd {
                            tidb_base::log_warn!("lost connection to PD");
                            self.lost_connection_to_pd.store(true, Ordering::SeqCst);
                            let id = self.server_id();
                            self.notify_change(id, false);
                            if let Some(kill_all) = &self.kill_all_connections {
                                kill_all();
                            }
                        }
                    }
                }
                _ = restored_tick.tick() => {
                    if self.is_lost_connection_to_pd()
                        && let Ok(_new_id) = self.acquire_server_id().await
                    {
                        tidb_base::log_info!("restored connection to PD");
                        self.lost_connection_to_pd.store(false, Ordering::SeqCst);
                        let _ = watchdog.on_keepalive_result(true, Instant::now());
                        on_restored();
                    }
                }
            }
        }
    }
}

async fn put_with_retry(etcd: &EtcdClient, key: &str, value: &str, lease_id: i64) -> Result<()> {
    let mut last_err = Error::Other("put_with_retry: no attempts".to_owned());
    for _ in 0..REFRESH_SERVER_ID_RETRY_CNT {
        match etcd_timeout(
            ACQUIRE_SERVER_ID_TIMEOUT,
            etcd.clone()
                .put(key, value, Some(PutOptions::new().with_lease(lease_id))),
        )
        .await
        {
            Ok(_) => return Ok(()),
            Err(e) => last_err = e,
        }
        tokio::time::sleep(ACQUIRE_SERVER_ID_RETRY_INTERVAL).await;
    }
    Err(last_err)
}

async fn etcd_timeout<T>(
    dur: Duration,
    f: impl std::future::Future<Output = std::result::Result<T, etcd_client::Error>>,
) -> Result<T> {
    match tokio::time::timeout(dur, f).await {
        Ok(r) => Ok(r?),
        Err(_) => Err(Error::Timeout),
    }
}

/// Resolves `target_server_id`'s RPC endpoint (`http://ip:status_port`) from `/tidb/server/info/`.
pub async fn find_server_rpc_url(
    etcd: &EtcdClient,
    target_server_id: u64,
) -> Result<Option<String>> {
    let resp = etcd_timeout(
        ACQUIRE_SERVER_ID_TIMEOUT,
        etcd.clone().get(
            keys::SERVER_INFO_PREFIX,
            Some(GetOptions::new().with_prefix()),
        ),
    )
    .await?;
    let mut best: Option<(i64, String)> = None;
    for kv in resp.kvs() {
        let Ok(info) = serde_json::from_slice::<ServerInfo>(kv.value()) else {
            continue;
        };
        if info.server_id != target_server_id {
            continue;
        }
        let Some(url) = info.rpc_base_url() else {
            continue;
        };
        match &best {
            Some((ts, _)) if info.start_timestamp <= *ts => {}
            _ => best = Some((info.start_timestamp, url)),
        }
    }
    Ok(best.map(|(_, url)| url))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn propose_server_id_uses_32bit_until_saturated_or_conflicted() {
        let mut used = HashSet::new();
        used.insert(1);
        used.insert(2);
        let id = propose_server_id_from_used(&used, 0, true);
        assert!((1..=MAX_SERVER_ID_32).contains(&id), "id={id}");
        assert!(!used.contains(&id));

        // Saturation (> 90% of 2047) upgrades to 64-bit range [2048, MAX_SERVER_ID_64].
        let saturated: HashSet<u64> = (1..=1900).collect();
        let upgraded = propose_server_id_from_used(&saturated, 0, true);
        assert!(
            (MAX_SERVER_ID_32 + 1..=MAX_SERVER_ID_64).contains(&upgraded),
            "upgraded={upgraded}"
        );

        // 3 conflicts upgrade to 64-bit range.
        let upgraded_conflict =
            propose_server_id_from_used(&used, ACQUIRE_32BITS_SERVER_ID_RETRY_CNT, true);
        assert!(
            (MAX_SERVER_ID_32 + 1..=MAX_SERVER_ID_64).contains(&upgraded_conflict),
            "upgraded_conflict={upgraded_conflict}"
        );
    }

    #[test]
    fn watchdog_triggers_kill_all_after_6h_disconnect_and_recovers() {
        let t0 = Instant::now();
        let mut wd = ServerIdWatchdog::new(t0, LOST_CONNECTION_TO_PD_TIMEOUT);
        assert_eq!(
            wd.on_keepalive_result(false, t0 + Duration::from_secs(3600)),
            KeeperAction::TransientFailure
        );
        assert!(!wd.is_lost());
        assert_eq!(
            wd.on_keepalive_result(false, t0 + Duration::from_secs(6 * 3600 + 1)),
            KeeperAction::LostConnectionToPd
        );
        assert!(wd.is_lost());
        assert_eq!(
            wd.on_keepalive_result(true, t0 + Duration::from_secs(6 * 3600 + 10)),
            KeeperAction::RestoredConnectionToPd
        );
        assert!(!wd.is_lost());
    }
}
