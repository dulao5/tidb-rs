//! The route trace (`--route-trace`, off by default): which engine ran each connection's last
//! routed statement, for the coverage census of the test harness (`tools/ab`, #260).
//!
//! `tidbrs_route_total` counts routes per engine and reason for the whole server; the census
//! needs them per statement. With the trace on, every routing decision of a connection
//! ([`crate::router::Router`], or the PoC itself with `--new-stack=off`) bumps that
//! connection's sequence number and records the engine and reason; the status port serves it
//! as `GET /debug/route?conn=<id>` (`<seq>\t<engine>\t<reason>`). The client reads it after
//! each statement on its own connection, so other connections cannot be mistaken for it, and
//! a statement that was never routed (a parse error, `COM_STMT_PREPARE`) leaves the sequence
//! number unchanged. Off, recording is one relaxed atomic load.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

static ENABLED: AtomicBool = AtomicBool::new(false);

static LAST: LazyLock<Mutex<HashMap<u64, Last>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// A connection's last routed statement.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Last {
    /// Routed statements of the connection so far (1 for the first).
    pub seq: u64,
    /// The `engine` label of `tidbrs_route_total`: `new`, `poc` or `none` (1235, ran nowhere).
    pub engine: &'static str,
    /// Its `reason` label (`ok`, `error`, `kind`, `ast`, `session`, a block name, ...;
    /// `new_stack_off` with `--new-stack=off`).
    pub reason: String,
}

/// Turns the trace on (`--route-trace` / `TIDB_RS_ROUTE_TRACE=1`).
pub fn enable() {
    ENABLED.store(true, Ordering::Relaxed);
}

/// Whether the trace is on.
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Records a routing decision of connection `conn`.
pub fn record(conn: u64, engine: &'static str, reason: &str) {
    if !enabled() {
        return;
    }
    let mut m = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let e = m.entry(conn).or_default();
    e.seq += 1;
    e.engine = engine;
    reason.clone_into(&mut e.reason);
}

/// Connection `conn`'s last routed statement (`seq` 0: none yet).
pub fn get(conn: u64) -> Last {
    LAST.lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&conn)
        .cloned()
        .unwrap_or_default()
}

/// The connection closed.
pub fn forget(conn: u64) {
    if enabled() {
        LAST.lock().unwrap_or_else(|e| e.into_inner()).remove(&conn);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_per_connection() {
        enable();
        record(9001, "new", "ok");
        record(9001, "poc", "D05.3");
        record(9002, "poc", "kind");
        assert_eq!(
            get(9001),
            Last {
                seq: 2,
                engine: "poc",
                reason: "D05.3".into()
            }
        );
        assert_eq!(get(9002).seq, 1);
        forget(9001);
        assert_eq!(get(9001), Last::default());
    }
}
