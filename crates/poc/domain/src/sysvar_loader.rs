//! The read-only node's driver of the domain's sysvar cache (D06.5 spec §8, #521; Go:
//! `Domain.LoadSysVarCacheLoop`, `rebuildSysVarCache`, `fetchTableValues`).
//!
//! Go's domain rebuilds its sysvar cache from `mysql.global_variables` at start, on every
//! etcd watch event of `/tidb/sysvars` (the key a `SET GLOBAL` on any instance puts) and
//! every 30 s, and every new session copies the session values from it
//! (`loadCommonGlobalVariablesIfNeeded`). Here the cache is tidb-systable's ported
//! [`SysVarCache`], the loop is the ported [`load_sys_var_cache_loop`] (run on a thread of
//! its own, because it blocks between events), and this module supplies its I/O:
//!
//! - `fetchTableValues`: the records of `mysql.global_variables` scanned at a fresh TSO and
//!   decoded ([`decode_global_variables`]);
//! - the etcd watch: a task on the runtime forwards each watch event to the loop and
//!   reports a closed watch, then watches again by itself.
//!
//! Differences from Go (D06.5 spec §8.4):
//! - the rebuild runs no SetGlobal hook ([`SysVarCache::rebuild`] without a hook session);
//! - a failed first rebuild does not stop the node: the loop keeps trying on every event,
//!   and until it succeeds the cache stays unbuilt and new sessions stay on the PoC (the
//!   router's step 0, "global variables not loaded");
//! - `metrics.LoadSysVarCacheCounter` is not kept.
//!
//! Built in every `--new-stack` mode (maintainer decision on #521): the new stack's
//! sessions read it, and so do the prototype's own decisions that Go takes from the
//! session's variables (the plan-cache warning of a PREPARE, D13.1 spec known difference
//! 13).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use etcd_client::{Client as EtcdClient, ConnectOptions};
use parking_lot::Mutex;
use poc_kv::KvClient;
use tidb_base::Error;
use tidb_systable::session_io::{
    LoopEvent, SYS_VAR_CACHE_KEY, SYS_VAR_CACHE_PERIOD, load_sys_var_cache_loop,
};
use tidb_systable::sysvar_cache::{SysVarCache, decode_global_variables};

use crate::privilege_loader::{load_mysql_table_infos_filtered, scan_table_records};
use crate::systable_io::EtcdNotifier;

/// Timeout of connecting to etcd and of creating the watch.
const ETCD_OP_TIMEOUT: Duration = Duration::from_secs(5);
/// The longest wait between two attempts to watch etcd.
const MAX_WATCH_BACKOFF: Duration = Duration::from_secs(30);

/// `fetchTableValues` as the loop calls it (blocking).
type Fetch = Box<dyn FnMut() -> Result<HashMap<String, String>, Error> + Send>;

/// The sysvar cache of `mysql.global_variables` and the loop that keeps it in step with the
/// cluster. Dropping the last handle (or [`SysVarSync::shutdown`]) ends the loop and the
/// watch.
#[derive(Clone)]
pub struct SysVarSync {
    inner: Arc<Inner>,
}

struct Inner {
    cache: Arc<SysVarCache>,
    /// Wakes the loop (watch events come from the watch task's clone; `Exit` from here).
    events: Mutex<Sender<LoopEvent>>,
    /// Stops the watch task.
    stop: tokio::sync::watch::Sender<bool>,
    /// The loop's thread (joined only by tests).
    #[cfg_attr(not(test), allow(dead_code))]
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Drop for Inner {
    // glue: Go's do.exit for LoadSysVarCacheLoop (the domain's Close)
    fn drop(&mut self) {
        let _ = self.events.lock().send(LoopEvent::Exit);
        self.stop.send_replace(true);
    }
}

impl SysVarSync {
    /// Starts the loop on a thread of its own and, with PD endpoints, the etcd watch of
    /// `/tidb/sysvars` on the current tokio runtime (it must be called inside one: the
    /// table is read through `kv` on it). Returns at once; the first rebuild runs on the
    /// loop's thread.
    // glue: the read-only node's start of Domain.LoadSysVarCacheLoop (D06.5 spec §8.1)
    pub fn start(kv: KvClient, pd_endpoints: Vec<String>) -> SysVarSync {
        let rt = tokio::runtime::Handle::current();
        let fetch_rt = rt.clone();
        let fetch: Fetch = Box::new(move || fetch_rt.block_on(fetch_table_values(&kv)));
        let notifier = EtcdNotifier {
            etcd: None,
            rt: rt.clone(),
        };
        let (sync, events) = SysVarSync::start_with(fetch, notifier, SYS_VAR_CACHE_PERIOD);
        if !pd_endpoints.is_empty() {
            let stop = sync.inner.stop.subscribe();
            rt.spawn(watch_loop(pd_endpoints, events, stop));
        }
        tidb_base::log_info!("sysvar cache loader started (mysql.global_variables)");
        sync
    }

