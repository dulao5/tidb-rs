//! Active `start_ts` tracking and `minStartTS` computation for `/tidb/server/minstartts/<ddl_id>`
//! reporting (Go: `pkg/domain/infosync/info.go` `ReportMinStartTS`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tidb_systable::sysvar_cache::SysVarCache;

/// Default `tidb_gc_max_wait_time` in seconds (Go: `variable.DefTiDBGCMaxWaitTime` = 24h).
pub const DEFAULT_GC_MAX_WAIT_SECS: i64 = 86_400;

/// Default periodic reporting interval of `/tidb/server/minstartts/<ddl_id>`
/// (Go: `infosync.ReportInterval` = 30s).
pub const DEFAULT_MIN_START_TS_INTERVAL: Duration = Duration::from_secs(30);

/// `MaxTS` (`math.MaxUint64`), used by autocommit point gets without a real TSO.
pub const MAX_TS: u64 = u64::MAX;

/// System variable name controlling the GC max wait time threshold.
const GC_MAX_WAIT_TIME_VAR: &str = "tidb_gc_max_wait_time";

#[derive(Default)]
struct TrackerState {
    next_id: u64,
    active: HashMap<u64, u64>,
}

/// Thread-safe registry of open transactions' and active read statements' `start_ts` values.
#[derive(Default)]
pub struct MinStartTsTracker {
    state: Mutex<TrackerState>,
}

/// RAII guard for one active `start_ts`; automatically unregisters it on drop.
pub struct StartTsGuard {
    tracker: Arc<MinStartTsTracker>,
    id: u64,
    start_ts: u64,
}

impl std::fmt::Debug for StartTsGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StartTsGuard")
            .field("id", &self.id)
            .field("start_ts", &self.start_ts)
            .finish()
    }
}

impl StartTsGuard {
    /// Returns the registered `start_ts`.
    pub fn start_ts(&self) -> u64 {
        self.start_ts
    }
}

impl Drop for StartTsGuard {
    fn drop(&mut self) {
        self.tracker.state.lock().active.remove(&self.id);
    }
}

impl MinStartTsTracker {
    /// Creates an empty tracker.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Registers an active `start_ts`. Returns `None` for `0` and `MAX_TS` (`u64::MAX`).
    pub fn register(self: &Arc<Self>, start_ts: u64) -> Option<StartTsGuard> {
        if start_ts == 0 || start_ts == MAX_TS {
            return None;
        }
        let mut s = self.state.lock();
        let id = s.next_id;
        s.next_id = s.next_id.wrapping_add(1);
        s.active.insert(id, start_ts);
        Some(StartTsGuard {
            tracker: Arc::clone(self),
            id,
            start_ts,
        })
    }

    /// Number of currently tracked active `start_ts` guards.
    pub fn active_count(&self) -> usize {
        self.state.lock().active.len()
    }

    /// Computes the node's `minStartTS` given `current_ts` from PD and `gc_max_wait_secs`
    /// (`tidb_gc_max_wait_time`).
    pub fn compute(&self, current_ts: u64, gc_max_wait_secs: i64) -> u64 {
        let s = self.state.lock();
        compute_min_start_ts(current_ts, gc_max_wait_secs, s.active.values().copied())
    }
}

/// Computes `minStartTS` matching Go's `InfoSyncer.ReportMinStartTS` (`infosync/info.go:828-870`):
/// - `now_ms = current_ts >> 18`
/// - `initial_min_ts = now_ms << 18`
/// - `lower_limit = now_ms.saturating_sub(max_wait_ms) << 18`
/// - picks the smallest active `ts` such that `ts > lower_limit && ts < initial_min_ts`,
///   ignoring `0` and `u64::MAX`.
pub fn compute_min_start_ts(
    current_ts: u64,
    gc_max_wait_secs: i64,
    active: impl Iterator<Item = u64>,
) -> u64 {
    let now_ms = current_ts >> 18;
    let mut min_ts = now_ms << 18;
    let max_wait_ms = (gc_max_wait_secs.max(0) as u64).saturating_mul(1_000);
    let lower_limit = now_ms.saturating_sub(max_wait_ms) << 18;
    for ts in active {
        if ts == 0 || ts == MAX_TS {
            continue;
        }
        if ts > lower_limit && ts < min_ts {
            min_ts = ts;
        }
    }
    min_ts
}

