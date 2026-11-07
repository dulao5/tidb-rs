//! go-file: pkg/errno/infoschema.go
//!
//! The error summary is protected by a mutex for simplicity.
//! It is not expected to be hot unless there are concurrent workloads
//! that are generating high error/warning counts, in which case
//! the system probably has other issues already.

// Go identifiers for constants and statics (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use std::collections::HashMap;
use std::time::SystemTime;

use parking_lot::Mutex;

/// ErrorSummary summarizes errors and warnings
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ErrorSummary {
    pub error_count: isize,
    pub warning_count: isize,
    pub first_seen: SystemTime,
    /// `None` = Go's zero `time.Time` (never incremented).
    pub last_seen: Option<SystemTime>,
}

/// instanceStatistics provide statistics for a tidb-server instance.
struct InstanceStatistics {
    global: HashMap<u16, ErrorSummary>,
    users: HashMap<String, HashMap<u16, ErrorSummary>>,
    hosts: HashMap<String, HashMap<u16, ErrorSummary>>,
}

static stats: Mutex<Option<InstanceStatistics>> = Mutex::new(None);

/// `init`: `FlushStats()`. Rust statics cannot run code at start-up; the first access
/// initialises (see `with_stats`).
// go: init L2 (lazy initialisation on first access)
fn init() -> InstanceStatistics {
    InstanceStatistics {
        global: HashMap::new(),
        users: HashMap::new(),
        hosts: HashMap::new(),
    }
}

// glue: lock + lazy init of the package variable `stats`
fn with_stats<R>(f: impl FnOnce(&mut InstanceStatistics) -> R) -> R {
    let mut g = stats.lock();
    f(g.get_or_insert_with(init))
}

/// FlushStats resets errors and warnings across global/users/hosts
// go: FlushStats L1 [M-LOCK, M-DEFER]
pub fn flush_stats() {
    with_stats(|s| {
        s.global = HashMap::new();
        s.users = HashMap::new();
        s.hosts = HashMap::new();
    });
}

// go: copyMap L1
fn copy_map(old_map: &HashMap<u16, ErrorSummary>) -> HashMap<u16, ErrorSummary> {
    let mut new_map = HashMap::with_capacity(old_map.len());
    for (k, v) in old_map {
        new_map.insert(
            *k,
            ErrorSummary {
                error_count: v.error_count,
                warning_count: v.warning_count,
                first_seen: v.first_seen,
                last_seen: v.last_seen,
            },
        );
    }
    new_map
}

/// GlobalStats summarizes errors and warnings across all users/hosts
// go: GlobalStats L1 [M-LOCK, M-DEFER]
pub fn global_stats() -> HashMap<u16, ErrorSummary> {
    with_stats(|s| copy_map(&s.global))
}

/// UserStats summarizes per-user
// go: UserStats L1 [M-LOCK, M-DEFER]
pub fn user_stats() -> HashMap<String, HashMap<u16, ErrorSummary>> {
    with_stats(|s| {
        let mut new_map = HashMap::with_capacity(s.users.len());
        for (k, v) in &s.users {
            new_map.insert(k.clone(), copy_map(v));
        }
        new_map
    })
}

/// HostStats summarizes per remote-host
// go: HostStats L1 [M-LOCK, M-DEFER]
pub fn host_stats() -> HashMap<String, HashMap<u16, ErrorSummary>> {
    with_stats(|s| {
        let mut new_map = HashMap::with_capacity(s.hosts.len());
        for (k, v) in &s.hosts {
            new_map.insert(k.clone(), copy_map(v));
        }
        new_map
    })
}

// go: initCounters L1 [M-LOCK, M-DEFER]
fn init_counters(err_code: u16, user: &str, host: &str) {
    let seen = SystemTime::now();
    with_stats(|s| {
        let fresh = || ErrorSummary {
            error_count: 0,
            warning_count: 0,
            first_seen: seen,
            last_seen: None,
        };
        s.global.entry(err_code).or_insert_with(fresh);
        s.users
            .entry(user.to_owned())
            .or_default()
            .entry(err_code)
            .or_insert_with(fresh);
        s.hosts
            .entry(host.to_owned())
            .or_default()
            .entry(err_code)
            .or_insert_with(fresh);
    });
}

/// IncrementError increments the global/user/host statistics for an errCode
// go: IncrementError L1 [M-LOCK, M-DEFER]
pub fn increment_error(err_code: u16, user: &str, host: &str) {
    let seen = SystemTime::now();
    init_counters(err_code, user, host);
    with_stats(|s| {
        if let Some(e) = s.global.get_mut(&err_code) {
            // Increment counter + update last seen
            e.error_count += 1;
            e.last_seen = Some(seen);
        }
        if let Some(e) = s.users.get_mut(user).and_then(|m| m.get_mut(&err_code)) {
            // Increment counter + update last seen
            e.error_count += 1;
            e.last_seen = Some(seen);
        }
        if let Some(e) = s.hosts.get_mut(host).and_then(|m| m.get_mut(&err_code)) {
            // Increment counter + update last seen
            e.error_count += 1;
            e.last_seen = Some(seen);
        }
    });
}

/// IncrementWarning increments the global/user/host statistics for an errCode
// go: IncrementWarning L1 [M-LOCK, M-DEFER]
pub fn increment_warning(err_code: u16, user: &str, host: &str) {
    let seen = SystemTime::now();
    init_counters(err_code, user, host);
    with_stats(|s| {
        if let Some(e) = s.global.get_mut(&err_code) {
            // Increment counter + update last seen
            e.warning_count += 1;
            e.last_seen = Some(seen);
        }
        if let Some(e) = s.users.get_mut(user).and_then(|m| m.get_mut(&err_code)) {
            // Increment counter + update last seen
            e.warning_count += 1;
            e.last_seen = Some(seen);
        }
        if let Some(e) = s.hosts.get_mut(host).and_then(|m| m.get_mut(&err_code)) {
            // Increment counter + update last seen
            e.warning_count += 1;
            e.last_seen = Some(seen);
        }
    });
}
