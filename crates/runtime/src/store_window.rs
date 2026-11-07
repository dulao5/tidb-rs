//! Q2, the per-store in-flight window (N3 3.6, decision D35).
//!
//! One [`StoreWindows`] per server, reachable from `ExecCtx` (foundation-interfaces). Every
//! request to a TiKV store takes an [`InflightPermit`] before it is sent and drops it when the
//! response arrives, whether or not the caller has consumed it (counting in-flight RPCs, not
//! buffered responses: otherwise an IndexLookUp could deadlock with its index side holding the
//! window). How much is buffered stays Go's `sendRate` (N3 3.1 第 6 条).
//!
//! Two independent limits live here:
//!
//! - **Go `tidb_store_limit`** (client-go `kv.StoreLimit`, default 0 = off), ported L1 from
//!   client-go `RegionRequestSender.getStoreToken` / `releaseStoreToken`: when a store already
//!   has `limit` requests in flight, the request **fails** with [`StoreTokenLimitError`]
//!   (client-go `tikverr.ErrTokenLimit`; the store driver turns it into TiDB error 9008,
//!   block D07.1). Go's check-then-add is not atomic ("preferring this for avoiding check in
//!   loop"), so the limit can be overshot slightly under contention; the port keeps that.
//! - **The window** (Rust only): a per-store cap that makes the request **wait**. Default
//!   unlimited, which is Go's behaviour. AIMD driven by TiKV's read-pool schedule wait and
//!   per-resource-group shares are functional exceptions, off: switching them on reports
//!   "not supported" until their benchmarks decide. [`StoreWindows::observe_schedule_wait`] is
//!   the hook AIMD will use; today it only records the signal.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use parking_lot::RwLock;
use prometheus::Histogram;
use prometheus::IntGauge;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;

use crate::NotSupported;
use crate::admission::Waiting;
use crate::metrics;
use crate::metrics::Queue;
use crate::metrics::QueueMetrics;

/// client-go `tikverr.ErrTokenLimit`: the store's `tidb_store_limit` is used up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreTokenLimitError {
    pub store_id: u64,
}

impl std::fmt::Display for StoreTokenLimitError {
    // go-dep: github.com/tikv/client-go/v2@v2.0.8-0.20260803075849-c3b50791b9fb/error.ErrTokenLimit.Error L1
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Store token is up to the limit, store id = {}.",
            self.store_id
        )
    }
}

impl std::error::Error for StoreTokenLimitError {}

/// Q2 configuration. The defaults are Go's.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoreWindowsConfig {
    /// Per-store window; `None` is unlimited (Go).
    pub window: Option<usize>,
    /// Go `tidb_store_limit` / config `tikv-client.store-limit` at start-up (0 = off).
    pub store_limit: i64,
    /// Functional exception: AIMD on the window. Off.
    pub aimd: bool,
    /// Functional exception: per-resource-group shares of the window. Off.
    pub group_shares: bool,
}

/// Per-store state (client-go keeps `tokenCount` on its `Store`).
struct StoreState {
    store_id: u64,
    token_count: AtomicI64,
    window: Option<Arc<Semaphore>>,
    inflight: IntGauge,
    schedule_wait: Histogram,
}

/// Q2: the server's per-store in-flight windows.
pub struct StoreWindows {
    /// client-go `kv.StoreLimit` (a process global in Go; one per server here).
    store_limit: AtomicI64,
    window: Option<usize>,
    stores: RwLock<HashMap<u64, Arc<StoreState>>>,
    metrics: QueueMetrics,
}

/// One in-flight request to a store. Drop it when the response arrives.
pub struct InflightPermit {
    state: Arc<StoreState>,
    store_token: bool,
    _window: Option<OwnedSemaphorePermit>,
}

impl Drop for InflightPermit {
    // glue: client-go defers releaseStoreToken after send; the window permit is released too
    fn drop(&mut self) {
        if self.store_token {
            release_store_token(&self.state);
        }
        self.state.inflight.dec();
    }
}

impl StoreWindows {
    /// Fails when a functional exception is switched on: they are not implemented yet.
    // glue: N3 3.6 Q2
    pub fn new(cfg: &StoreWindowsConfig) -> Result<StoreWindows, NotSupported> {
        if cfg.aimd {
            return Err(NotSupported("AIMD on the per-store window"));
        }
        if cfg.group_shares {
            return Err(NotSupported(
                "per-resource-group shares of the per-store window",
            ));
        }
        if cfg.window == Some(0) {
            return Err(NotSupported("a per-store window of 0"));
        }
        Ok(StoreWindows {
            store_limit: AtomicI64::new(cfg.store_limit),
            window: cfg.window,
            stores: RwLock::new(HashMap::new()),
            metrics: QueueMetrics::new(Queue::StoreWindow),
        })
    }