    /// The loop over `fetch`, waking every `period` without an event; returns the handle
    /// and a sender of loop events (the watch's).
    // glue: the loop's thread and its event channel (D06.5 spec §8.1)
    fn start_with(
        fetch: Fetch,
        notifier: EtcdNotifier,
        period: Duration,
    ) -> (SysVarSync, Sender<LoopEvent>) {
        let cache = Arc::new(SysVarCache::new());
        let (tx, rx) = std::sync::mpsc::channel();
        let (stop, _) = tokio::sync::watch::channel(false);
        let loop_cache = Arc::clone(&cache);
        let thread = std::thread::Builder::new()
            .name("sysvar-cache".to_owned())
            .spawn(move || run(&loop_cache, fetch, &rx, period, &notifier))
            .expect("spawn the sysvar cache thread");
        let sync = SysVarSync {
            inner: Arc::new(Inner {
                cache,
                events: Mutex::new(tx.clone()),
                stop,
                thread: Mutex::new(Some(thread)),
            }),
        };
        (sync, tx)
    }

    /// The cache every new session reads its global values from.
    pub fn cache(&self) -> &Arc<SysVarCache> {
        &self.inner.cache
    }

    /// Ends the loop and the watch (also done when the last handle is dropped).
    pub fn shutdown(&self) {
        let _ = self.inner.events.lock().send(LoopEvent::Exit);
        self.inner.stop.send_replace(true);
    }
}

/// The loop's thread: Go's `LoadSysVarCacheLoop` (the ported [`load_sys_var_cache_loop`])
/// with the rebuild over `fetch`. Go returns the first rebuild's error to the bootstrap,
/// which stops the server; here the node keeps running with the cache unbuilt (new
/// sessions stay on the PoC) and the loop starts again at the next event (D06.5 spec §8.3).
// glue: the loop's I/O (events from the channel, the 30 s timer, sleeping) and its restart after a failed first rebuild
fn run(
    cache: &SysVarCache,
    mut fetch: Fetch,
    rx: &Receiver<LoopEvent>,
    period: Duration,
    notifier: &EtcdNotifier,
) {
    // Go's rebuildSysVarCache(ctx) on the loop's session; no hook session: no SetGlobal hook
    // runs (D06.5 spec §8.4). The first success is logged once (after a failed start it
    // ends the time in which new sessions stay on the PoC).
    // `tidbrs_sysvar_cache_loaded` is 0 until then.
    let mut built = false;
    tidb_metrics::sysvar_cache_loaded(false);
    let mut rebuild = || {
        cache.rebuild(None, &mut *fetch)?;
        if !std::mem::replace(&mut built, true) {
            tidb_metrics::sysvar_cache_loaded(true);
            tidb_base::log_info!("global variables loaded from mysql.global_variables");
        }
        Ok(())
    };
    let mut next = || {
        let event = match rx.recv_timeout(period) {
            Ok(event) => event,
            Err(RecvTimeoutError::Timeout) => LoopEvent::Tick,
            Err(RecvTimeoutError::Disconnected) => LoopEvent::Exit,
        };
        // Go logs this after its select for a watch event and for the timer alike
        // (`domain.go` line 2084).
        if matches!(event, LoopEvent::Watch | LoopEvent::Tick) {
            tidb_base::log_debug!("Rebuilding sysvar cache from etcd watch event.");
        }
        event
    };
    // The watch task watches again by itself after it reported a closed watch.
    let mut rewatch = || {};
    let mut sleep = std::thread::sleep;
    let mut warned = false;
    loop {
        match load_sys_var_cache_loop(&mut rebuild, &mut next, &mut rewatch, &mut sleep, notifier) {
            Ok(()) => break,
            Err(e) => {
                // Logged once, not at every retry (maintainer decision on #521).
                if !std::mem::replace(&mut warned, true) {
                    tidb_base::log_warn!(
                        "loading the global variables from mysql.global_variables failed; retrying, new-stack sessions stay on the PoC until it succeeds",
                        "error" => &e,
                    );
                }
                if next() == LoopEvent::Exit {
                    break;
                }
            }
        }
    }
    tidb_base::log_info!("LoadSysVarCacheLoop exited.");
}

/// `fetchTableValues`: `SELECT variable_name, variable_value FROM mysql.global_variables`,
/// as the records of the table scanned at a fresh TSO and decoded.
// glue: the scan behind Domain.fetchTableValues (the decoding is tidb-systable's decode_global_variables)
async fn fetch_table_values(kv: &KvClient) -> Result<HashMap<String, String>, Error> {
    let err = |e: crate::Error| tidb_base::errors::new(e.to_string());
    let ts = kv.current_ts().await.map_err(|e| err(e.into()))?;
    let tables = load_mysql_table_infos_filtered(kv, ts, &["global_variables"])
        .await
        .map_err(err)?;
    let Some(tbl) = tables.get("global_variables") else {
        return Err(tidb_base::errors::new(
            "mysql.global_variables not found in the cluster's metadata",
        ));
    };
    let records = scan_table_records(kv, ts, tbl.id).await.map_err(err)?;
    decode_global_variables(tbl, &records)
}

/// The etcd watch of `/tidb/sysvars` (Go: `do.etcdClient.Watch(ctx, sysVarCacheKey)`): every
/// event wakes the loop ([`LoopEvent::Watch`]); when an established watch ends, the loop
/// is told ([`LoopEvent::WatchClosed`], which rebuilds) and the watch is created again.
/// A watch that cannot be created is retried with a growing wait (Go's etcd client
/// retries by itself); the 30 s timer covers the time without a watch.
// glue: the etcd I/O of Domain.LoadSysVarCacheLoop
async fn watch_loop(
    pd_endpoints: Vec<String>,
    events: Sender<LoopEvent>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let mut backoff = Duration::from_secs(1);
    while !*stop.borrow() {
        match watch_once(&pd_endpoints, &events, &mut stop, &mut backoff).await {
            Ok(WatchEnd::Stop) => return,
            Ok(WatchEnd::Closed) => {
                tidb_base::log_warn!("LoadSysVarCacheLoop loop watch channel closed");
                if events.send(LoopEvent::WatchClosed).is_err() {
                    return;
                }
            }
            Err(e) => {
                let err_str = e.to_string();
                tidb_base::log_warn!(
                    "sysvar cache etcd watch failed; retrying with backoff",
                    "error" => err_str.as_str(),
                    "retry_in_secs" => backoff.as_secs(),
                );
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = stop.changed() => return,
        }
        backoff = (backoff * 2).min(MAX_WATCH_BACKOFF);
    }
}

/// How an established watch ended.
enum WatchEnd {
    /// The loader stops.
    Stop,
    /// The watch stream ended, was canceled or failed.
    Closed,
}

/// One watch: connect, watch the key, forward its events until it ends.
// glue: one etcd watch stream of Domain.LoadSysVarCacheLoop
async fn watch_once(
    pd_endpoints: &[String],
    events: &Sender<LoopEvent>,
    stop: &mut tokio::sync::watch::Receiver<bool>,
    backoff: &mut Duration,
) -> crate::Result<WatchEnd> {
    let mut etcd = tokio::time::timeout(
        ETCD_OP_TIMEOUT,
        EtcdClient::connect(
            pd_endpoints,
            Some(
                ConnectOptions::new()
                    .with_connect_timeout(ETCD_OP_TIMEOUT)
                    .with_keep_alive(Duration::from_secs(10), Duration::from_secs(5)),
            ),
        ),
    )
    .await
    .map_err(|_| crate::Error::Timeout)??;
    let (_watcher, mut stream) =
        tokio::time::timeout(ETCD_OP_TIMEOUT, etcd.watch(SYS_VAR_CACHE_KEY, None))
            .await
            .map_err(|_| crate::Error::Timeout)??;
    *backoff = Duration::from_secs(1);
    loop {
        let msg = tokio::select! {
            m = stream.message() => m,
            _ = stop.changed() => return Ok(WatchEnd::Stop),
        };
        match msg {
            Ok(Some(msg)) if msg.canceled() => return Ok(WatchEnd::Closed),
            Ok(Some(msg)) => {
                if msg.events().is_empty() {
                    continue;
                }
                if events.send(LoopEvent::Watch).is_err() {
                    return Ok(WatchEnd::Stop);
                }
            }
            Ok(None) | Err(_) => return Ok(WatchEnd::Closed),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

    use super::*;

    fn table(sql_mode: &str) -> HashMap<String, String> {
        [("sql_mode".to_owned(), sql_mode.to_owned())].into()
    }

    fn notifier(rt: &tokio::runtime::Runtime) -> EtcdNotifier {
        EtcdNotifier {
            etcd: None,
            rt: rt.handle().clone(),
        }
    }

    /// Waits (up to 60 s: the first WARN resolves an error's backtrace, which takes seconds
    /// on a CI runner with debug info) until `f` holds.
    fn eventually(f: impl Fn() -> bool) {
        for _ in 0..6000 {
            if f() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("condition not reached");
    }

    /// The first rebuild fails (TiKV unreachable): the node keeps running with the cache
    /// unbuilt, and the next event (here the timer) builds it.
    #[test]
    fn failed_first_load_is_retried_and_the_cache_stays_unbuilt_meanwhile() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&calls);
        let fetch: Fetch = Box::new(move || {
            if c.fetch_add(1, SeqCst) == 0 {
                Err(tidb_base::errors::new("TiKV unreachable"))
            } else {
                Ok(table("ANSI_QUOTES"))
            }
        });
        let (sync, events) =
            SysVarSync::start_with(fetch, notifier(&rt), Duration::from_secs(3600));
        eventually(|| calls.load(SeqCst) >= 1);
        assert!(sync.cache().needs_rebuild());
        events.send(LoopEvent::Tick).unwrap();
        eventually(|| !sync.cache().needs_rebuild());
        assert_eq!(sync.cache().global_var("sql_mode").unwrap(), "ANSI_QUOTES");
    }

    /// A watch event (a `SET GLOBAL` on another instance) rebuilds the cache with the
    /// table's new value; a failed rebuild keeps the last good values (Go); a closed watch
    /// rebuilds too; the timer rebuilds without an event; dropping the handle ends the loop.
    #[test]
    fn watch_events_and_the_timer_rebuild_and_drop_ends_the_loop() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let value = Arc::new(Mutex::new(Ok::<_, String>("".to_owned())));
        let v = Arc::clone(&value);
        let calls = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&calls);
        let fetch: Fetch = Box::new(move || {
            c.fetch_add(1, SeqCst);
            match &*v.lock() {
                Ok(m) => Ok(table(m)),
                Err(e) => Err(tidb_base::errors::new(e.clone())),
            }
        });
        let (sync, events) =
            SysVarSync::start_with(fetch, notifier(&rt), Duration::from_secs(3600));
        eventually(|| !sync.cache().needs_rebuild());
        let cache = Arc::clone(sync.cache());
        let sql_mode = || cache.global_var("sql_mode").unwrap();
        assert_eq!(sql_mode(), "");

        *value.lock() = Ok("PIPES_AS_CONCAT".to_owned());
        events.send(LoopEvent::Watch).unwrap();
        eventually(|| sql_mode() == "PIPES_AS_CONCAT");

        let before = calls.load(SeqCst);
        *value.lock() = Err("scan failed".to_owned());
        events.send(LoopEvent::Watch).unwrap();
        eventually(|| calls.load(SeqCst) > before);
        assert_eq!(sql_mode(), "PIPES_AS_CONCAT");

        *value.lock() = Ok("ANSI_QUOTES".to_owned());
        events.send(LoopEvent::WatchClosed).unwrap();
        eventually(|| sql_mode() == "ANSI_QUOTES");

        // The timer: a loop with a short period rebuilds by itself.
        let fetch: Fetch = Box::new(|| Ok(table("REAL_AS_FLOAT")));
        let (timed, _tx) = SysVarSync::start_with(fetch, notifier(&rt), Duration::from_millis(20));
        eventually(|| !timed.cache().needs_rebuild());

        let thread = sync.inner.thread.lock().take().unwrap();
        drop(sync);
        thread.join().unwrap();
    }
}
