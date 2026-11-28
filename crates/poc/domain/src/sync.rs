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
use poc_executor::ExecContext;
use poc_kv::KvClient;
use tidb_distsql::DagContext;
use tidb_meta::InfoSchema;
use tidb_meta::MetaReader;
use tidb_mysql_protocol::Value;
use tidb_sql::PlanContext;
use tidb_sql::Statement;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::Error;
use crate::Result;
use crate::keys;
use crate::registry::Limits;
use crate::registry::MdlJob;
use crate::registry::Registry;
use crate::server_id::{
    KillAllConnectionsFn, ServerIdChangeFn, ServerIdLease, ServerIdTiming, find_server_rpc_url,
};
use crate::serverinfo::ServerInfo;

/// Timeout of one etcd request (Go: `KeyOpDefaultTimeout`).
const ETCD_OP_TIMEOUT: Duration = Duration::from_secs(2);
/// Retries of the compare-and-swap job report (Go: `keyOpDefaultRetryCnt`, `KeyOpRetryInterval`).
const PUT_MONO_RETRIES: usize = 3;
const PUT_MONO_RETRY_INTERVAL: Duration = Duration::from_millis(30);
/// Go keeps at most this many reported jobs before clearing its cache.
const JOB_CACHE_LIMIT: usize = 1000;

/// Settings of [`SchemaSync`].
#[derive(Clone)]
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
    /// Build and reload the ported domain (D06.5 part 1) for the new stack: only when the
    /// new stack is on (`--new-stack` not `off`). Off by default.
    pub ported_domain: bool,
    /// Acquire and maintain `/tidb/server_id/<id>` lease for cluster global KILL.
    pub acquire_server_id: bool,
    /// Prefer 32-bit global connection IDs (`[1, 2047]`) before upgrading to 64-bit.
    pub enable_32bits_conn_id: bool,
    /// Timing parameters for `/tidb/server_id/<id>` lease keepalive and 6h PD disconnect watchdog.
    pub server_id_timing: ServerIdTiming,
    /// Notified whenever the acquired `server_id` or its validity changes.
    pub on_server_id_change: Option<ServerIdChangeFn>,
    /// Invoked on > 6h PD disconnect to kill all active connections.
    pub kill_all_connections: Option<KillAllConnectionsFn>,
}

