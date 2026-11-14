//! Snapshot reads against TiKV for the read-only node.
//!
//! A thin wrapper over [`tikv_client`] (our fork with coprocessor support): PD TSO, point and
//! batch gets, range scans and coprocessor requests, all at a fixed snapshot timestamp and with
//! TiDB-like retry budgets. Errors are mapped into [`Error`].
//!
//! ```no_run
//! # async fn demo() -> poc_kv::Result<()> {
//! let client = poc_kv::KvClient::connect(vec!["127.0.0.1:2379".to_owned()]).await?;
//! let ts = client.current_ts().await?;
//! let mut snapshot = client.snapshot(ts);
//! let value = snapshot.get(b"some key".to_vec()).await?;
//! # let _ = value;
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]

mod error;

use std::time::Duration;
use std::time::Instant;

use tikv_client::Backoff;
use tikv_client::BoundRange;
use tikv_client::RetryOptions;
use tikv_client::Timestamp;
use tikv_client::TimestampExt;
use tikv_client::TransactionClient;
use tikv_client::TransactionOptions;

pub use crate::error::Error;
pub use crate::error::Result;
pub use tidb_kv::kv::ReqTypeDAG as REQ_TYPE_DAG;
pub use tidb_kv::kv::Request as CopRequest;

/// The coprocessor store of D07.2 over this client (`tidb_store::copr`, ADR 0008).
type CoprStore = tidb_store::copr::store::Store<tidb_store::copr::client_rust::ClientRustStore>;

/// The read timestamp of [`KvClient::latest_snapshot`] (Go: `math.MaxUint64`).
pub const MAX_TS: u64 = u64::MAX;

/// A key-value pair returned by gets and scans.
pub type KvPair = (Vec<u8>, Vec<u8>);

/// A half-open key range `[start, end)`. An empty `end` means "to the end of the keyspace".
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeyRange {
    pub start: Vec<u8>,
    pub end: Vec<u8>,
}

impl KeyRange {
    pub fn new(start: Vec<u8>, end: Vec<u8>) -> KeyRange {
        KeyRange { start, end }
    }

    /// True if the range contains no key. An empty `end` is unbounded, so never empty.
    pub fn is_empty(&self) -> bool {
        !self.end.is_empty() && self.start >= self.end
    }
}

impl From<KeyRange> for BoundRange {
    fn from(r: KeyRange) -> BoundRange {
        if r.end.is_empty() {
            (r.start..).into()
        } else {
            (r.start..r.end).into()
        }
    }
}

/// client-go's base budget of a read's `Backoffer`, in ms of total sleep: `getMaxBackoff`,
/// `batchGetMaxBackoff`, `scannerNextMaxBackoff` (txnsnapshot) and TiDB's `CopNextMaxBackoff`.
pub const BASE_READ_BACKOFF_MS: u64 = 20_000;

/// TiDB's default `tidb_backoff_weight` (client-go `kv.DefBackOffWeight`, kv/variables.go:71).
/// `NewBackofferWithVars` multiplies every budget by it (config/retry/backoff.go:106-110), so
/// a read really gets 40 s. The variable is not wired to sessions yet (D05.2 §4): when it is,
/// the budget must follow the session's value.
pub const DEFAULT_BACKOFF_WEIGHT: u64 = 2;

/// Retry budgets of TiDB / client-go.
///
/// client-go gives each read one `Backoffer` with a budget of total sleep of
/// [`BASE_READ_BACKOFF_MS`] × `tidb_backoff_weight` (40 s by default), with one curve per kind
/// of error (`BoRegionMiss` 2..500 ms, `BoTiKVRPC` 100..2000 ms, `BoTxnLockFast` 10..3000 ms,
/// ...), and does not charge `ServerIsBusy` waits to it (up to 10 minutes).
/// `Backoff::time_budget` is that `Backoffer` (client-rust#12): given for both region errors and
/// lock waits, a read's region retries, its lock resolution and its lock waits all draw from
/// the same budget (`specs/D07.1-store/client-go-parity.md` P0-1).
pub fn tidb_retry_options() -> RetryOptions {
    let budget = Backoff::time_budget(BASE_READ_BACKOFF_MS * DEFAULT_BACKOFF_WEIGHT);
    RetryOptions::new(budget.clone(), budget)
}

