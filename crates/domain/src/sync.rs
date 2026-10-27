//! The background schema sync: etcd registration and keepalive, the reload loop driven by
//! `global_schema_version`, the MDL check and reports, and the watchdog that deregisters the
//! node when it is not keeping up. See `docs/adr/0005-schema-sync.md`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use etcd_client::Client as EtcdClient;
use etcd_client::Compare;
use etcd_client::CompareOp;
use etcd_client::ConnectOptions;
use etcd_client::PutOptions;
use etcd_client::Txn;
use etcd_client::TxnOp;
use etcd_client::WatchOptions;
use tidb_distsql::DagContext;
use tidb_executor::ExecContext;
use tidb_kv::KvClient;
use tidb_meta::InfoSchema;
use tidb_meta::MetaReader;
use tidb_mysql_protocol::Value;
use tidb_sql::PlanContext;
use tidb_sql::Statement;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tracing::debug;
use tracing::info;
use tracing::warn;

use crate::Error;
use crate::Result;
use crate::keys;
use crate::registry::Limits;
use crate::registry::MdlJob;
use crate::registry::Registry;
use crate::serverinfo::ServerInfo;

/// Timeout of one etcd request (Go: `KeyOpDefaultTimeout`).
const ETCD_OP_TIMEOUT: Duration = Duration::from_secs(2);
/// Retries of the compare-and-swap job report (Go: `keyOpDefaultRetryCnt`, `KeyOpRetryInterval`).
const PUT_MONO_RETRIES: usize = 3;
const PUT_MONO_RETRY_INTERVAL: Duration = Duration::from_millis(30);
/// Go keeps at most this many reported jobs before clearing its cache.
const JOB_CACHE_LIMIT: usize = 1000;

/// Settings of [`SchemaSync`].
#[derive(Clone, Debug)]
pub struct SyncConfig {
    /// PD client URLs (PD embeds the cluster's etcd).
    pub pd_endpoints: Vec<String>,
    /// This node's server-info record (its `ddl_id` names every key it writes).
    pub info: ServerInfo,
    /// Lease TTL of the registration (Go: `SessionTTL`, 90 s).
    pub lease_ttl: Duration,
    /// Period of full cycles (reload check + MDL refresh) besides the watch.
    pub tick: Duration,
    /// Period of MDL re-checks while a job is held back (Go: `mdlCheckLookDuration`, 50 ms).
    pub mdl_recheck: Duration,
    pub limits: Limits,
    /// Coprocessor settings for the node's own `mysql.tidb_mdl_info` reads.
    pub dag: DagContext,
    /// Test hook: while this file exists the sync loop does nothing (as if stuck).
    pub stall_file: Option<PathBuf>,
}

impl SyncConfig {
    pub fn new(pd_endpoints: Vec<String>, info: ServerInfo, dag: DagContext) -> SyncConfig {
        SyncConfig {
            pd_endpoints,
            info,
            lease_ttl: Duration::from_secs(90),
            tick: Duration::from_secs(1),
            mdl_recheck: Duration::from_millis(50),
            limits: Limits::default(),
            dag,
            stall_file: None,
        }
    }
}

/// The node's live registration.
struct Registration {
    lease: i64,
    keepalive: JoinHandle<()>,
}

struct Inner {
    cfg: SyncConfig,
    kv: KvClient,
    etcd: EtcdClient,
    registry: Arc<Registry>,
    /// Held while registering or deregistering.
    registration: tokio::sync::Mutex<Option<Registration>>,
    wake: Notify,
    stalled: AtomicBool,
    stop: tokio::sync::watch::Sender<bool>,
}

/// Handle of the running schema sync.
#[derive(Clone)]
pub struct SchemaSync {
    inner: Arc<Inner>,
}

impl SchemaSync {
    /// Connect to PD's etcd and start the sync tasks. The node registers itself once its
    /// first reload succeeds; statements use synced mode only after that (see
    /// [`Registry::admit`]).
    pub async fn start(kv: KvClient, cfg: SyncConfig) -> Result<SchemaSync> {
        let etcd = EtcdClient::connect(
            &cfg.pd_endpoints,
            Some(
                ConnectOptions::new()
                    .with_connect_timeout(Duration::from_secs(5))
                    .with_keep_alive(Duration::from_secs(10), Duration::from_secs(5)),
            ),
        )
        .await?;
        let (stop, _) = tokio::sync::watch::channel(false);
        let inner = Arc::new(Inner {
            registry: Registry::new(cfg.limits),
            cfg,
            kv,
            etcd,
            registration: tokio::sync::Mutex::new(None),
            wake: Notify::new(),
            stalled: AtomicBool::new(false),
            stop,
        });
        tokio::spawn(inner.clone().watch_global_version());
        tokio::spawn(inner.clone().sync_loop());
        tokio::spawn(inner.clone().watchdog());
        info!(
            ddl_id = %inner.cfg.info.ddl_id,
            ip = %inner.cfg.info.ip,
            port = inner.cfg.info.listening_port,
            "schema sync started"
        );
        Ok(SchemaSync { inner })
    }

