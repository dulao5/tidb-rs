//! The node's schema-sync state shared by the sync loop and the sessions: the newest loaded
//! schema, the health state machine that decides whether statements may use it ("synced
//! mode"), and the registry of in-flight synced statements the MDL protocol drains.
//!
//! Everything here is synchronous and takes the current [`Instant`] as an argument, so the
//! state machine and the drain logic are unit-tested without a cluster.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use tidb_meta::InfoSchema;

/// Bounds of the health state machine.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Synced mode needs a completed reload + MDL cycle at most this long ago, and the node
    /// may lag `global_schema_version` at most this long. Past either bound the node
    /// deregisters.
    pub max_staleness: Duration,
    /// Synced mode (and accepting a synced statement's result) needs the lease to be proven
    /// alive for at least this much longer.
    pub lease_margin: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_staleness: Duration::from_secs(5),
            lease_margin: Duration::from_secs(3),
        }
    }
}

/// A pending DDL job step from `mysql.tidb_mdl_info` (Go: `jobsVerMap` / `jobsIDsMap`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MdlJob {
    pub job_id: i64,
    /// The schema version this step needs every node to reach.
    pub version: i64,
    /// Tables the job touches.
    pub table_ids: Vec<i64>,
}

impl MdlJob {
    /// `table_ids` as stored (comma-separated ids, Go: `util.Str2Int64Map`). Unparsable parts
    /// are skipped, as Go does.
    pub fn parse_table_ids(s: &str) -> Vec<i64> {
        s.split(',').filter_map(|p| p.trim().parse().ok()).collect()
    }
}

/// Health inputs. See [`Health::admissible`] and [`Health::should_deregister`].
#[derive(Clone, Debug, Default)]
pub(crate) struct Health {
    /// When the current registration completed; `None` while not registered.
    registered_at: Option<Instant>,
    /// The lease is alive at least until then (keepalive send time + granted TTL).
    lease_deadline: Option<Instant>,
    /// Start of the newest full cycle (reload + MDL report) that *started* after the current
    /// registration. A reload at a TSO taken after registering is what makes the owner's
    /// later waits include this node (see the ADR), so earlier cycles do not count.
    cycle_ok_at: Option<Instant>,
    /// Newest `global_schema_version` seen in etcd.
    global_seen: i64,
    /// Since when the loaded version has been below `global_seen`.
    behind_since: Option<Instant>,
}

impl Health {
    fn lease_ok(&self, now: Instant, limits: &Limits) -> bool {
        self.lease_deadline
            .is_some_and(|d| now + limits.lease_margin < d)
    }

    fn behind_too_long(&self, now: Instant, limits: &Limits) -> bool {
        self.behind_since
            .is_some_and(|t| now >= t + limits.max_staleness)
    }

    /// Whether a statement may start in synced mode now.
    pub(crate) fn admissible(&self, now: Instant, limits: &Limits) -> bool {
        self.registered_at.is_some()
            && self
                .cycle_ok_at
                .is_some_and(|t| now < t + limits.max_staleness)
            && self.lease_ok(now, limits)
            && !self.behind_too_long(now, limits)
    }

    /// Whether the node must give up its registration so that DDL is not blocked by it: the
    /// lease is (nearly) gone, or the sync loop has made no progress for too long, or it has
    /// lagged the global version for too long.
    pub(crate) fn should_deregister(&self, now: Instant, limits: &Limits) -> bool {
        let Some(registered_at) = self.registered_at else {
            return false;
        };
        let progress = self
            .cycle_ok_at
            .map_or(registered_at, |t| t.max(registered_at));
        !self.lease_ok(now, limits)
            || now >= progress + limits.max_staleness
            || self.behind_too_long(now, limits)
    }
}

/// One in-flight synced statement.
#[derive(Clone, Debug)]
struct Entry {
    /// Version of the schema the statement uses.
    version: i64,
    /// Registration epoch it was admitted in.
    epoch: u64,
    /// Tables it reads; `None` (all tables) until planned.
    tables: Option<Vec<i64>>,
}