/// A connection to a TiKV cluster through PD.
#[derive(Clone)]
pub struct KvClient {
    inner: TransactionClient,
    retry: RetryOptions,
    /// The ported store driver's `KVStore` when `TIDB_RS_POINT_GET=d07.1`: snapshots then
    /// do their point gets and batch gets through `tidb_store::driver` (D07.1, VS1-18), so
    /// that the fault scenarios (tools/ab/faults) compare it with this crate's own reads.
    driver: Option<std::sync::Arc<tidb_store::driver::tikv::KVStore>>,
    /// The coprocessor client's store (D07.2); its workers run on the ExecPool once
    /// [`with_exec_pool`](KvClient::with_exec_pool) is called.
    copr: std::sync::Arc<CoprStore>,
}

/// Whether `TIDB_RS_POINT_GET=d07.1` is set (see [`KvClient`]).
fn driver_point_gets() -> bool {
    std::env::var("TIDB_RS_POINT_GET").is_ok_and(|v| v == "d07.1")
}

/// The copr store over `inner`, its workers on `pool` (N3 ExecPool) when given.
fn copr_store(
    inner: &TransactionClient,
    pool: Option<tidb_runtime::pool::ExecPool>,
) -> Result<CoprStore> {
    let store =
        tidb_store::copr::client_rust::ClientRustStore::new(std::sync::Arc::new(inner.clone()))
            .map_err(Error::Copr)?;
    Ok(tidb_store::copr::store::new_store(
        std::sync::Arc::new(store),
        pool,
        rand_seed(),
    ))
}

/// Go `rand.Uint32()` for the replica-read seed of `copr.NewStore`.
fn rand_seed() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos())
}

impl KvClient {
    /// Connect to the PD endpoints (`host:port`), with one connection per TiKV store.
    pub async fn connect(pd_endpoints: Vec<String>) -> Result<KvClient> {
        Self::connect_with(pd_endpoints, 1).await
    }

    /// Connect to the PD endpoints, opening `store_connections` gRPC connections to each TiKV
    /// store (client-go: `grpc-connection-count`, default 4). Requests are spread over them.
    pub async fn connect_with(
        pd_endpoints: Vec<String>,
        store_connections: usize,
    ) -> Result<KvClient> {
        let config = tikv_client::Config::default().with_grpc_connection_count(store_connections);
        let inner = TransactionClient::new_with_config(pd_endpoints, config)
            .await
            .map_err(Error::from)?;
        let driver = driver_point_gets()
            .then(|| tidb_store::driver::tikv::KVStore::new(std::sync::Arc::new(inner.clone())));
        let copr = std::sync::Arc::new(copr_store(&inner, None)?);
        Ok(KvClient {
            inner,
            retry: tidb_retry_options(),
            driver,
            copr,
        })
    }

    /// Run the coprocessor client's workers on `pool` (M-GO) instead of the current runtime.
    pub fn with_exec_pool(mut self, pool: tidb_runtime::pool::ExecPool) -> Result<KvClient> {
        self.copr = std::sync::Arc::new(copr_store(&self.inner, Some(pool))?);
        Ok(self)
    }

    /// Override the retry budgets used by snapshots created from now on (mainly for tests).
    pub fn with_retry_options(mut self, retry: RetryOptions) -> KvClient {
        self.retry = retry;
        self
    }

    /// A fresh timestamp from PD's TSO (TiDB's `start_ts`), as the 64-bit TSO version.
    /// Awaited at once, like Go's `GetTS` (`GetTSAsync().Wait()`).
    pub async fn current_ts(&self) -> Result<u64> {
        let start = Instant::now();
        let ts = self.fetch_ts().await;
        tikv_client::metrics::tso_wait(Duration::ZERO, start.elapsed());
        ts
    }

    /// Request a timestamp now and wait for it later (Go `GetTSAsync` / `Wait`, TiDB's
    /// `txnFuture`). Must be called inside a tokio runtime.
    pub fn ts_future(&self) -> TsFuture {
        let kv = self.clone();
        TsFuture {
            handle: tokio::spawn(async move { kv.fetch_ts().await }),
            requested: Instant::now(),
        }
    }

    async fn fetch_ts(&self) -> Result<u64> {
        Ok(self
            .inner
            .current_timestamp()
            .await
            .map_err(Error::from)?
            .version())
    }

    /// A snapshot that reads the latest committed version of each key without a TSO
    /// (TiDB's MaxTS autocommit point get). Only point gets are meant to use it: they are sent
    /// at high priority and handle locks like client-go does for `math.MaxUint64` reads (see
    /// `docs/adr/0005-schema-sync.md`). Every get observes some committed state at or after
    /// the moment it is sent; different gets of one such snapshot are not one snapshot.
    pub fn latest_snapshot(&self) -> KvSnapshot {
        self.snapshot(MAX_TS)
    }

