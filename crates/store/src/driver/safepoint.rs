//! Client-side GC safe point cache (`client-go/tikv/safepoint.go` & `CheckVisibility`, ADR 0008).
//!
//! TiDB / read-only nodes cache `/tidb/store/gcworker/saved_safe_point` from PD's etcd every
//! [`GC_SAFE_POINT_UPDATE_INTERVAL`] (`10s`, or [`GC_SAFE_POINT_QUICK_REPEAT_INTERVAL`] `1s` on
//! error). Snapshot reads call [`SafePointCache::check_visibility`] to reject stale snapshots
//! with `ErrGCTooEarly` (9006) once GC passes `start_ts`, or `ErrPDServerTimeout` (9001) if
//! the cache has not been refreshed within [`GC_SAFE_POINT_CACHE_INTERVAL`] minus
//! [`GC_CPU_TIME_INACCURACY_BOUND`] (`99s`).
//!
//! Keeping this cache in `tidb-store` (rank 11) with no `etcd-client` dependency lets both
//! [`crate::driver`] and [`crate::copr::client_rust`] check visibility locally while callers
//! with etcd access (such as `poc-kv`) drive the periodic updates.

use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tidb_base::Error;
use tidb_base::gofmt::Arg;

use crate::driver::error::{ErrGCTooEarly, ErrPDServerTimeout};

/// etcd key where Go TiDB's GC worker persists the current safe point
/// (`client-go/tikv/safepoint.go`).
pub const GC_SAVED_SAFE_POINT: &str = "/tidb/store/gcworker/saved_safe_point";

/// Safe point cache expiry window (`client-go/tikv/safepoint.go`: `GcSafePointCacheInterval`).
pub const GC_SAFE_POINT_CACHE_INTERVAL: Duration = Duration::from_secs(100);

/// CPU/clock inaccuracy safety margin subtracted from [`GC_SAFE_POINT_CACHE_INTERVAL`]
/// (`client-go/tikv/safepoint.go`: `gcCPUTimeInaccuracyBound`).
pub const GC_CPU_TIME_INACCURACY_BOUND: Duration = Duration::from_secs(1);

/// Normal background refresh period (`client-go/tikv/safepoint.go`: `gcSafePointUpdateInterval`).
pub const GC_SAFE_POINT_UPDATE_INTERVAL: Duration = Duration::from_secs(10);

/// Retry delay after a failed safe point load (`client-go/tikv/safepoint.go`: `gcSafePointQuickRepeatInterval`).
pub const GC_SAFE_POINT_QUICK_REPEAT_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug)]
struct SafePointState {
    safe_point: u64,
    updated_at: Instant,
}

/// Thread-safe cache of the cluster's GC safe point timestamp and last refresh instant.
#[derive(Debug)]
pub struct SafePointCache {
    state: Mutex<SafePointState>,
}

impl Default for SafePointCache {
    fn default() -> Self {
        Self::new()
    }
}

impl SafePointCache {
    /// Initializes an empty safe point cache (`safe_point = 0`, `updated_at = now`),
    /// matching `client-go/tikv/kv.go`.
    // go-l3: ADR-0008 client-go tikv NewKVStore safePoint init
    pub fn new() -> Self {
        Self {
            state: Mutex::new(SafePointState {
                safe_point: 0,
                updated_at: Instant::now(),
            }),
        }
    }

    /// Replaces the cached `safe_point` and `updated_at` timestamp (`KVStore.UpdateSPCache`).
    // go-l3: ADR-0008 client-go tikv KVStore.UpdateSPCache
    pub fn update(&self, safe_point: u64, at: Instant) {
        let mut s = self.state.lock();
        s.safe_point = safe_point;
        s.updated_at = at;
    }

    /// Returns the currently cached safe point TSO.
    pub fn safe_point(&self) -> u64 {
        self.state.lock().safe_point
    }

    /// Returns when the cache was last updated.
    pub fn updated_at(&self) -> Instant {
        self.state.lock().updated_at
    }

