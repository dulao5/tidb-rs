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

use crate::schema_validator::CheckResult;
use crate::schema_validator::SchemaCheckError;
use crate::schema_validator::SchemaCheckRetry;
use crate::schema_validator::SchemaValidator;

/// Default schema lease (Go: `config.DefSchemaLease`, the `--lease` flag): 45 s.
pub const DEF_SCHEMA_LEASE: Duration =
    Duration::from_nanos(tidb_base::config::DefSchemaLease.0 as u64);

/// Bounds of the health state machine.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// The schema lease (Go: `--lease`): statements may use the cached schema while the last
    /// successful reload is at most this long ago, measured in TSO ([`SchemaValidator`]).
    pub schema_lease: Duration,
    /// The node deregisters when it has completed no reload + MDL cycle for this long, or has
    /// lagged `global_schema_version` for this long. Must not be shorter than
    /// `schema_lease` (see [`Limits::check`]).
    pub max_staleness: Duration,
    /// Synced mode (and accepting a synced statement's result) needs the etcd lease to be
    /// proven alive for at least this much longer.
    pub lease_margin: Duration,
    /// How long a synced statement whose read TS is past the schema lease waits for a reload
    /// before it falls back to the ADR 0003 path (Go: `SchemaOutOfDateRetry*`).
    pub schema_check_retry: SchemaCheckRetry,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            schema_lease: DEF_SCHEMA_LEASE,
            max_staleness: DEF_SCHEMA_LEASE,
            lease_margin: Duration::from_secs(3),
            schema_check_retry: SchemaCheckRetry::default(),
        }
    }
}

impl Limits {
    /// The node must stop admitting statements before it deregisters, and stay registered
    /// while it admits them: once it is deregistered the DDL owner no longer waits for it, so
    /// a statement on the cached schema could see data two schema versions ahead. Admission
    /// lasts at most one lease after the last reload, deregistration comes `max_staleness`
    /// after the last cycle, so `max_staleness` must be at least the lease.
    pub fn check(&self) -> Result<(), String> {
        if self.schema_lease.is_zero() {
            return Err("the schema lease must be greater than 0".to_owned());
        }
        if self.max_staleness < self.schema_lease {
            return Err(format!(
                "schema-sync max staleness ({:?}) must not be shorter than the schema lease ({:?})",
                self.max_staleness, self.schema_lease
            ));
        }
        Ok(())
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
#[derive(Clone, Debug)]
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
    /// The schema lease (Go: `do.SchemaValidator`).
    validator: SchemaValidator,
}

impl Health {
    fn new(limits: &Limits) -> Health {
        Health {
            registered_at: None,
            lease_deadline: None,
            cycle_ok_at: None,
            global_seen: 0,
            behind_since: None,
            validator: SchemaValidator::new(limits.schema_lease),
        }
    }

    fn lease_ok(&self, now: Instant, limits: &Limits) -> bool {
        self.lease_deadline
            .is_some_and(|d| now + limits.lease_margin < d)
    }

    fn behind_too_long(&self, now: Instant, limits: &Limits) -> bool {
        self.behind_since
            .is_some_and(|t| now >= t + limits.max_staleness)
    }

    /// Whether a statement may start in synced mode now: the node is registered with its
    /// etcd lease proven alive, has completed a cycle since registering, and the schema lease
    /// has not expired (Go admits statements on the cached schema the same way, and checks
    /// the lease with `SchemaValidator`). A slow or failing reload therefore does not stop
    /// admission until the lease runs out (#136).
    ///
    /// Admission also requires that the node is not about to deregister, so the admission
    /// window always lies inside the registration window (see [`Limits::check`]).
    pub(crate) fn admissible(&self, now: Instant, limits: &Limits) -> bool {
        self.registered_at.is_some()
            && self.cycle_ok_at.is_some()
            && self.lease_ok(now, limits)
            && self.validator.is_started()
            && !self.validator.is_lease_expired(now)
            && !self.should_deregister(now, limits)
    }