    /// A read-only snapshot at `ts` (a TSO version).
    pub fn snapshot(&self, ts: u64) -> KvSnapshot {
        let options = TransactionOptions::new_optimistic()
            .read_only()
            .retry_options(self.retry.clone());
        KvSnapshot {
            inner: self.inner.snapshot(Timestamp::from_version(ts), options),
            ts,
            driver: self
                .driver
                .as_ref()
                .map(|kv| tidb_store::driver::txn::snapshot::new_snapshot(kv.get_snapshot(ts))),
            copr: self.copr.clone(),
        }
    }
}

/// A timestamp requested from PD ahead of the point where it is needed.
pub struct TsFuture {
    handle: tokio::task::JoinHandle<Result<u64>>,
    requested: Instant,
}

impl TsFuture {
    /// Wait for the timestamp, recording PD client's `tso_async_wait` (request to wait) and
    /// `wait` (the wait itself) like pd/client's `tso.Request.Wait`.
    pub async fn wait(self) -> Result<u64> {
        let waiting = Instant::now();
        let ts = match self.handle.await {
            Ok(ts) => ts,
            Err(e) => Err(Error::from(tikv_client::Error::JoinError(e))),
        };
        tikv_client::metrics::tso_wait(
            waiting.saturating_duration_since(self.requested),
            waiting.elapsed(),
        );
        ts
    }
}

/// Register tikv-client's client-go / PD client metrics (`tidb_tikvclient_*`,
/// `pd_client_*`) so they are exported from the start.
pub fn register_client_metrics() {
    tikv_client::metrics::register();
}

/// Reads at one snapshot timestamp. Locks met on the read path are resolved (or waited for)
/// by tikv-client, using the client's retry options.
pub struct KvSnapshot {
    inner: tikv_client::Snapshot,
    ts: u64,
    /// The D07.1 snapshot at the same timestamp (`TIDB_RS_POINT_GET=d07.1`).
    driver: Option<tidb_store::driver::txn::snapshot::TikvSnapshot>,
    copr: std::sync::Arc<CoprStore>,
}

/// Batch size of [`KvSnapshot::scan_all`].
const SCAN_BATCH: u32 = 256;

impl KvSnapshot {
    /// The snapshot timestamp.
    pub fn ts(&self) -> u64 {
        self.ts
    }

    pub async fn get(&mut self, key: Vec<u8>) -> Result<Option<Vec<u8>>> {
        if let Some(snap) = &self.driver {
            use tidb_kv::kv::Getter;
            let ctx = tidb_kv::Context::background();
            return match snap.get(&ctx, &tidb_kv::Key(key), &[]).await {
                Ok(entry) => Ok(Some(entry.value)),
                Err(e) if tidb_kv::error::is_err_not_found(&e) => Ok(None),
                Err(e) => Err(Error::Driver(Box::new(e))),
            };
        }
        self.inner.get(key).await.map_err(Error::from)
    }

    /// Values of the keys that exist, in no particular order.
    pub async fn batch_get(&mut self, keys: Vec<Vec<u8>>) -> Result<Vec<KvPair>> {
        if let Some(snap) = &self.driver {
            use tidb_kv::kv::BatchGetter;
            let ctx = tidb_kv::Context::background();
            let keys: Vec<tidb_kv::Key> = keys.into_iter().map(tidb_kv::Key).collect();
            let values = snap
                .batch_get(&ctx, &keys, &[])
                .await
                .map_err(|e| Error::Driver(Box::new(e)))?;
            return Ok(values.into_iter().map(|(k, v)| (k, v.value)).collect());
        }
        let pairs = self.inner.batch_get(keys).await.map_err(Error::from)?;
        Ok(pairs.map(|p| (p.0.into(), p.1)).collect())
    }

    /// At most `limit` pairs of `range`, in ascending key order.
    pub async fn scan(&mut self, range: KeyRange, limit: u32) -> Result<Vec<KvPair>> {
        let pairs = self
            .inner
            .scan(BoundRange::from(range), limit)
            .await
            .map_err(Error::from)?;
        Ok(pairs.map(|p| (p.0.into(), p.1)).collect())
    }

