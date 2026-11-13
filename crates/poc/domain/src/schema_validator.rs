//! The schema lease (Go: `pkg/domain/schema_validator.go`, `pkg/domain/schema_checker.go`).
//!
//! The validator remembers the TSO of the last successful schema reload. The loaded schema
//! stays usable for one schema lease (`--lease`, 45 s as in Go) after that TSO, so a slow or
//! failing reload (for example while the meta region elects a new leader, #136) does not
//! stop the node from serving statements on the cached schema. Like Go, the lease is measured
//! in TSO: a statement whose read TS is more than one lease after the reload's TS fails with
//! `ErrInfoSchemaExpired` (8027).
//!
//! go-file: pkg/domain/schema_validator.go
//! go-file: pkg/domain/schema_checker.go

use std::time::Duration;
use std::time::Instant;

use tracing::info;

/// Go: `oracle.GetTimeFromTS` in milliseconds (the physical part of a TSO).
// go-dep: client-go oracle.GetTimeFromTS L1
fn physical_ms(ts: u64) -> u64 {
    const PHYSICAL_SHIFT_BITS: u64 = 18;
    ts >> PHYSICAL_SHIFT_BITS
}

/// Result of [`SchemaValidator::check`].
// go: checkResult L1
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckResult {
    /// The check passes (Go: `ResultSucc`).
    Succ,
    /// The schema is too old (Go: `ResultFail`).
    Fail,
    /// The lease has expired at the statement's TS, or the validator is stopped (Go:
    /// `ResultUnknown`); the caller may wait for a reload.
    Unknown,
}

/// Errors of [`SchemaChecker`](check_by_schema_ver) (Go: `domain.ErrInfoSchemaChanged`,
/// `domain.ErrInfoSchemaExpired`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SchemaCheckError {
    #[error(
        "Information schema is changed during the execution of the statement(for example, table definition may be updated by other DDL ran in parallel). If you see this error often, try increasing `tidb_max_delta_schema_count`"
    )]
    Changed,
    #[error(
        "Information schema is out of date: schema failed to update in 1 lease, please make sure TiDB can connect to TiKV"
    )]
    Expired,
}

impl SchemaCheckError {
    /// MySQL error code (Go: `errno.ErrInfoSchemaChanged` / `errno.ErrInfoSchemaExpired`).
    pub fn code(self) -> u16 {
        match self {
            SchemaCheckError::Changed => 8028,
            SchemaCheckError::Expired => 8027,
        }
    }
}

/// When the latest schema stops being valid.
#[derive(Clone, Copy, Debug)]
struct Expire {
    /// Go's `latestSchemaExpire`: physical time (ms) of the reload's TSO plus the lease,
    /// minus 1 ms. Compared with a statement's read TS.
    ts_ms: u64,
    /// The same bound on the local monotonic clock, counted from when the reload *requested*
    /// its TSO. The TSO's physical time is not earlier than that, so this never expires later
    /// than `ts_ms`. Used where Go compares `time.Now()` (no TSO at hand: admission, MaxTS
    /// point gets).
    local: Instant,
}

/// Go's `schemaValidator`, without the delta queue: tidb-rs always runs the MDL protocol
/// (ADR 0005), and with MDL Go's `Check` never consults the deltas (`needCheckSchema` is
/// false), so a statement on a version older than the latest one passes.
#[derive(Clone, Debug)]
pub(crate) struct SchemaValidator {
    is_started: bool,
    lease: Duration,
    latest_schema_ver: i64,
    restart_schema_ver: i64,
    latest_schema_expire: Option<Expire>,
}

impl SchemaValidator {
    // go: NewSchemaValidator L2 (no delta queue and no `*Domain`; see the type's doc)
    pub(crate) fn new(lease: Duration) -> SchemaValidator {
        debug_assert!(!lease.is_zero(), "lease should be greater than 0");
        SchemaValidator {
            is_started: true,
            lease,
            latest_schema_ver: 0,
            restart_schema_ver: 0,
            latest_schema_expire: None,
        }
    }

    // go: schemaValidator.IsStarted L1
    pub(crate) fn is_started(&self) -> bool {
        self.is_started
    }