    /// Whether the node must give up its registration so that DDL is not blocked by it: the
    /// etcd lease is (nearly) gone, or the sync loop has made no progress for `max_staleness`,
    /// or it has lagged the global version for that long. `max_staleness` is not shorter than
    /// the schema lease, so a reload that fails for less than a lease (a meta-region leader
    /// election, #136) neither deregisters the node nor stops admission.
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
    /// Version of the schema the statement uses.
    version: i64,
}

/// Outcome of [`StmtGuard::check_results`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResultCheck {
    /// The result may be returned.
    Valid,
    /// Discard the result and run the statement on the fallback (ADR 0003) path: the node
    /// left sync (deregistered, or its etcd lease is not proven alive), or the schema lease
    /// had expired at the read TS and no reload renewed it while waiting. Reads never return
    /// `ErrInfoSchemaExpired` (8027): Go's reads do not either.
    Fallback,
    /// The schema lease check failed with a definite result (Go: `ResultFail` →
    /// `ErrInfoSchemaChanged`, 8028).
    Err(SchemaCheckError),
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

    /// The schema lease check of the statement's reads: at its read TS `txn_ts` (`None` for a
    /// point get at MaxTS, checked with the local clock instead). Call it after the reads have
    /// completed.
    ///
    /// `txn_ts` must be a TS fetched for this statement *after* its admission (i.e. "now");
    /// never a transaction's `start_ts` (D05.4, explicit transactions, must not pass one).
    fn schema_check_at(&self, txn_ts: Option<u64>, now: Instant) -> CheckResult {
        let state = self.registry.lock();
        let v = &state.health.validator;
        match txn_ts {
            Some(ts) => v.check(ts, self.version),
            None => v.check_now(now, self.version),
        }
    }

    /// Whether a result read at `txn_ts` with the admitted schema may be returned: first the
    /// fencing of [`StmtGuard::results_valid`] (failing it means [`ResultCheck::Fallback`]),
    /// then the schema lease at the read TS. While the lease has expired at that TS, wait for
    /// a reload to renew it as Go does; if none does, return [`ResultCheck::Fallback`] (not
    /// Go's `ErrInfoSchemaExpired`: Go only returns it at commit, and its reads never fail
    /// with it; the fallback path re-reads the schema at a fresh TSO and succeeds once the
    /// meta region and TSO are available again).
    ///
    /// `txn_ts` only accepts a TS fetched for this statement after admission (i.e. "now");
    /// never a transaction's `start_ts`. `None` means a MaxTS point get. D05.4 (explicit
    /// transactions) must not pass a transaction's `start_ts` here: the lease bounds the
    /// schema's age at the moment of the read, not at the transaction's start.
    // go: SchemaChecker.CheckBySchemaVer L2 (async sleep; re-checks the fencing on every try; falls back instead of ErrInfoSchemaExpired)
    pub async fn check_results(&self, txn_ts: Option<u64>) -> ResultCheck {
        let retry = self.registry.limits.schema_check_retry;
        for _ in 0..retry.times {
            if !self.results_valid() {
                return ResultCheck::Fallback;
            }
            match self.schema_check_at(txn_ts, Instant::now()) {
                CheckResult::Succ => return ResultCheck::Valid,
                CheckResult::Fail => return ResultCheck::Err(SchemaCheckError::Changed),
                CheckResult::Unknown => tokio::time::sleep(retry.interval).await,
            }
        }
        ResultCheck::Fallback
    }
}

impl Drop for StmtGuard {
    fn drop(&mut self) {
        self.registry.lock().entries.remove(&self.id);
    }
}