    /// Checks whether reading at `start_ts` is still visible with respect to the cached GC
    /// safe point (`client-go/tikv/kv.go`: `KVStore.CheckVisibility`).
    // go-l3: ADR-0008 client-go tikv KVStore.CheckVisibility
    pub fn check_visibility(&self, start_ts: u64) -> Result<(), Error> {
        let (cached_safe_point, cached_time) = {
            let s = self.state.lock();
            (s.safe_point, s.updated_at)
        };
        let max_age = GC_SAFE_POINT_CACHE_INTERVAL.saturating_sub(GC_CPU_TIME_INACCURACY_BOUND);
        if cached_time.elapsed() > max_age {
            return Err(ErrPDServerTimeout.gen_with_stack_by_args(vec![Arg::from(
                "start timestamp may fall behind safe point",
            )]));
        }
        if start_ts < cached_safe_point {
            let t1 = oracle_ts_to_go_time_string(start_ts);
            let t2 = oracle_ts_to_go_time_string(cached_safe_point);
            return Err(ErrGCTooEarly.gen_with_stack_by_args(vec![Arg::from(t1), Arg::from(t2)]));
        }
        Ok(())
    }
}

/// Formats a TSO timestamp with Go's `oracle.GetTimeFromTS(ts).String()` layout in local time.
// go-l3: ADR-0008 client-go oracle.GetTimeFromTS formatted via time.Time.String
pub fn oracle_ts_to_go_time_string(ts: u64) -> String {
    let ms = (ts >> 18) as i64;
    let sec = ms.div_euclid(1_000);
    let rem_ms = ms.rem_euclid(1_000);
    tidb_base::gostd::time::unix(sec, rem_ms * 1_000_000)
        .in_loc(&tidb_base::gostd::time::local())
        .string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tidb_base::errno;

    #[test]
    fn visibility_allows_max_ts_and_safe_timestamps() {
        let cache = SafePointCache::new();
        let now = Instant::now();
        let sp = 445_644_800_032_243_754_u64;
        cache.update(sp, now);
        assert_eq!(cache.safe_point(), sp);
        assert!(cache.check_visibility(sp).is_ok());
        assert!(cache.check_visibility(sp + 1).is_ok());
        assert!(cache.check_visibility(u64::MAX).is_ok());
    }

    #[test]
    fn visibility_returns_9006_when_start_ts_below_safe_point() {
        let cache = SafePointCache::new();
        let start_ts = (1_700_000_000_123_u64 << 18) + 42;
        let safe_point = (1_700_000_060_456_u64 << 18) + 1;
        cache.update(safe_point, Instant::now());
        let err = cache.check_visibility(start_ts).unwrap_err();
        assert_eq!(err.code(), errno::ErrGCTooEarly as isize);
        let sql_err = tidb_base::terror::to_sql_error(&err);
        assert_eq!(sql_err.code, 9006);
        assert_eq!(sql_err.state, "HY000");
        let want = format!(
            "GC life time is shorter than transaction duration, transaction starts at {}, GC safe point is {}",
            oracle_ts_to_go_time_string(start_ts),
            oracle_ts_to_go_time_string(safe_point),
        );
        assert_eq!(String::from_utf8_lossy(&sql_err.message), want);
    }

    #[test]
    fn visibility_returns_9001_when_cache_is_stale() {
        let cache = SafePointCache::new();
        let stale = Instant::now() - Duration::from_secs(100);
        cache.update(100, stale);
        let err = cache.check_visibility(200).unwrap_err();
        assert_eq!(err.code(), errno::ErrPDServerTimeout as isize);
        let sql_err = tidb_base::terror::to_sql_error(&err);
        assert_eq!(sql_err.code, 9001);
        assert_eq!(sql_err.state, "HY000");
        assert_eq!(
            String::from_utf8_lossy(&sql_err.message),
            "PD server timeout: start timestamp may fall behind safe point"
        );
        // MaxTS still checks cache freshness first, matching client-go.
        let err_max = cache.check_visibility(u64::MAX).unwrap_err();
        assert_eq!(err_max.code(), errno::ErrPDServerTimeout as isize);
    }
}