#[derive(Default)]
struct State {
    schema: Option<Arc<InfoSchema>>,
    health: Health,
    /// Incremented on every deregistration: statements admitted under an older epoch may no
    /// longer return results, and no longer hold back MDL reports.
    epoch: u64,
    entries: HashMap<u64, Entry>,
    next_id: u64,
}

/// Counters for tests and logs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SyncStats {
    /// Statements admitted to synced mode.
    pub admitted: u64,
    /// Synced statements whose result was discarded (deregistered or lease not proven alive
    /// when the read finished) and re-run on the fallback path.
    pub invalidated: u64,
    pub registrations: u64,
    pub deregistrations: u64,
    /// Full reloads (new schema versions installed).
    pub reloads: u64,
    /// `all_schema_by_job_versions` reports written.
    pub mdl_reports: u64,
}

/// Shared schema-sync state.
pub struct Registry {
    limits: Limits,
    state: Mutex<State>,
    invalidated: AtomicU64,
    admitted: AtomicU64,
    counters: Mutex<SyncStats>,
}

/// A statement admitted to synced mode: the schema to use and its registry entry.
pub struct Admission {
    pub schema: Arc<InfoSchema>,
    pub guard: StmtGuard,
}

/// Registry entry of an in-flight synced statement; removed on drop. Keep it until the
/// statement's response has been written (Go holds the MDL until the statement ends).
pub struct StmtGuard {
    registry: Arc<Registry>,
    id: u64,
    epoch: u64,
}

impl StmtGuard {
    /// Narrow the entry to the tables the plan reads (from "all tables").
    pub fn set_tables(&self, tables: Vec<i64>) {
        let mut state = self.registry.lock();
        if let Some(e) = state.entries.get_mut(&self.id) {
            e.tables = Some(tables);
        }
    }

    /// Whether a result computed with the admitted schema may be returned: the node is
    /// still in the registration epoch it was admitted in, and the lease is proven alive now.
    /// Call it after the statement's reads have completed. If it fails, the owner may have
    /// stopped waiting for this node (or may do so before the lease margin), so the reads may
    /// have seen data two schema versions ahead; discard them.
    pub fn results_valid(&self) -> bool {
        self.results_valid_at(Instant::now())
    }

    fn results_valid_at(&self, now: Instant) -> bool {
        let state = self.registry.lock();
        let ok = state.epoch == self.epoch && state.health.lease_ok(now, &self.registry.limits);
        drop(state);
        if !ok {
            self.registry.invalidated.fetch_add(1, Ordering::Relaxed);
        }
        ok
    }
}

impl Drop for StmtGuard {
    fn drop(&mut self) {
        self.registry.lock().entries.remove(&self.id);
    }
}