    /// Set Go `tidb_store_limit` (client-go `kv.StoreLimit.Store`; config reload and the
    /// global variable both set it).
    // glue: client-go kv.StoreLimit.Store
    pub fn set_store_limit(&self, limit: i64) {
        self.store_limit.store(limit, Ordering::Relaxed);
    }

    // glue: client-go kv.StoreLimit.Load
    pub fn store_limit(&self) -> i64 {
        self.store_limit.load(Ordering::Relaxed)
    }

    // glue: per-store state lookup (client-go keeps it on the store cache entry)
    fn state(&self, store_id: u64) -> Arc<StoreState> {
        if let Some(s) = self.stores.read().get(&store_id) {
            return s.clone();
        }
        self.stores
            .write()
            .entry(store_id)
            .or_insert_with(|| {
                Arc::new(StoreState {
                    store_id,
                    token_count: AtomicI64::new(0),
                    window: self.window.map(|n| Arc::new(Semaphore::new(n))),
                    inflight: metrics::store_inflight(store_id),
                    schedule_wait: metrics::store_schedule_wait(store_id),
                })
            })
            .clone()
    }

    /// Take a permit for one request to `store_id` (`addr` labels the client-go metric):
    /// first the window (waits; unlimited by default), then Go's store limit (fails with
    /// [`StoreTokenLimitError`] when used up, as client-go does before sending).
    /// Cancel-safe.
    // glue: N3 3.6 Q2 (client-go sendReqState.send's store-limit block + the Rust-only window)
    pub async fn acquire(
        &self,
        store_id: u64,
        addr: &str,
    ) -> Result<InflightPermit, StoreTokenLimitError> {
        let state = self.state(store_id);
        let window = match &state.window {
            None => None,
            Some(sem) => {
                let start = Instant::now();
                let waiting = Waiting::new(&self.metrics.waiting);
                let p = sem
                    .clone()
                    .acquire_owned()
                    .await
                    .expect("the window semaphore is never closed");
                drop(waiting);
                self.metrics.wait.observe(start.elapsed().as_secs_f64());
                Some(p)
            }
        };
        let mut store_token = false;
        let limit = self.store_limit();
        if limit > 0 {
            get_store_token(&state, addr, limit)?;
            store_token = true;
        }
        state.inflight.inc();
        Ok(InflightPermit {
            state,
            store_token,
            _window: window,
        })
    }

    /// Requests in flight to `store_id`.
    // glue: accessor
    pub fn inflight(&self, store_id: u64) -> i64 {
        self.stores
            .read()
            .get(&store_id)
            .map_or(0, |s| s.inflight.get())
    }

    /// TiKV's read-pool schedule wait of one coprocessor response
    /// (`ScanDetailV2.read_pool_schedule_wait_nanos`): the congestion signal AIMD will use.
    /// Today it is only recorded.
    // glue: N3 3.6 Q2 rule 3 (AIMD hook, off)
    pub fn observe_schedule_wait(&self, store_id: u64, wait: Duration) {
        self.state(store_id)
            .schedule_wait
            .observe(wait.as_secs_f64());
    }
}

// go-dep: github.com/tikv/client-go/v2@v2.0.8-0.20260803075849-c3b50791b9fb/internal/locate.RegionRequestSender.getStoreToken L1
fn get_store_token(st: &StoreState, addr: &str, limit: i64) -> Result<(), StoreTokenLimitError> {
    // Checking limit is not thread safe, preferring this for avoiding load in loop.
    let count = st.token_count.load(Ordering::SeqCst);
    if count < limit {
        // Adding tokenCount is no thread safe, preferring this for avoiding check in loop.
        st.token_count.fetch_add(1, Ordering::SeqCst);
        return Ok(());
    }
    metrics::store_limit_token_error(addr, st.store_id);
    Err(StoreTokenLimitError {
        store_id: st.store_id,
    })
}

// go-dep: github.com/tikv/client-go/v2@v2.0.8-0.20260803075849-c3b50791b9fb/internal/locate.RegionRequestSender.releaseStoreToken L1
fn release_store_token(st: &StoreState) {
    let count = st.token_count.load(Ordering::SeqCst);
    // Decreasing tokenCount is no thread safe, preferring this for avoiding check in loop.
    if count > 0 {
        st.token_count.fetch_sub(1, Ordering::SeqCst);
        return;
    }
    log::warn!("release store token failed, count equals to 0");
}