    // go: schemaValidator.Stop L1
    pub(crate) fn stop(&mut self) {
        if self.is_started {
            info!("the schema validator stops");
        }
        self.is_started = false;
        self.latest_schema_ver = 0;
    }

    /// `loaded_ver`: the version of the schema loaded now (Go reads
    /// `do.InfoSchema().SchemaMetaVersion()`).
    // go: schemaValidator.Restart L1
    pub(crate) fn restart(&mut self, loaded_ver: i64) {
        info!("the schema validator restarts");
        self.is_started = true;
        self.restart_schema_ver = loaded_ver;
    }

    // go: schemaValidator.Reset L1
    pub(crate) fn reset(&mut self) {
        self.is_started = true;
        self.latest_schema_ver = 0;
        self.restart_schema_ver = 0;
    }

    /// Forget the lease, so that only a reload that comes later renews it.
    // glue: ADR 0005 "Joining": after (re-)registering, statements may use the cached schema
    // only once a reload at a TSO taken after the registration has succeeded. Go's Restart
    // keeps the old expiry because Go's mustReload follows it at once.
    pub(crate) fn forget_lease(&mut self) {
        self.latest_schema_expire = None;
    }

    /// A reload succeeded at `lease_grant_ts` (requested on the local clock at
    /// `requested_at`); `curr_ver` is the version loaded now. Called even when the version
    /// did not change: that renews the lease.
    // go: schemaValidator.Update L2 (no delta queue; the local-clock bound is kept too)
    pub(crate) fn update(
        &mut self,
        lease_grant_ts: u64,
        requested_at: Instant,
        old_ver: i64,
        curr_ver: i64,
    ) {
        if !self.is_started {
            info!("the schema validator stopped before updating");
            return;
        }
        let _ = old_ver;
        // Renew the lease.
        self.latest_schema_ver = curr_ver;
        let lease = self.lease.saturating_sub(Duration::from_millis(1));
        self.latest_schema_expire = Some(Expire {
            ts_ms: physical_ms(lease_grant_ts).saturating_add(lease.as_millis() as u64),
            local: requested_at + lease,
        });
    }

    /// Whether the lease has expired at `now` (local clock).
    // go: schemaValidator.IsLeaseExpired L2 (monotonic clock from the reload's TSO request
    // instead of wall clock vs. the TSO's physical time; never later than Go's)
    pub(crate) fn is_lease_expired(&self, now: Instant) -> bool {
        self.latest_schema_expire.is_none_or(|e| now > e.local)
    }

    /// Whether a statement using `schema_ver` may use its reads at `txn_ts`.
    // go: schemaValidator.Check L2 (MDL is always on: `relatedPhysicalTableIDs` and
    // `needCheckSchema` are not needed)
    pub(crate) fn check(&self, txn_ts: u64, schema_ver: i64) -> CheckResult {
        self.check_with(schema_ver, |e| physical_ms(txn_ts) > e.ts_ms)
    }

    /// [`SchemaValidator::check`] for a read without a TS (a point get at MaxTS): the lease is
    /// compared with the local clock at `now`, as [`SchemaValidator::is_lease_expired`] does.
    // go-part: schemaValidator.Check
    pub(crate) fn check_now(&self, now: Instant, schema_ver: i64) -> CheckResult {
        self.check_with(schema_ver, |e| now > e.local)
    }

    // go-part: schemaValidator.Check
    fn check_with(&self, schema_ver: i64, expired: impl FnOnce(&Expire) -> bool) -> CheckResult {
        if !self.is_started {
            return CheckResult::Unknown;
        }
        if schema_ver < self.restart_schema_ver {
            info!(
                schema_ver,
                "the schema version is too old, TiDB and PD maybe unhealthy after the transaction started"
            );
            return CheckResult::Fail;
        }
        // Schema changed: with MDL the owner waited for this statement, so it passes.
        if schema_ver < self.latest_schema_ver {
            return CheckResult::Succ;
        }
        // Schema unchanged, maybe success or the schema validator is unavailable.
        match &self.latest_schema_expire {
            Some(e) if !expired(e) => CheckResult::Succ,
            _ => CheckResult::Unknown,
        }
    }
}