/// Reads the live `tidb_gc_max_wait_time` setting (in seconds) from `cache`, falling back
/// to [`DEFAULT_GC_MAX_WAIT_SECS`] (`86400`) when absent, unbuilt, or invalid.
pub fn gc_max_wait_time_secs(cache: Option<&SysVarCache>) -> i64 {
    let Some(cache) = cache else {
        return DEFAULT_GC_MAX_WAIT_SECS;
    };
    if cache.needs_rebuild() {
        return DEFAULT_GC_MAX_WAIT_SECS;
    }
    match cache.global_var(GC_MAX_WAIT_TIME_VAR) {
        Ok(v) => v.trim().parse::<i64>().unwrap_or(DEFAULT_GC_MAX_WAIT_SECS),
        Err(_) => DEFAULT_GC_MAX_WAIT_SECS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tso(ms: u64, logical: u64) -> u64 {
        (ms << 18) | logical
    }

    #[test]
    fn compute_handles_empty_active_and_special_ts() {
        let now_ms = 1_700_000_000_123_u64;
        let current = tso(now_ms, 42);
        let expected_now = tso(now_ms, 0);

        // Empty active -> current physical ms (logical cleared).
        assert_eq!(
            compute_min_start_ts(current, DEFAULT_GC_MAX_WAIT_SECS, std::iter::empty()),
            expected_now
        );

        // 0 and MAX_TS are ignored.
        assert_eq!(
            compute_min_start_ts(current, DEFAULT_GC_MAX_WAIT_SECS, [0, MAX_TS].into_iter()),
            expected_now
        );
    }

    #[test]
    fn compute_picks_oldest_within_gc_max_wait_and_ignores_older() {
        let now_ms = 1_700_000_000_000_u64;
        let current = tso(now_ms, 7);
        let active_younger = tso(now_ms - 5_000, 3);
        let active_older = tso(now_ms - 60_000, 9);
        let too_old = tso(now_ms - 86_400_000, 0); // equal to lower_limit -> excluded (<=)
        let way_too_old = tso(now_ms - 100_000_000, 1);

        assert_eq!(
            compute_min_start_ts(
                current,
                DEFAULT_GC_MAX_WAIT_SECS,
                [active_younger, active_older, too_old, way_too_old].into_iter()
            ),
            active_older
        );

        // With a shorter gc_max_wait_secs (30s), active_older (60s old) is ignored.
        assert_eq!(
            compute_min_start_ts(
                current,
                30,
                [active_younger, active_older, too_old].into_iter()
            ),
            active_younger
        );
    }

    #[test]
    fn tracker_guards_register_and_unregister_on_drop() {
        let tracker = MinStartTsTracker::new();
        assert!(tracker.register(0).is_none());
        assert!(tracker.register(MAX_TS).is_none());
        assert_eq!(tracker.active_count(), 0);

        let now_ms = 1_700_000_000_000_u64;
        let current = tso(now_ms, 5);
        let ts1 = tso(now_ms - 10_000, 1);
        let ts2 = tso(now_ms - 20_000, 2);

        let g1 = tracker.register(ts1).unwrap();
        let g2_dup = tracker.register(ts2).unwrap();
        let g2 = tracker.register(ts2).unwrap();
        assert_eq!(tracker.active_count(), 3);
        assert_eq!(tracker.compute(current, DEFAULT_GC_MAX_WAIT_SECS), ts2);

        // Dropping one duplicate guard keeps ts2 active via the other guard.
        drop(g2_dup);
        assert_eq!(tracker.active_count(), 2);
        assert_eq!(tracker.compute(current, DEFAULT_GC_MAX_WAIT_SECS), ts2);

        // Dropping the second ts2 guard advances min_start_ts to ts1.
        drop(g2);
        assert_eq!(tracker.active_count(), 1);
        assert_eq!(tracker.compute(current, DEFAULT_GC_MAX_WAIT_SECS), ts1);

        // Dropping the last guard returns current physical ts.
        drop(g1);
        assert_eq!(tracker.active_count(), 0);
        assert_eq!(
            tracker.compute(current, DEFAULT_GC_MAX_WAIT_SECS),
            tso(now_ms, 0)
        );
    }

    #[test]
    fn gc_max_wait_time_reads_sysvar_cache_or_defaults() {
        assert_eq!(gc_max_wait_time_secs(None), DEFAULT_GC_MAX_WAIT_SECS);
        let cache = SysVarCache::new();
        // Unbuilt cache returns default.
        assert_eq!(
            gc_max_wait_time_secs(Some(&cache)),
            DEFAULT_GC_MAX_WAIT_SECS
        );
        cache
            .rebuild(None, &mut || {
                Ok(HashMap::from([(
                    GC_MAX_WAIT_TIME_VAR.to_owned(),
                    "1200".to_owned(),
                )]))
            })
            .unwrap();
        assert_eq!(gc_max_wait_time_secs(Some(&cache)), 1200);
    }
}