    pub fn registry(&self) -> &Arc<Registry> {
        &self.inner.registry
    }

    pub fn ddl_id(&self) -> &str {
        &self.inner.cfg.info.ddl_id
    }

    /// Test hook: stop (or resume) the sync loop as if it were stuck.
    pub fn set_stalled(&self, stalled: bool) {
        self.inner.stalled.store(stalled, Ordering::SeqCst);
        self.inner.wake.notify_one();
    }

    /// Wait until statements can use synced mode (or `timeout` passes).
    pub async fn wait_admissible(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.inner.registry.admissible(Instant::now()) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    /// Graceful shutdown: stop the tasks, leave synced mode and delete this node's keys (by
    /// revoking the lease), so DDL stops waiting for it at once.
    pub async fn shutdown(&self) {
        self.inner.stop.send_replace(true);
        self.inner.wake.notify_one();
        self.inner.deregister("shutdown").await;
    }
}

impl Inner {
    fn etcd(&self) -> EtcdClient {
        self.etcd.clone()
    }

    fn stopped(&self) -> bool {
        *self.stop.borrow()
    }

    fn is_stalled(&self) -> bool {
        self.stalled.load(Ordering::SeqCst)
            || self.cfg.stall_file.as_ref().is_some_and(|p| p.exists())
    }

    /// Keep [`Registry`] informed of `global_schema_version` and wake the sync loop on every
    /// change (Go: `GlobalVersionCh`).
    async fn watch_global_version(self: Arc<Self>) {
        let mut stop = self.stop.subscribe();
        while !self.stopped() {
            if let Err(e) = self.watch_once(&mut stop).await {
                warn!(error = %e, "watch of global_schema_version failed; retrying");
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                _ = stop.changed() => {}
            }
        }
    }

    async fn watch_once(&self, stop: &mut tokio::sync::watch::Receiver<bool>) -> Result<()> {
        let resp = timeout(self.etcd().get(keys::GLOBAL_SCHEMA_VERSION, None)).await?;
        let revision = resp.header().map_or(0, |h| h.revision());
        if let Some(kv) = resp.kvs().first() {
            self.note_global(kv.value());
        }
        let (_watcher, mut stream) = timeout(self.etcd().watch(
            keys::GLOBAL_SCHEMA_VERSION,
            Some(WatchOptions::new().with_start_revision(revision + 1)),
        ))
        .await?;
        loop {
            let msg = tokio::select! {
                m = stream.message() => m?,
                _ = stop.changed() => return Ok(()),
            };
            let Some(msg) = msg else {
                return Err(Error::Other("watch stream ended".to_owned()));
            };
            for event in msg.events() {
                if let Some(kv) = event.kv() {
                    self.note_global(kv.value());
                }
            }
            if msg.canceled() {
                return Err(Error::Other("watch canceled".to_owned()));
            }
        }
    }

    fn note_global(&self, value: &[u8]) {
        match std::str::from_utf8(value)
            .ok()
            .and_then(|s| s.parse::<i64>().ok())
        {
            Some(v) => {
                debug!(version = v, "global schema version");
                self.registry.note_global(v, Instant::now());
                self.wake.notify_one();
            }
            None => warn!(value = ?String::from_utf8_lossy(value), "bad global_schema_version"),
        }
    }