/// Go's `SchemaOutOfDateRetryInterval` / `SchemaOutOfDateRetryTimes`: how long
/// `CheckBySchemaVer` waits for a reload to renew an expired lease.
#[derive(Clone, Copy, Debug)]
pub struct SchemaCheckRetry {
    pub interval: Duration,
    pub times: u32,
}

impl Default for SchemaCheckRetry {
    fn default() -> Self {
        SchemaCheckRetry {
            interval: Duration::from_millis(500),
            times: 10,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEASE: Duration = Duration::from_secs(45);

    /// A TSO whose physical part is `ms`.
    fn ts(ms: u64) -> u64 {
        ms << 18 | 7
    }

    #[test]
    fn within_the_lease_succeeds_after_it_unknown() {
        let t0 = Instant::now();
        let mut v = SchemaValidator::new(LEASE);
        // Nothing loaded yet: no lease.
        assert_eq!(v.check(ts(1_000), 0), CheckResult::Unknown);
        assert!(v.is_lease_expired(t0));
        v.update(ts(1_000), t0, 0, 10);
        assert_eq!(v.check(ts(1_000), 10), CheckResult::Succ);
        assert_eq!(v.check(ts(1_000 + 44_999), 10), CheckResult::Succ);
        // Go: expire = grant + lease - 1 ms; a TS after it is unknown.
        assert_eq!(v.check(ts(1_000 + 45_000), 10), CheckResult::Unknown);
        assert!(!v.is_lease_expired(t0 + LEASE - Duration::from_millis(2)));
        assert!(v.is_lease_expired(t0 + LEASE));
        assert_eq!(
            v.check_now(t0 + Duration::from_secs(44), 10),
            CheckResult::Succ
        );
        assert_eq!(v.check_now(t0 + LEASE, 10), CheckResult::Unknown);
        // A reload renews the lease even if the version did not change.
        v.update(ts(30_000), t0 + Duration::from_secs(29), 10, 10);
        assert_eq!(v.check(ts(1_000 + 45_000), 10), CheckResult::Succ);
        assert!(!v.is_lease_expired(t0 + LEASE));
    }

    #[test]
    fn older_versions_pass_with_mdl_and_restart_fails_them() {
        let t0 = Instant::now();
        let mut v = SchemaValidator::new(LEASE);
        v.update(ts(1_000), t0, 0, 10);
        v.update(ts(2_000), t0, 10, 11);
        // A statement admitted on 10 while 11 was installed: the MDL protocol held the owner.
        assert_eq!(v.check(ts(2_000), 10), CheckResult::Succ);
        // Even past the lease (Go checks the lease only for the latest version).
        assert_eq!(v.check(ts(100_000), 10), CheckResult::Succ);
        // Stopped: unknown, and updates are ignored.
        v.stop();
        assert!(!v.is_started());
        assert_eq!(v.check(ts(2_000), 11), CheckResult::Unknown);
        v.update(ts(3_000), t0, 11, 12);
        assert_eq!(v.check(ts(2_000), 11), CheckResult::Unknown);
        // Restarted with 12 loaded: older versions fail.
        v.restart(12);
        v.update(ts(3_000), t0, 11, 12);
        assert_eq!(v.check(ts(3_000), 11), CheckResult::Fail);
        assert_eq!(v.check(ts(3_000), 12), CheckResult::Succ);
        v.reset();
        assert_eq!(v.check(ts(3_000), 11), CheckResult::Succ);
    }

    #[test]
    fn forget_lease_needs_a_new_update() {
        let t0 = Instant::now();
        let mut v = SchemaValidator::new(LEASE);
        v.update(ts(1_000), t0, 0, 10);
        v.forget_lease();
        assert!(v.is_lease_expired(t0));
        assert_eq!(v.check(ts(1_000), 10), CheckResult::Unknown);
        v.update(ts(1_500), t0, 10, 10);
        assert_eq!(v.check(ts(1_500), 10), CheckResult::Succ);
    }

    #[test]
    fn errors_use_go_codes_and_messages() {
        assert_eq!(SchemaCheckError::Expired.code(), 8027);
        assert_eq!(SchemaCheckError::Changed.code(), 8028);
        assert!(
            SchemaCheckError::Expired
                .to_string()
                .starts_with("Information schema is out of date")
        );
    }
}