impl std::fmt::Debug for SyncConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncConfig")
            .field("pd_endpoints", &self.pd_endpoints)
            .field("info", &self.info)
            .field("lease_ttl", &self.lease_ttl)
            .field("tick", &self.tick)
            .field("mdl_recheck", &self.mdl_recheck)
            .field("limits", &self.limits)
            .field("stall_file", &self.stall_file)
            .field("ported_domain", &self.ported_domain)
            .field("acquire_server_id", &self.acquire_server_id)
            .field("enable_32bits_conn_id", &self.enable_32bits_conn_id)
            .finish()
    }
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
            ported_domain: false,
            acquire_server_id: false,
            enable_32bits_conn_id: true,
            server_id_timing: ServerIdTiming::default(),
            on_server_id_change: None,
            kill_all_connections: None,
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
    /// The ported domain (D06.5 part 1), reloaded at the timestamp of every reload here;
    /// `None` when the new stack is off.
    ported: Option<Arc<tidb_domain::Domain>>,
    /// Held while registering or deregistering.
    registration: tokio::sync::Mutex<Option<Registration>>,
    /// JoinHandle of `sync_loop` so `shutdown()` awaits `"loadSchemaInLoop exited."` before
    /// logging `"domain closed"`.
    sync_loop_handle: tokio::sync::Mutex<Option<JoinHandle<()>>>,
    /// Cluster-unique `/tidb/server_id/<id>` lease handle when global kill is enabled.
    server_id_lease: Option<Arc<ServerIdLease>>,
    server_id: std::sync::atomic::AtomicU64,
    server_id_keeper_handle: tokio::sync::Mutex<Option<JoinHandle<()>>>,
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
    pub async fn start(kv: KvClient, mut cfg: SyncConfig) -> Result<SchemaSync> {
        cfg.limits.check().map_err(Error::Other)?;
        let etcd = EtcdClient::connect(
            &cfg.pd_endpoints,
            Some(
                ConnectOptions::new()
                    .with_connect_timeout(Duration::from_secs(5))
                    .with_keep_alive(Duration::from_secs(10), Duration::from_secs(5)),
            ),
        )
        .await?;
        let server_id_lease = if cfg.acquire_server_id {
            let lease = Arc::new(ServerIdLease::new(
                etcd.clone(),
                cfg.enable_32bits_conn_id,
                cfg.server_id_timing,
                cfg.on_server_id_change.clone(),
                cfg.kill_all_connections.clone(),
            ));
            let id = lease.acquire_server_id().await?;
            cfg.info.server_id = id;
            Some(lease)
        } else {
            None
        };
        let initial_server_id = cfg.info.server_id;
        let (stop, _) = tokio::sync::watch::channel(false);
        let inner = Arc::new(Inner {
            registry: Registry::new(cfg.limits),
            ported: crate::ported_domain::ported_domain_if(cfg.ported_domain, || {
                crate::ported_domain::new_ported_domain(kv.clone())
            }),
            cfg,
            kv,
            etcd,
            registration: tokio::sync::Mutex::new(None),
            sync_loop_handle: tokio::sync::Mutex::new(None),
            server_id_lease: server_id_lease.clone(),
            server_id: std::sync::atomic::AtomicU64::new(initial_server_id),
            server_id_keeper_handle: tokio::sync::Mutex::new(None),
            wake: Notify::new(),
            stalled: AtomicBool::new(false),
            stop: stop.clone(),
        });
        if let Some(lease) = server_id_lease {
            let inner_for_restore = Arc::clone(&inner);
            let lease_for_restore = Arc::clone(&lease);
            let on_restored: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
                let inner = Arc::clone(&inner_for_restore);
                let new_id = lease_for_restore.server_id();
                inner.server_id.store(new_id, Ordering::SeqCst);
                tokio::spawn(async move {
                    inner.store_server_info().await;
                });
            });
            let keeper_handle = tokio::spawn(lease.run_keeper(stop.subscribe(), on_restored));
            *inner.server_id_keeper_handle.lock().await = Some(keeper_handle);
        }
        tokio::spawn(inner.clone().watch_global_version());
        let loop_handle = tokio::spawn(inner.clone().sync_loop());
        *inner.sync_loop_handle.lock().await = Some(loop_handle);
        tokio::spawn(inner.clone().watchdog());
        tidb_base::log_info!(
            "schema sync started",
            "ddl_id" => inner.cfg.info.ddl_id.as_str(),
            "ip" => inner.cfg.info.ip.as_str(),
            "port" => inner.cfg.info.listening_port,
        );
        Ok(SchemaSync { inner })
    }

    /// The ported domain the new stack's sessions take their InfoSchema from.
    /// `None` when the new stack is off.
    pub fn ported_domain(&self) -> Option<&Arc<tidb_domain::Domain>> {
        self.inner.ported.as_ref()
    }

    pub fn registry(&self) -> &Arc<Registry> {
        &self.inner.registry
    }

    pub fn ddl_id(&self) -> &str {
        &self.inner.cfg.info.ddl_id
    }

    /// Returns this node's currently active cluster `server_id` (`0` if unleased).
    pub fn server_id(&self) -> u64 {
        self.inner.server_id.load(Ordering::SeqCst)
    }

    /// Resolves `target_server_id`'s status/RPC URL (`http://ip:status_port`) from etcd.
    pub async fn lookup_server_rpc_url(&self, target_server_id: u64) -> Result<Option<String>> {
        find_server_rpc_url(&self.inner.etcd(), target_server_id).await
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
        let started = Instant::now();
        self.inner.stop.send_replace(true);
        self.inner.wake.notify_one();
        if let Some(handle) = self.inner.server_id_keeper_handle.lock().await.take() {
            let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
        }
        if let Some(lease) = &self.inner.server_id_lease {
            lease.release_server_id().await;
        }
        if let Some(handle) = self.inner.sync_loop_handle.lock().await.take() {
            let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
        }
        self.inner.deregister("shutdown").await;
        tidb_base::log_info!("domain closed", "take time" => started.elapsed());
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
                let err_str = e.to_string();
                tidb_base::log_warn!(
                    "watch of global_schema_version failed; retrying",
                    "error" => err_str.as_str(),
                );
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
                tidb_base::log_debug!("global schema version", "version" => v);
                self.registry.note_global(v, Instant::now());
                self.wake.notify_one();
            }
            None => tidb_base::log_warn!("bad global_schema_version", "value" => value),
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
                tidb_base::log_info!("loadSchemaInLoop exited.");
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
                    let err_str = e.to_string();
                    tidb_base::log_warn!("schema reload failed", "error" => err_str.as_str());
                    continue;
                }
                if !self.registry.is_registered() {
                    match self.register().await {
                        // Reload again at a TSO taken after the keys are visible.
                        Ok(()) => force_full = true,
                        Err(e) => {
                            let err_str = e.to_string();
                            tidb_base::log_warn!(
                                "registering in etcd failed",
                                "error" => err_str.as_str(),
                            );
                        }
                    }
                    self_version_put = self.registry.loaded_version();
                    continue;
                }
                match self.read_mdl_jobs().await {
                    Ok(j) => jobs = j,
                    Err(e) => {
                        let err_str = e.to_string();
                        tidb_base::log_warn!(
                            "reading mysql.tidb_mdl_info failed",
                            "error" => err_str.as_str(),
                        );
                        continue;
                    }
                }
            }
            if !self.registry.is_registered() {
                continue;
            }
            let mut ok = true;
            let mut self_pending = false;
            let loaded = self.registry.loaded_version();
            if loaded > self_version_put
                && let Some(v) = loaded
            {
                if !self.registry.drained_below(v) {
                    self_pending = true;
                } else {
                    match self.put_self_version(v).await {
                        Ok(()) => self_version_put = loaded,
                        Err(e) => {
                            ok = false;
                            let err_str = e.to_string();
                            tidb_base::log_warn!(
                                "updating all_schema_versions failed",
                                "error" => err_str.as_str(),
                            );
                        }
                    }
                }
            }
            let (still_pending, report_ok) = self.report(&jobs, &mut reported).await;
            pending = still_pending || self_pending;
            if full && ok && report_ok {
                self.registry.cycle_completed(started);
            }
        }
    }

    /// Reload the schema at a fresh TSO if its version changed (full reload; Go applies schema
    /// diffs incrementally). The version is the one whose diff exists, as in ADR 0003.
    ///
    /// Every successful reload renews the schema lease at its TSO, whether or not the version
    /// changed (Go: `Domain.Reload` calls `SchemaValidator.Update` "despite it is cache or
    /// not").
    async fn reload(&self) -> Result<()> {
        let requested_at = Instant::now();
        let ts = self.kv.current_ts().await?;
        let mut snapshot = self.kv.snapshot(ts);
        let version = MetaReader::new(&mut snapshot)
            .schema_version_with_non_empty_diff()
            .await?;
        let loaded = self.registry.loaded_version();
        let old_version = loaded.unwrap_or(0);
        let full_load = loaded.is_none_or(|loaded| loaded < version);
        if full_load {
            let started = Instant::now();
            let schema = InfoSchema::load(&mut snapshot).await?;
            let version = schema.version();
            self.registry.install(Arc::new(schema), Instant::now());
            self.registry.count(|s| s.reloads += 1);
            tidb_base::log_info!(
                "schema loaded",
                "version" => version,
                "elapsed" => started.elapsed(),
            );
        }
        self.registry
            .reloaded(ts, requested_at, old_version, full_load);
        // The ported domain loads the same version at the same timestamp (VS1 plan §4.1): a
        // diff load from its cached version, or a full load. A failure leaves the new stack
        // on its older version; the router sends statements to the PoC until a later reload
        // of the ported domain succeeds (`Registry::new_stack_schema_version`), even when the
        // versions compare equal.
        if let Some(ported) = &self.ported {
            let r = ported.reload_at(ts).await;
            if let Err(e) = &r {
                tidb_base::log_warn!(
                    "reloading the ported domain's InfoSchema failed",
                    "error" => e,
                );
            }
            self.registry.ported_reloaded(r.is_ok());
        }
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
            killer: None,
            kill_cancel: None,
        };
        let rows = poc_executor::execute(&plan, &mut snapshot, &cx).await?;
        let loaded = schema.version();
        let mut jobs = Vec::new();
        for row in rows {
            let (Some(job_id), Some(version)) = (int(row.first()), int(row.get(1))) else {
                let row_str = format!("{row:?}");
                tidb_base::log_warn!("unexpected mysql.tidb_mdl_info row", "row" => row_str.as_str());
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
            tidb_base::log_info!(
                "mdl gets lock, update self version to owner",
                "jobID" => job.job_id,
                "version" => job.version,
            );
            match self.put_mono(&key, &job.version.to_string()).await {
                Ok(()) => {
                    reported.insert(job.job_id, job.version);
                    self.registry.count(|s| s.mdl_reports += 1);
                }
                Err(e) => {
                    let err_str = e.to_string();
                    tidb_base::log_warn!(
                        "mdl gets lock, update self version to owner failed",
                        "jobID" => job.job_id,
                        "version" => job.version,
                        "error" => err_str.as_str(),
                    );
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

    fn current_info(&self) -> ServerInfo {
        let sid = self.server_id.load(Ordering::SeqCst);
        let mut info = self.cfg.info.clone();
        if sid != 0 {
            info.server_id = sid;
        }
        info
    }

    async fn store_server_info(&self) {
        let lease = self.registration.lock().await.as_ref().map(|r| r.lease);
        let Some(lease) = lease else {
            return;
        };
        let info = self.current_info();
        if let Err(e) = timeout(self.etcd().put(
            info.key(),
            info.to_json(),
            Some(PutOptions::new().with_lease(lease)),
        ))
        .await
        {
            let err_str = e.to_string();
            tidb_base::log_error!("StoreServerInfo failed", "error" => err_str.as_str());
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
        let info_rec = self.current_info();
        let info_key = info_rec.key();
        let info = info_rec.to_json();
        let version = self.registry.loaded_version().unwrap_or(0);
        let puts = async {
            timeout(
                self.etcd()
                    .put(info_key, info, Some(PutOptions::new().with_lease(lease))),
            )
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
        let lease_hex = format!("{lease:x}");
        tidb_base::log_info!(
            "registered in etcd",
            "ddl_id" => self.cfg.info.ddl_id.as_str(),
            "lease" => lease_hex.as_str(),
            "version" => version,
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
                    let err_str = e.to_string();
                    tidb_base::log_warn!(
                        "lease keepalive: cannot open stream",
                        "error" => err_str.as_str(),
                    );
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };
            loop {
                let sent = Instant::now();
                if let Err(e) = keeper.keep_alive().await {
                    let err_str = e.to_string();
                    tidb_base::log_warn!("lease keepalive failed", "error" => err_str.as_str());
                    break;
                }
                match tokio::time::timeout(ETCD_OP_TIMEOUT, stream.message()).await {
                    Ok(Ok(Some(resp))) if resp.ttl() > 0 => {
                        let deadline = sent + Duration::from_secs(resp.ttl() as u64);
                        self.registry.lease_renewed(epoch, deadline);
                    }
                    Ok(Ok(Some(_))) => {
                        let lease_hex = format!("{lease:x}");
                        tidb_base::log_warn!("lease expired", "lease" => lease_hex.as_str());
                        self.registry.lease_lost(epoch);
                        return;
                    }
                    Ok(Ok(None)) => {
                        tidb_base::log_warn!("lease keepalive stream ended");
                        break;
                    }
                    Ok(Err(e)) => {
                        let err_str = e.to_string();
                        tidb_base::log_warn!("lease keepalive failed", "error" => err_str.as_str());
                        break;
                    }
                    Err(_) => {
                        tidb_base::log_warn!("lease keepalive got no answer");
                        break;
                    }
                }
                tokio::time::sleep(interval).await;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    /// Deregister the node when it is not keeping up (Go has no such bound: a stuck Go TiDB
    /// blocks DDL until its lease expires). The bound (`max_staleness`) is at least the schema
    /// lease, and admission already stops when the bound is reached, so the node never
    /// deregisters while it still admits statements.
    async fn watchdog(self: Arc<Self>) {
        let mut stop = self.stop.subscribe();
        loop {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                _ = stop.changed() => return,
            }
            let now = Instant::now();
            if self.registry.should_deregister(now) {
                debug_assert!(
                    !self.registry.admissible(now),
                    "deregistering while statements are admitted"
                );
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
                tidb_base::log_warn!("deregistered (no lease held)", "why" => why);
            }
            return;
        };
        r.keepalive.abort();
        let started = Instant::now();
        match timeout(self.etcd().lease_revoke(r.lease)).await {
            Ok(_) => {}
            Err(e) => {
                let err_str = e.to_string();
                tidb_base::log_warn!(
                    "lease revoke failed; deleting the keys",
                    "error" => err_str.as_str(),
                );
                let _ = timeout(self.etcd().delete(self.cfg.info.key(), None)).await;
                let _ = timeout(
                    self.etcd()
                        .delete(keys::self_schema_version(&self.cfg.info.ddl_id), None),
                )
                .await;
            }
        }
        tidb_base::log_warn!(
            "deregistered from etcd",
            "why" => why,
            "elapsed" => started.elapsed(),
        );
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