    /// The sync loop (Go: `loadSchemaInLoop` + `mdlCheckLoop`). A *full cycle* reloads the
    /// schema at a fresh TSO when the version changed, registers the node if needed, and
    /// re-reads `mysql.tidb_mdl_info`; it runs on every global-version change and every
    /// `tick`. Between full cycles, while some job is held back by an in-flight statement,
    /// the registry is re-checked every `mdl_recheck`.
    async fn sync_loop(self: Arc<Self>) {
        let mut stop = self.stop.subscribe();
        let mut jobs: Vec<MdlJob> = Vec::new();
        let mut reported: HashMap<i64, i64> = HashMap::new();
        let mut self_version_put: Option<i64> = None;
        let mut pending = false;
        let mut force_full = true;
        let mut last_full: Option<Instant> = None;
        loop {
            if !force_full {
                let delay = if pending {
                    self.cfg.mdl_recheck
                } else {
                    self.cfg.tick
                };
                tokio::select! {
                    _ = self.wake.notified() => force_full = true,
                    _ = tokio::time::sleep(delay) => {}
                    _ = stop.changed() => {}
                }
            }
            if self.stopped() {
                return;
            }
            if self.is_stalled() {
                force_full = false;
                pending = false;
                continue;
            }
            let full = force_full || last_full.is_none_or(|t| t.elapsed() >= self.cfg.tick);
            force_full = false;
            let started = Instant::now();
            if full {
                last_full = Some(started);
                if let Err(e) = self.reload().await {
                    warn!(error = %e, "schema reload failed");
                    continue;
                }
                if !self.registry.is_registered() {
                    match self.register().await {
                        // Reload again at a TSO taken after the keys are visible.
                        Ok(()) => force_full = true,
                        Err(e) => warn!(error = %e, "registering in etcd failed"),
                    }
                    self_version_put = self.registry.loaded_version();
                    continue;
                }
                match self.read_mdl_jobs().await {
                    Ok(j) => jobs = j,
                    Err(e) => {
                        warn!(error = %e, "reading mysql.tidb_mdl_info failed");
                        continue;
                    }
                }
            }
            if !self.registry.is_registered() {
                continue;
            }
            let mut ok = true;
            let loaded = self.registry.loaded_version();
            if loaded > self_version_put
                && let Some(v) = loaded
            {
                match self.put_self_version(v).await {
                    Ok(()) => self_version_put = loaded,
                    Err(e) => {
                        ok = false;
                        warn!(error = %e, "updating all_schema_versions failed");
                    }
                }
            }
            let (still_pending, report_ok) = self.report(&jobs, &mut reported).await;
            pending = still_pending;
            if full && ok && report_ok {
                self.registry.cycle_completed(started);
            }
        }
    }

    /// Reload the schema at a fresh TSO if its version changed (full reload; Go applies schema
    /// diffs incrementally). The version is the one whose diff exists, as in ADR 0003.
    async fn reload(&self) -> Result<()> {
        let ts = self.kv.current_ts().await?;
        let mut snapshot = self.kv.snapshot(ts);
        let version = MetaReader::new(&mut snapshot)
            .schema_version_with_non_empty_diff()
            .await?;
        if self
            .registry
            .loaded_version()
            .is_some_and(|loaded| loaded >= version)
        {
            return Ok(());
        }
        let started = Instant::now();
        let schema = InfoSchema::load(&mut snapshot).await?;
        let version = schema.version();
        self.registry.install(Arc::new(schema), Instant::now());
        self.registry.count(|s| s.reloads += 1);
        info!(version, elapsed = ?started.elapsed(), "schema loaded");
        Ok(())
    }

    /// Pending DDL job steps this node must acknowledge (Go: `refreshMDLCheckTableInfo`): rows
    /// of `mysql.tidb_mdl_info` at a fresh TSO whose version is not newer than the loaded
    /// schema. Read through the node's own planner and executor, with the loaded schema.
    async fn read_mdl_jobs(&self) -> Result<Vec<MdlJob>> {
        let Some(schema) = self.registry.schema() else {
            return Ok(Vec::new());
        };
        let lowered = tidb_sql::parse_and_lower(
            "SELECT job_id, version, table_ids FROM mysql.tidb_mdl_info",
        )?;
        let Statement::Select(sel) = &lowered.stmt else {
            unreachable!("a SELECT")
        };
        let plan = tidb_sql::plan_select(
            sel,
            &schema,
            PlanContext {
                current_db: None,
                params: &[],
                describe_only: false,
            },
        )?;
        let ts = self.kv.current_ts().await?;
        let mut snapshot = self.kv.snapshot(ts);
        let cx = ExecContext {
            dag: self.cfg.dag.clone(),
            schema_version: schema.version(),
        };
        let rows = tidb_executor::execute(&plan, &mut snapshot, &cx).await?;
        let loaded = schema.version();
        let mut jobs = Vec::new();
        for row in rows {
            let (Some(job_id), Some(version)) = (int(row.first()), int(row.get(1))) else {
                warn!(?row, "unexpected mysql.tidb_mdl_info row");
                continue;
            };
            if version > loaded {
                continue;
            }
            let table_ids = match row.get(2) {
                Some(Value::Bytes(b)) => MdlJob::parse_table_ids(&String::from_utf8_lossy(b)),
                _ => Vec::new(),
            };
            jobs.push(MdlJob {
                job_id,
                version,
                table_ids,
            });
        }
        Ok(jobs)
    }

