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

use futures::Stream;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::stream::BoxStream;
use tikv_client::Backoff;
use tikv_client::BoundRange;
use tikv_client::RetryOptions;
use tikv_client::Timestamp;
use tikv_client::TimestampExt;
use tikv_client::TransactionClient;
use tikv_client::TransactionOptions;

pub use crate::error::Error;
pub use crate::error::Result;
pub use tikv_client::CopRequest;
pub use tikv_client::CopResponse;
pub use tikv_client::transaction::REQ_TYPE_DAG;

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

/// Retry budgets of TiDB / client-go.
///
/// client-go gives each read one `Backoffer` with a 20 s budget of total sleep (`Get`,
/// `BatchGet`, a scan page, a coprocessor task: `getMaxBackoff`, `batchGetMaxBackoff`,
/// `scannerNextMaxBackoff`, `copNextMaxBackoff`), with one curve per kind of error
/// (`BoRegionMiss` 2..500 ms, `BoTiKVRPC` 100..2000 ms, `BoTxnLockFast` 10..3000 ms, ...), and
/// does not charge `ServerIsBusy` waits to it (up to 10 minutes). `Backoff::time_budget` is that
/// `Backoffer` (client-rust#12): given for both region errors and lock waits, a read's region
/// retries, its lock resolution and its lock waits all draw from the same 20 s
/// (`specs/D07.1-store/client-go-parity.md` P0-1).
pub fn tidb_retry_options() -> RetryOptions {
    let budget = Backoff::time_budget(20_000);
    RetryOptions::new(budget.clone(), budget)
}

/// A connection to a TiKV cluster through PD.
#[derive(Clone)]
pub struct KvClient {
    inner: TransactionClient,
    retry: RetryOptions,
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
        Ok(KvClient {
            inner,
            retry: tidb_retry_options(),
        })
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
}

/// Batch size of [`KvSnapshot::scan_all`].
const SCAN_BATCH: u32 = 256;

impl KvSnapshot {
    /// The snapshot timestamp.
    pub fn ts(&self) -> u64 {
        self.ts
    }

    pub async fn get(&mut self, key: Vec<u8>) -> Result<Option<Vec<u8>>> {
        self.inner.get(key).await.map_err(Error::from)
    }

    /// Values of the keys that exist, in no particular order.
    pub async fn batch_get(&mut self, keys: Vec<Vec<u8>>) -> Result<Vec<KvPair>> {
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

    /// Send a coprocessor request at this snapshot. Unless the request sets its own retry
    /// options, it uses the client's (TiDB-like) ones.
    pub async fn coprocessor(&mut self, request: CopRequest) -> Result<CopStream> {
        let stream = self.inner.coprocessor(request).await.map_err(Error::from)?;
        Ok(CopStream {
            inner: stream.map_err(Error::from).boxed(),
        })
    }
}

/// The responses of a coprocessor request; ends after the first error.
pub struct CopStream {
    inner: BoxStream<'static, Result<CopResponse>>,
}

impl CopStream {
    /// Wait for all responses.
    pub async fn collect_all(self) -> Result<Vec<CopResponse>> {
        self.inner.try_collect().await
    }
}

impl Stream for CopStream {
    type Item = Result<CopResponse>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.inner.poll_next_unpin(cx)
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

    /// Both budgets are client-go's 20 s of sleep, not tikv-client's ~1.5 s default.
    #[test]
    fn lock_backoff_budget_is_tidb_like() {
        assert!(tidb_retry_options().region_backoff.is_time_budget());
        assert!(tidb_retry_options().lock_backoff.is_time_budget());
        let mut lock = tidb_retry_options().lock_backoff;
        let mut total = std::time::Duration::ZERO;
        while let Some(d) = lock.next_delay_duration() {
            total += d;
        }
        assert!(total.as_secs() >= 10, "{total:?}");
        assert!(total.as_secs() <= 30, "{total:?}");

        let mut region = tidb_retry_options().region_backoff;
        let mut total = std::time::Duration::ZERO;
        while let Some(d) = region.next_delay_duration() {
            total += d;
        }
        assert!((15..=25).contains(&total.as_secs()), "{total:?}");
    }
}