impl Registry {
    pub fn new(limits: Limits) -> Arc<Registry> {
        Arc::new(Registry {
            limits,
            state: Mutex::default(),
            invalidated: AtomicU64::new(0),
            admitted: AtomicU64::new(0),
            counters: Mutex::default(),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("schema sync registry lock")
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Admit a statement to synced mode if the node is healthy: returns the newest loaded
    /// schema and a registry entry (all tables, until [`StmtGuard::set_tables`]). Taking the
    /// schema and inserting the entry under one lock is what lets the MDL check rely on the
    /// registry: a statement either is in it, or started after the newer schema was installed.
    pub fn admit(self: &Arc<Self>, now: Instant) -> Option<Admission> {
        let mut state = self.lock();
        if !state.health.admissible(now, &self.limits) {
            return None;
        }
        let schema = state.schema.clone()?;
        let id = state.next_id;
        state.next_id += 1;
        let epoch = state.epoch;
        state.entries.insert(
            id,
            Entry {
                version: schema.version(),
                epoch,
                tables: None,
            },
        );
        drop(state);
        self.admitted.fetch_add(1, Ordering::Relaxed);
        Some(Admission {
            schema,
            guard: StmtGuard {
                registry: self.clone(),
                id,
                epoch,
            },
        })
    }

    /// Whether [`Registry::admit`] would admit now.
    pub fn admissible(&self, now: Instant) -> bool {
        self.lock().health.admissible(now, &self.limits)
    }

    /// The newest loaded schema if synced mode is available (for name resolution that
    /// needs no MDL entry, e.g. `USE` and `PREPARE`).
    pub fn schema_if_admissible(&self, now: Instant) -> Option<Arc<InfoSchema>> {
        let state = self.lock();
        if state.health.admissible(now, &self.limits) {
            state.schema.clone()
        } else {
            None
        }
    }

    /// The newest loaded schema (healthy or not).
    pub fn schema(&self) -> Option<Arc<InfoSchema>> {
        self.lock().schema.clone()
    }

    pub fn loaded_version(&self) -> Option<i64> {
        self.lock().schema.as_ref().map(|s| s.version())
    }

    pub fn should_deregister(&self, now: Instant) -> bool {
        self.lock().health.should_deregister(now, &self.limits)
    }

    pub fn is_registered(&self) -> bool {
        self.lock().health.registered_at.is_some()
    }

    pub fn stats(&self) -> SyncStats {
        let mut s = *self.counters.lock().expect("stats lock");
        s.admitted = self.admitted.load(Ordering::Relaxed);
        s.invalidated = self.invalidated.load(Ordering::Relaxed);
        s
    }

    pub(crate) fn count(&self, f: impl FnOnce(&mut SyncStats)) {
        f(&mut self.counters.lock().expect("stats lock"));
    }

    /// Install a newly loaded schema (newer versions only).
    pub(crate) fn install(&self, schema: Arc<InfoSchema>, now: Instant) {
        let mut state = self.lock();
        if state
            .schema
            .as_ref()
            .is_some_and(|s| s.version() >= schema.version())
        {
            return;
        }
        state.schema = Some(schema);
        Self::update_behind(&mut state, now);
    }

    /// A `global_schema_version` value seen in etcd.
    pub(crate) fn note_global(&self, version: i64, now: Instant) {
        let mut state = self.lock();
        state.health.global_seen = state.health.global_seen.max(version);
        Self::update_behind(&mut state, now);
    }

    fn update_behind(state: &mut State, now: Instant) {
        let loaded = state.schema.as_ref().map_or(0, |s| s.version());
        if loaded >= state.health.global_seen {
            state.health.behind_since = None;
        } else if state.health.behind_since.is_none() {
            state.health.behind_since = Some(now);
        }
    }

    /// The node's keys are in etcd now, with a lease alive until `lease_deadline`. Returns the
    /// registration epoch, to be passed to [`Registry::lease_renewed`].
    pub(crate) fn registered(&self, now: Instant, lease_deadline: Instant) -> u64 {
        let mut state = self.lock();
        state.health.registered_at = Some(now);
        state.health.lease_deadline = Some(lease_deadline);
        state.health.cycle_ok_at = None;
        drop(state);
        self.count(|s| s.registrations += 1);
        self.lock().epoch
    }

    /// A keepalive of the registration of `epoch` proved the lease alive until `deadline`.
    pub(crate) fn lease_renewed(&self, epoch: u64, deadline: Instant) {
        let mut state = self.lock();
        if state.epoch == epoch && state.health.registered_at.is_some() {
            let d = state.health.lease_deadline.get_or_insert(deadline);
            *d = (*d).max(deadline);
        }
    }

    /// etcd reported the lease of `epoch` gone.
    pub(crate) fn lease_lost(&self, epoch: u64) {
        let mut state = self.lock();
        if state.epoch == epoch {
            state.health.lease_deadline = None;
        }
    }

    /// A full cycle that started at `started` completed.
    pub(crate) fn cycle_completed(&self, started: Instant) {
        let mut state = self.lock();
        if state.health.registered_at.is_some_and(|r| started >= r) {
            let t = state.health.cycle_ok_at.get_or_insert(started);
            *t = (*t).max(started);
        }
    }

    /// Stop synced mode at once and start a new epoch: statements admitted before may not
    /// return results and no longer hold back MDL reports. Returns whether the node was
    /// registered.
    pub(crate) fn deregister(&self) -> bool {
        let mut state = self.lock();
        let was = state.health.registered_at.take().is_some();
        state.health.lease_deadline = None;
        state.health.cycle_ok_at = None;
        state.epoch += 1;
        drop(state);
        if was {
            self.count(|s| s.deregistrations += 1);
        }
        was
    }

    /// For each job: whether this node may report it (Go: `RemoveLockDDLJobs`). A job is
    /// held back while a statement of the current epoch uses a schema older than the job's
    /// version and reads one of its tables (or has not been planned yet).
    pub(crate) fn jobs_ready(&self, jobs: &[MdlJob]) -> Vec<bool> {
        let state = self.lock();
        jobs.iter()
            .map(|job| {
                !state.entries.values().any(|e| {
                    e.epoch == state.epoch
                        && e.version < job.version
                        && e.tables
                            .as_ref()
                            .is_none_or(|t| t.iter().any(|id| job.table_ids.contains(id)))
                })
            })
            .collect()
    }

    /// Number of in-flight synced statements (all epochs).
    pub fn in_flight(&self) -> usize {
        self.lock().entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);

    fn limits() -> Limits {
        Limits {
            max_staleness: 5 * S,
            lease_margin: 3 * S,
        }
    }

    fn schema(v: i64) -> Arc<InfoSchema> {
        Arc::new(InfoSchema::new(v, vec![]))
    }

    /// A registry registered at `t0` with a 90 s lease, schema 10 loaded and one cycle done.
    fn healthy(t0: Instant) -> Arc<Registry> {
        let r = Registry::new(limits());
        r.install(schema(10), t0);
        r.registered(t0, t0 + 90 * S);
        r.cycle_completed(t0);
        assert!(r.admissible(t0));
        r
    }

    #[test]
    fn admission_needs_a_cycle_started_after_registration() {
        let t0 = Instant::now();
        let r = Registry::new(limits());
        r.install(schema(10), t0);
        assert!(!r.admissible(t0), "not registered");
        assert!(!r.should_deregister(t0));
        r.registered(t0 + S, t0 + 91 * S);
        assert!(!r.admissible(t0 + S), "no cycle after registering yet");
        // A cycle that started before the registration completed does not count.
        r.cycle_completed(t0);
        assert!(!r.admissible(t0 + S));
        r.cycle_completed(t0 + 2 * S);
        assert!(r.admissible(t0 + 2 * S));
        assert!(r.admit(t0 + 2 * S).is_some());
    }

    #[test]
    fn stale_sync_loop_stops_admission_and_deregisters() {
        let t0 = Instant::now();
        let r = healthy(t0);
        assert!(r.admissible(t0 + 4 * S));
        assert!(!r.should_deregister(t0 + 4 * S));
        // No cycle for max_staleness: unhealthy, and the node must give up its keys.
        assert!(!r.admissible(t0 + 5 * S));
        assert!(r.admit(t0 + 5 * S).is_none());
        assert!(r.should_deregister(t0 + 5 * S));
        // A registered node whose first cycle never completes is bounded the same way.
        let r = Registry::new(limits());
        r.install(schema(10), t0);
        r.registered(t0, t0 + 90 * S);
        assert!(!r.should_deregister(t0 + 4 * S));
        assert!(r.should_deregister(t0 + 5 * S));
    }

    #[test]
    fn lease_bounds_admission_and_results() {
        let t0 = Instant::now();
        let r = healthy(t0);
        let epoch = r.lock().epoch;
        let a = r.admit(t0).unwrap();
        // Keepalives keep extending the deadline; cycles keep the loop fresh.
        r.lease_renewed(epoch, t0 + 100 * S);
        r.cycle_completed(t0 + 96 * S);
        assert!(r.admissible(t0 + 96 * S));
        assert!(a.guard.results_valid_at(t0 + 96 * S));
        // Within the margin of the deadline: no admission, results refused, deregister.
        assert!(!r.admissible(t0 + 97 * S));
        assert!(!a.guard.results_valid_at(t0 + 97 * S));
        assert!(r.should_deregister(t0 + 97 * S));
        // etcd says the lease is gone.
        let r = healthy(t0);
        let epoch = r.lock().epoch;
        r.lease_lost(epoch);
        assert!(!r.admissible(t0));
        assert!(r.should_deregister(t0));
    }

    #[test]
    fn lagging_the_global_version_is_bounded() {
        let t0 = Instant::now();
        let r = healthy(t0);
        r.note_global(10, t0);
        assert!(!r.should_deregister(t0 + S));
        r.note_global(11, t0 + S);
        r.cycle_completed(t0 + 5 * S);
        assert!(r.admissible(t0 + 5 * S), "behind for 4 s only");
        assert!(!r.admissible(t0 + 6 * S));
        assert!(r.should_deregister(t0 + 6 * S));
        r.install(schema(11), t0 + 6 * S);
        assert!(r.admissible(t0 + 6 * S));
        assert!(!r.should_deregister(t0 + 6 * S));
    }

    #[test]
    fn deregistration_starts_a_new_epoch() {
        let t0 = Instant::now();
        let r = healthy(t0);
        let old_epoch = r.lock().epoch;
        let a = r.admit(t0).unwrap();
        assert!(a.guard.results_valid_at(t0));
        assert!(r.deregister());
        assert!(!r.deregister(), "only once");
        assert!(!r.admissible(t0));
        assert!(!r.should_deregister(t0));
        // Results of statements admitted before are refused even after re-registering.
        assert!(!a.guard.results_valid_at(t0));
        r.registered(t0 + S, t0 + 91 * S);
        r.cycle_completed(t0 + S);
        assert!(r.admissible(t0 + S));
        assert!(!a.guard.results_valid_at(t0 + S));
        // A keepalive of the old registration does not extend the new one.
        r.lease_renewed(old_epoch, t0 + 1000 * S);
        assert!(!r.admissible(t0 + 89 * S));
        // Nor does it hold back MDL reports.
        let job = MdlJob {
            job_id: 1,
            version: 11,
            table_ids: vec![],
        };
        assert_eq!(r.jobs_ready(&[job]), vec![true]);
        let stats = r.stats();
        assert_eq!((stats.registrations, stats.deregistrations), (2, 1));
        assert_eq!(stats.invalidated, 2);
    }

    #[test]
    fn drain_waits_for_older_statements_on_the_jobs_tables() {
        let t0 = Instant::now();
        let r = healthy(t0);
        let job = |version, tables: &[i64]| MdlJob {
            job_id: 7,
            version,
            table_ids: tables.to_vec(),
        };
        let a = r.admit(t0).unwrap();
        assert_eq!(a.schema.version(), 10);
        // Unplanned statements count as reading every table.
        assert_eq!(r.jobs_ready(&[job(11, &[100])]), vec![false]);
        a.guard.set_tables(vec![200]);
        assert_eq!(
            r.jobs_ready(&[job(11, &[100]), job(11, &[100, 200]), job(10, &[200])]),
            vec![true, false, true],
            "other tables do not block; a statement on version >= the job's does not either"
        );
        // A newer schema installed: new statements use it and do not block the job.
        r.install(schema(11), t0);
        let b = r.admit(t0).unwrap();
        assert_eq!(b.schema.version(), 11);
        assert_eq!(
            r.jobs_ready(&[job(11, &[200])]),
            vec![false],
            "a still runs"
        );
        drop(a);
        assert_eq!(r.jobs_ready(&[job(11, &[200])]), vec![true]);
        drop(b);
        assert_eq!(r.in_flight(), 0);
        // Installing an older version is ignored.
        r.install(schema(9), t0);
        assert_eq!(r.loaded_version(), Some(11));
    }

    #[test]
    fn table_ids_parse_like_go() {
        assert_eq!(MdlJob::parse_table_ids("112,114"), vec![112, 114]);
        assert_eq!(MdlJob::parse_table_ids("112"), vec![112]);
        assert_eq!(MdlJob::parse_table_ids(""), Vec::<i64>::new());
        assert_eq!(MdlJob::parse_table_ids("1,x,3"), vec![1, 3]);
    }
}