    /// Report every job no in-flight statement holds back (Go: `mdlCheckLoop` after
    /// `CheckOldRunningTxn`). Returns (some job still pending, no etcd error).
    async fn report(&self, jobs: &[MdlJob], reported: &mut HashMap<i64, i64>) -> (bool, bool) {
        if reported.len() > JOB_CACHE_LIMIT {
            reported.clear();
        }
        let ready = self.registry.jobs_ready(jobs);
        let mut pending = false;
        let mut ok = true;
        for (job, ready) in jobs.iter().zip(ready) {
            if reported.get(&job.job_id).is_some_and(|&v| v >= job.version) {
                continue;
            }
            if !ready {
                pending = true;
                continue;
            }
            let key = keys::job_schema_version(job.job_id, &self.cfg.info.ddl_id);
            match self.put_mono(&key, &job.version.to_string()).await {
                Ok(()) => {
                    info!(
                        job_id = job.job_id,
                        version = job.version,
                        "mdl: reported schema version to the DDL owner"
                    );
                    reported.insert(job.job_id, job.version);
                    self.registry.count(|s| s.mdl_reports += 1);
                }
                Err(e) => {
                    warn!(job_id = job.job_id, error = %e, "mdl: reporting to the DDL owner failed");
                    pending = true;
                    ok = false;
                }
            }
        }
        (pending, ok)
    }

    /// Go's `PutKVToEtcdMono`: read the key's mod revision, then put without a lease only if it
    /// is unchanged; retried a few times.
    async fn put_mono(&self, key: &str, value: &str) -> Result<()> {
        let mut last = Error::Other("put_mono: no attempt".to_owned());
        for _ in 0..PUT_MONO_RETRIES {
            match self.put_mono_once(key, value).await {
                Ok(()) => return Ok(()),
                Err(e) => last = e,
            }
            tokio::time::sleep(PUT_MONO_RETRY_INTERVAL).await;
        }
        Err(last)
    }

    async fn put_mono_once(&self, key: &str, value: &str) -> Result<()> {
        let resp = timeout(self.etcd().get(key, None)).await?;
        let rev = resp.kvs().first().map_or(0, |kv| kv.mod_revision());
        let txn = Txn::new()
            .when([Compare::mod_revision(key, CompareOp::Equal, rev)])
            .and_then([TxnOp::put(key, value, None)]);
        let resp = timeout(self.etcd().txn(txn)).await?;
        if resp.succeeded() {
            Ok(())
        } else {
            Err(Error::Other(
                "compare-and-swap of the job schema version failed".to_owned(),
            ))
        }
    }

    /// `all_schema_versions/<ddl_id>` = loaded version, bound to the lease.
    async fn put_self_version(&self, version: i64) -> Result<()> {
        let lease = self.registration.lock().await.as_ref().map(|r| r.lease);
        let Some(lease) = lease else {
            return Ok(());
        };
        timeout(self.etcd().put(
            keys::self_schema_version(&self.cfg.info.ddl_id),
            version.to_string(),
            Some(PutOptions::new().with_lease(lease)),
        ))
        .await?;
        Ok(())
    }