impl Registry {
    pub fn new(limits: Limits) -> Arc<Registry> {
        debug_assert!(limits.check().is_ok(), "{:?}", limits.check());
        Arc::new(Registry {
            limits,
            state: Mutex::new(State {
                schema: None,
                health: Health::new(&limits),
                epoch: 0,
                entries: HashMap::new(),
                next_id: 0,
            }),
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
        let version = schema.version();
        state.entries.insert(
            id,
            Entry {
                version,
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
                version,
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

    /// A reload succeeded at the TSO `lease_grant_ts`, requested at `requested_at`; `old_ver`
    /// is the version loaded before it, and `full_load` whether it loaded a new schema (every
    /// load is a full one here). Renews the schema lease even if the version did not change.
    // go-part: Domain.Reload (`SchemaValidator.Reset` on a full load, then `Update`)
    pub(crate) fn reloaded(
        &self,
        lease_grant_ts: u64,
        requested_at: Instant,
        old_ver: i64,
        full_load: bool,
    ) {
        let mut state = self.lock();
        let curr_ver = state.schema.as_ref().map_or(0, |s| s.version());
        let validator = &mut state.health.validator;
        if full_load {
            validator.reset();
        }
        validator.update(lease_grant_ts, requested_at, old_ver, curr_ver);
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
        // Go: `do.SchemaValidator.Restart()` after the syncer restarted and reloaded.
        let loaded = state.schema.as_ref().map_or(0, |s| s.version());
        state.health.validator.restart(loaded);
        state.health.validator.forget_lease();
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
        // Go: `do.SchemaValidator.Stop()` when the schema syncer's session is gone.
        state.health.validator.stop();
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

    /// Whether no statement of the current epoch uses a schema older than `version` (on any
    /// table). `all_schema_versions/<ddl_id>` is advanced only then: with
    /// `tidb_enable_metadata_lock=OFF` the owner waits on that key instead of the per-job
    /// reports, and this keeps the two-version bound for in-flight statements in that mode too.
    pub(crate) fn drained_below(&self, version: i64) -> bool {
        let state = self.lock();
        !state
            .entries
            .values()
            .any(|e| e.epoch == state.epoch && e.version < version)
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
            schema_lease: 5 * S,
            max_staleness: 5 * S,
            lease_margin: 3 * S,
            schema_check_retry: SchemaCheckRetry {
                interval: Duration::from_millis(1),
                times: 3,
            },
        }
    }

    /// A TSO `secs` seconds after an arbitrary origin.
    fn tso(secs: f64) -> u64 {
        (1_700_000_000_000 + (secs * 1000.0) as u64) << 18
    }

    fn schema(v: i64) -> Arc<InfoSchema> {
        Arc::new(InfoSchema::new(v, vec![]))
    }

    /// A registry registered at `t0` with a 90 s lease, schema 10 loaded and one cycle done.
    fn healthy(t0: Instant) -> Arc<Registry> {
        let r = Registry::new(limits());
        r.install(schema(10), t0);
        r.registered(t0, t0 + 90 * S);
        r.reloaded(tso(0.0), t0, 10, false);
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
        // A reload before registering does not renew the schema lease either.
        r.cycle_completed(t0 + 2 * S);
        assert!(!r.admissible(t0 + 2 * S), "no reload after registering yet");
        r.reloaded(tso(2.0), t0 + 2 * S, 10, false);
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
        r.reloaded(tso(96.0), t0 + 96 * S, 10, false);
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
        r.reloaded(tso(5.0), t0 + 5 * S, 10, false);
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
        r.reloaded(tso(1.0), t0 + S, 10, false);
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
        assert!(!r.drained_below(11), "a is still on 10");
        drop(a);
        assert_eq!(r.jobs_ready(&[job(11, &[200])]), vec![true]);
        assert!(r.drained_below(11), "b is on 11");
        assert!(!r.drained_below(12));
        drop(b);
        assert_eq!(r.in_flight(), 0);
        // Installing an older version is ignored.
        r.install(schema(9), t0);
        assert_eq!(r.loaded_version(), Some(11));
    }

    /// #136: the meta region's leader is on a killed TiKV, so reloads fail for ~12 s. With
    /// Go's 45 s schema lease the node keeps admitting statements on the cached schema and
    /// stays registered; both end only when the lease runs out.
    #[test]
    fn failing_reloads_within_the_lease_keep_admitting() {
        let t0 = Instant::now();
        let r = Registry::new(Limits::default());
        r.install(schema(10), t0);
        r.registered(t0, t0 + 90 * S);
        r.reloaded(tso(0.0), t0, 10, false);
        r.cycle_completed(t0);
        let epoch = r.lock().epoch;
        for secs in [1, 5, 12, 30, 44] {
            let now = t0 + secs * S;
            // Keepalives go to etcd, not TiKV: they still succeed.
            r.lease_renewed(epoch, now + 90 * S);
            assert!(r.admissible(now), "{secs} s");
            assert!(!r.should_deregister(now), "{secs} s");
        }
        // The meta region is back after 12 s: a reload renews the lease from its TSO.
        r.reloaded(tso(12.0), t0 + 12 * S, 10, false);
        r.cycle_completed(t0 + 12 * S);
        assert!(r.admissible(t0 + 50 * S));
        // No reload for a whole lease: stop admitting, then deregister.
        let r = Registry::new(Limits::default());
        r.install(schema(10), t0);
        r.registered(t0, t0 + 1000 * S);
        r.reloaded(tso(0.0), t0, 10, false);
        r.cycle_completed(t0);
        assert!(r.admissible(t0 + 44 * S));
        assert!(!r.admissible(t0 + 45 * S));
        assert!(r.should_deregister(t0 + 45 * S));
    }

    /// Lucky's invariant on #136: admission window <= registration window. At every instant
    /// at which a statement may be admitted, the watchdog must not deregister; admission
    /// stops no later than deregistration.
    #[test]
    fn admission_stops_before_deregistration() {
        let t0 = Instant::now();
        let ms = Duration::from_millis(1);
        type Drive = Box<dyn Fn(&Registry)>;
        let scenarios: Vec<(&str, Limits, Drive)> = vec![
            (
                "stuck loop, max_staleness = lease",
                limits(),
                Box::new(|_| {}),
            ),
            (
                "stuck loop, max_staleness > lease",
                Limits {
                    max_staleness: 8 * S,
                    ..limits()
                },
                Box::new(|_| {}),
            ),
            (
                // Cycles (MDL reports) keep failing while reloads succeed.
                "reloads only",
                limits(),
                Box::new(move |r: &Registry| {
                    for i in 1..20 {
                        r.reloaded(
                            tso(f64::from(i)),
                            t0 + u32::try_from(i).unwrap() * S,
                            10,
                            false,
                        );
                    }
                }),
            ),
            (
                "lagging the global version",
                limits(),
                Box::new(move |r: &Registry| {
                    r.note_global(11, t0 + S);
                    for i in 1..20 {
                        r.reloaded(
                            tso(f64::from(i)),
                            t0 + u32::try_from(i).unwrap() * S,
                            10,
                            false,
                        );
                        r.cycle_completed(t0 + u32::try_from(i).unwrap() * S);
                    }
                }),
            ),
        ];
        for (name, limits, drive) in scenarios {
            let r = Registry::new(limits);
            r.install(schema(10), t0);
            r.registered(t0, t0 + 30 * S);
            r.reloaded(tso(0.0), t0, 10, false);
            r.cycle_completed(t0);
            drive(&r);
            let mut stopped_admitting = None;
            let mut deregister_at = None;
            for step in 0..40_000u32 {
                let now = t0 + step * ms;
                let admissible = r.admissible(now);
                let deregister = r.should_deregister(now);
                assert!(
                    !(admissible && deregister),
                    "{name}: admits and deregisters at {step} ms"
                );
                if !admissible && stopped_admitting.is_none() {
                    stopped_admitting = Some(step);
                }
                if deregister && deregister_at.is_none() {
                    deregister_at = Some(step);
                }
            }
            let (Some(stop), Some(dereg)) = (stopped_admitting, deregister_at) else {
                panic!("{name}: {stopped_admitting:?} {deregister_at:?}");
            };
            assert!(
                stop <= dereg,
                "{name}: stops admitting at {stop} ms, deregisters at {dereg} ms"
            );
        }
        // And deregistering stops admission at once, before the lease is revoked.
        let r = healthy(t0);
        assert!(r.deregister());
        assert!(!r.admissible(t0) && r.admit(t0).is_none());
    }

    #[test]
    fn max_staleness_must_cover_the_lease() {
        assert!(Limits::default().check().is_ok());
        assert_eq!(Limits::default().schema_lease, Duration::from_secs(45));
        let short = Limits {
            max_staleness: 5 * S,
            ..Limits::default()
        };
        assert!(short.check().unwrap_err().contains("must not be shorter"));
    }

    #[tokio::test]
    async fn results_are_checked_against_the_lease_at_the_read_ts() {
        let t0 = Instant::now();
        let r = healthy(t0);
        let a = r.admit(t0).unwrap();
        // Read TS within one lease of the reload's TS (Go: ResultSucc).
        assert_eq!(
            a.guard.check_results(Some(tso(4.9))).await,
            ResultCheck::Valid
        );
        // Past it (Go: ResultUnknown, retried): no reload renews it, so the statement falls
        // back to the ADR 0003 path instead of returning ErrInfoSchemaExpired (8027).
        let invalidated = r.stats().invalidated;
        let got = a.guard.check_results(Some(tso(5.0))).await;
        assert_eq!(got, ResultCheck::Fallback);
        assert_eq!(
            r.stats().invalidated,
            invalidated,
            "the node is still in sync"
        );
        // A MaxTS point get has no TS: the local clock decides (the lease is still fresh).
        assert_eq!(a.guard.check_results(None).await, ResultCheck::Valid);
        // A newer schema installed meanwhile: with MDL, the older statement passes.
        r.install(schema(11), t0);
        r.reloaded(tso(1.0), t0 + S, 10, true);
        assert_eq!(
            a.guard.check_results(Some(tso(100.0))).await,
            ResultCheck::Valid
        );
        // Deregistered: the result is discarded and the statement falls back.
        assert!(r.deregister());
        assert_eq!(
            a.guard.check_results(Some(tso(1.0))).await,
            ResultCheck::Fallback
        );
    }

    #[tokio::test]
    async fn a_reload_during_the_retries_renews_the_lease() {
        let t0 = Instant::now();
        let r = Registry::new(Limits {
            schema_check_retry: SchemaCheckRetry {
                interval: Duration::from_millis(20),
                times: 50,
            },
            ..limits()
        });
        r.install(schema(10), t0);
        r.registered(t0, Instant::now() + 90 * S);
        r.reloaded(tso(0.0), t0, 10, false);
        r.cycle_completed(t0);
        let a = r.admit(Instant::now()).unwrap();
        let renew = {
            let r = r.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                r.reloaded(tso(6.0), Instant::now(), 10, false);
            })
        };
        assert_eq!(
            a.guard.check_results(Some(tso(6.0))).await,
            ResultCheck::Valid
        );
        renew.await.unwrap();
    }

    #[test]
    fn table_ids_parse_like_go() {
        assert_eq!(MdlJob::parse_table_ids("112,114"), vec![112, 114]);
        assert_eq!(MdlJob::parse_table_ids("112"), vec![112]);
        assert_eq!(MdlJob::parse_table_ids(""), Vec::<i64>::new());
        assert_eq!(MdlJob::parse_table_ids("1,x,3"), vec![1, 3]);
    }
}