    /// Every pair of `range`, in ascending key order. Meant for small ranges such as meta
    /// hashes; it buffers the whole result.
    pub async fn scan_all(&mut self, range: KeyRange) -> Result<Vec<KvPair>> {
        let mut out: Vec<KvPair> = Vec::new();
        let mut start = range.start;
        loop {
            let batch = self
                .scan(KeyRange::new(start, range.end.clone()), SCAN_BATCH)
                .await?;
            let n = batch.len();
            out.extend(batch);
            if n < SCAN_BATCH as usize {
                return Ok(out);
            }
            // Continue right after the last key.
            let mut next = out.last().map(|p| p.0.clone()).unwrap_or_default();
            next.push(0);
            start = next;
        }
    }

    /// Send a coprocessor request at this snapshot through the coprocessor client of D07.2
    /// (`copr.CopClient.Send`, then `Next` until the end) and return every response's data,
    /// in the iterator's order. `req.start_ts` is the snapshot's timestamp.
    pub async fn coprocessor(&mut self, mut req: CopRequest) -> Result<Vec<Vec<u8>>> {
        req.start_ts = self.ts;
        let client = self.copr.get_client();
        // `tikv.Variables`: TiDB's default backoff weight (client-go parity P0-1).
        let vars: tidb_kv::kv::OptionValue = Box::new(tidb_store::copr::tikv::Variables {
            backoff_weight: DEFAULT_BACKOFF_WEIGHT as i64,
            killed: None,
        });
        let option = tidb_kv::kv::ClientSendOption::default();
        let mut resp = client.send(&req, Some(&vars), &option);
        let ctx = tidb_kv::Context::background();
        let mut out = Vec::new();
        loop {
            match resp.next(&ctx).await {
                Ok(Some(r)) => out.push(r.get_data().to_vec()),
                Ok(None) => break,
                Err(e) => {
                    let _ = resp.close();
                    return Err(Error::Copr(e));
                }
            }
        }
        let _ = resp.close();
        Ok(out)
    }
}

/// Read-write transactions (D05.2 PR C: the SET GLOBAL writer of tidb-systable, spec §6.2;
/// tidb-rs is otherwise read-only). The tikv-client transaction is returned as is: the
/// writer classifies its commit errors with [`is_write_conflict`].
impl KvClient {
    /// An optimistic transaction with a start ts from the TSO.
    /// A statement that finds nothing to write drops it without commit (a read-only
    /// optimistic transaction holds no locks), so the drop check is off.
    #[allow(clippy::result_large_err)] // tikv_client::Error, the client's own Result
    pub async fn begin_optimistic(&self) -> tikv_client::Result<tikv_client::Transaction> {
        let options = TransactionOptions::new_optimistic()
            .retry_options(self.retry.clone())
            .drop_check(tikv_client::CheckLevel::None);
        self.inner.begin_with_options(options).await
    }
}

/// Whether a commit failed on a write conflict, which Go's autocommit retry handles by
/// re-running the statement in a new transaction (`kv.ErrWriteConflict`). A key that already
/// exists (an inserted key another transaction committed first) is not retryable in Go
/// either: the client gets the duplicate-entry error.
pub fn is_write_conflict(e: &tikv_client::Error) -> bool {
    use tikv_client::Error as E;
    match e {
        E::KeyError(k) => k.conflict.is_some(),
        E::MultipleKeyErrors(v) => v.iter().any(is_write_conflict),
        E::PessimisticLockError { inner, .. } => is_write_conflict(inner),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_range_emptiness() {
        assert!(!KeyRange::new(b"a".to_vec(), vec![]).is_empty());
        assert!(!KeyRange::new(b"a".to_vec(), b"b".to_vec()).is_empty());
        assert!(KeyRange::new(b"b".to_vec(), b"b".to_vec()).is_empty());
        assert!(KeyRange::new(b"c".to_vec(), b"b".to_vec()).is_empty());
    }

    /// Both budgets are client-go's 40 s of sleep (20 s × `tidb_backoff_weight` 2), not
    /// tikv-client's ~1.5 s default.
    #[test]
    fn lock_backoff_budget_is_tidb_like() {
        assert!(tidb_retry_options().region_backoff.is_time_budget());
        assert!(tidb_retry_options().lock_backoff.is_time_budget());
        let mut lock = tidb_retry_options().lock_backoff;
        let mut total = std::time::Duration::ZERO;
        while let Some(d) = lock.next_delay_duration() {
            total += d;
        }
        assert!((38..=42).contains(&total.as_secs()), "{total:?}");

        let mut region = tidb_retry_options().region_backoff;
        let mut total = std::time::Duration::ZERO;
        while let Some(d) = region.next_delay_duration() {
            total += d;
        }
        assert!((38..=42).contains(&total.as_secs()), "{total:?}");
    }
}