    /// Register: a lease, the server-info record and `all_schema_versions/<ddl_id>` under it,
    /// and a keepalive task.
    async fn register(self: &Arc<Self>) -> Result<()> {
        let mut reg = self.registration.lock().await;
        if self.registry.is_registered() || self.stopped() {
            return Ok(());
        }
        if let Some(old) = reg.take() {
            // Leftover of a registration the registry already dropped.
            old.keepalive.abort();
            let _ = timeout(self.etcd().lease_revoke(old.lease)).await;
        }
        let sent = Instant::now();
        let grant = timeout(
            self.etcd()
                .lease_grant(self.cfg.lease_ttl.as_secs() as i64, None),
        )
        .await?;
        let lease = grant.id();
        let deadline = sent + Duration::from_secs(grant.ttl().max(0) as u64);
        let info = self.cfg.info.to_json();
        let version = self.registry.loaded_version().unwrap_or(0);
        let puts = async {
            timeout(self.etcd().put(
                self.cfg.info.key(),
                info,
                Some(PutOptions::new().with_lease(lease)),
            ))
            .await?;
            timeout(self.etcd().put(
                keys::self_schema_version(&self.cfg.info.ddl_id),
                version.to_string(),
                Some(PutOptions::new().with_lease(lease)),
            ))
            .await?;
            Ok::<(), Error>(())
        };
        if let Err(e) = puts.await {
            let _ = timeout(self.etcd().lease_revoke(lease)).await;
            return Err(e);
        }
        let epoch = self.registry.registered(Instant::now(), deadline);
        let keepalive = tokio::spawn(self.clone().keep_alive(lease, epoch));
        *reg = Some(Registration { lease, keepalive });
        info!(
            ddl_id = %self.cfg.info.ddl_id,
            lease = format!("{lease:x}"),
            version,
            "registered in etcd"
        );
        Ok(())
    }

    /// Keep the lease alive (Go's etcd session: every TTL/3) and tell the registry how long it
    /// is proven alive. A keepalive answer of TTL 0 means the lease is gone.
    async fn keep_alive(self: Arc<Self>, lease: i64, epoch: u64) {
        let interval = self.cfg.lease_ttl / 3;
        loop {
            let stream = timeout(self.etcd().lease_keep_alive(lease)).await;
            let (mut keeper, mut stream) = match stream {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, "lease keepalive: cannot open stream");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };
            loop {
                let sent = Instant::now();
                if let Err(e) = keeper.keep_alive().await {
                    warn!(error = %e, "lease keepalive failed");
                    break;
                }
                match tokio::time::timeout(ETCD_OP_TIMEOUT, stream.message()).await {
                    Ok(Ok(Some(resp))) if resp.ttl() > 0 => {
                        let deadline = sent + Duration::from_secs(resp.ttl() as u64);
                        self.registry.lease_renewed(epoch, deadline);
                    }
                    Ok(Ok(Some(_))) => {
                        warn!(lease = format!("{lease:x}"), "lease expired");
                        self.registry.lease_lost(epoch);
                        return;
                    }
                    Ok(Ok(None)) => {
                        warn!("lease keepalive stream ended");
                        break;
                    }
                    Ok(Err(e)) => {
                        warn!(error = %e, "lease keepalive failed");
                        break;
                    }
                    Err(_) => {
                        warn!("lease keepalive got no answer");
                        break;
                    }
                }
                tokio::time::sleep(interval).await;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    /// Deregister the node when it is not keeping up (Go has no such bound: a stuck Go TiDB
    /// blocks DDL until its lease expires).
    async fn watchdog(self: Arc<Self>) {
        let mut stop = self.stop.subscribe();
        loop {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                _ = stop.changed() => return,
            }
            if self.registry.should_deregister(Instant::now()) {
                self.deregister("not keeping up with the cluster").await;
            }
        }
    }

    /// Leave synced mode (new epoch: in-flight synced statements' results are refused) and
    /// revoke the lease, which deletes the server-info and `all_schema_versions` keys.
    async fn deregister(&self, why: &str) {
        let mut reg = self.registration.lock().await;
        let was = self.registry.deregister();
        let Some(r) = reg.take() else {
            if was {
                warn!(why, "deregistered (no lease held)");
            }
            return;
        };
        r.keepalive.abort();
        let started = Instant::now();
        match timeout(self.etcd().lease_revoke(r.lease)).await {
            Ok(_) => {}
            Err(e) => {
                warn!(error = %e, "lease revoke failed; deleting the keys");
                let _ = timeout(self.etcd().delete(self.cfg.info.key(), None)).await;
                let _ = timeout(
                    self.etcd()
                        .delete(keys::self_schema_version(&self.cfg.info.ddl_id), None),
                )
                .await;
            }
        }
        warn!(why, elapsed = ?started.elapsed(), "deregistered from etcd");
        self.wake.notify_one();
    }
}

fn int(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Int(i) => Some(*i),
        Value::UInt(u) => i64::try_from(*u).ok(),
        _ => None,
    }
}

async fn timeout<T>(
    f: impl std::future::Future<Output = std::result::Result<T, etcd_client::Error>>,
) -> Result<T> {
    match tokio::time::timeout(ETCD_OP_TIMEOUT, f).await {
        Ok(r) => Ok(r?),
        Err(_) => Err(Error::Timeout),
    }
}
