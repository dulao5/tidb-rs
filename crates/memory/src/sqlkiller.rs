//! go-file: pkg/util/sqlkiller/sqlkiller.go
//!
//! `pkg/util/sqlkiller` (block D00.3, ported whole per constitution 4.4 into the lowest crate
//! that uses it: the memory tracker calls `Killer.HandleSignal` in `Consume`). N3 3.3 item 1:
//! the signal values, the CAS in `SendKillSignal`, the error mapping of `HandleSignal`, the
//! connection-alive check, `FinishResultSet` and `Reset` are L1.
//!
//! Not here yet: the wake-up channel of N3 3.3 item 2 (`M-GOCTX`, a `CancellationToken`
//! triggered by `SendKillSignal`). It belongs with `ExecCtx` (block D00.1) and is attached
//! through [`SqlKiller::set_on_kill`]. The `randomPanic` failpoint of `HandleSignal` is not
//! ported (no failpoints in tidb-rs yet).

// Signal constants keep their Go names (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::SeqCst;
use std::time::Duration;
use std::time::Instant;

use arc_swap::ArcSwapOption;
use parking_lot::Mutex;
use tidb_base::Error;
use tidb_base::dbterror::exeerrors;
use tidb_base::go_args;

/// Go `killSignal` (`uint32`).
pub type KillSignal = u32;

/// The zero value: no signal.
pub const UnspecifiedKillSignal: KillSignal = 0;
/// `KILL QUERY` / connection gone.
pub const QueryInterrupted: KillSignal = 1;
/// `max_execution_time`.
pub const MaxExecTimeExceeded: KillSignal = 2;
/// `tidb_mem_quota_query` with `tidb_mem_oom_action = CANCEL`.
pub const QueryMemoryExceeded: KillSignal = 3;
/// `tidb_server_memory_limit`.
pub const ServerMemoryExceeded: KillSignal = 4;
/// Runaway query.
pub const RunawayQueryExceeded: KillSignal = 5;

/// Go `SQLKiller.Finish` / `IsConnectionAlive` callbacks.
pub type FinishFn = Box<dyn Fn() + Send + Sync>;
/// Go `*func() bool` in `IsConnectionAlive`.
pub type AliveFn = Box<dyn Fn() -> bool + Send + Sync>;

/// Go `sqlkiller.SQLKiller`: kills a running statement. Shared (`Arc`) by the session, KILL,
/// the memory controller and the trackers.
#[derive(Default)]
pub struct SqlKiller {
    /// Go `Signal` (accessed with atomics).
    pub signal: AtomicU32,
    /// Go `ConnID`.
    pub conn_id: AtomicU64,
    /// Go `FinishFuncLock` + `Finish`: the lock guards the callback (L2: one `Mutex<Option<_>>`).
    finish: Mutex<Option<FinishFn>>,
    /// Go `InWriteResultSet`.
    pub in_write_result_set: AtomicBool,
    last_check_time: ArcSwapOption<Instant>,
    /// Go `IsConnectionAlive`.
    pub is_connection_alive: ArcSwapOption<AliveFn>,
    on_kill: ArcSwapOption<FinishFn>,
}

impl std::fmt::Debug for SqlKiller {
    // glue: Debug for a struct holding callbacks
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqlKiller")
            .field("signal", &self.signal.load(SeqCst))
            .field("conn_id", &self.conn_id.load(SeqCst))
            .finish_non_exhaustive()
    }
}

/// `intest.InTest`: Go sets it in every test binary (build tag `intest`); here it is this
/// crate's unit tests.
// glue: Go intest.InTest
const fn in_test() -> bool {
    cfg!(test)
}

impl SqlKiller {
    // glue: constructor (Go: zero value `sqlkiller.SQLKiller{}`)
    pub fn new() -> Arc<SqlKiller> {
        Arc::new(SqlKiller::default())
    }

    /// Runs `f` after a successful `SendKillSignal` (N3 3.3 item 2, `M-GOCTX`): `ExecCtx`
    /// cancels its token here so that waits on channels wake up.
    // glue: wake-up hook of N3 3.3 item 2
    pub fn set_on_kill(&self, f: Option<FinishFn>) {
        self.on_kill.store(f.map(Arc::new));
    }

    // go: SQLKiller.SendKillSignal L1
    pub fn send_kill_signal(&self, reason: KillSignal) {
        if self
            .signal
            .compare_exchange(0, reason, SeqCst, SeqCst)
            .is_ok()
        {
            let status = self.signal.load(SeqCst);
            let err = self.get_kill_error(status);
            let reason = err
                .map(|e| String::from_utf8_lossy(&e.error()).into_owned())
                .unwrap_or_else(|| "<nil>".to_owned());
            tracing::warn!(
                connection_id = self.conn_id.load(SeqCst),
                reason = %reason,
                "kill initiated"
            );
            if let Some(f) = self.on_kill.load().as_ref() {
                f();
            }
        }
    }

    // go: SQLKiller.GetKillSignal L1
    pub fn get_kill_signal(&self) -> KillSignal {
        self.signal.load(SeqCst)
    }

    // go: SQLKiller.getKillError L1
    fn get_kill_error(&self, status: KillSignal) -> Option<Error> {
        match status {
            QueryInterrupted => Some(exeerrors::ErrQueryInterrupted.gen_with_stack_by_args(vec![])),
            MaxExecTimeExceeded => {
                Some(exeerrors::ErrMaxExecTimeExceeded.gen_with_stack_by_args(vec![]))
            }
            QueryMemoryExceeded => Some(
                exeerrors::ErrMemoryExceedForQuery
                    .gen_with_stack_by_args(go_args![self.conn_id.load(SeqCst)]),
            ),
            ServerMemoryExceeded => Some(
                exeerrors::ErrMemoryExceedForInstance
                    .gen_with_stack_by_args(go_args![self.conn_id.load(SeqCst)]),
            ),
            RunawayQueryExceeded => Some(
                exeerrors::ErrResourceGroupQueryRunawayInterrupted
                    .fast_gen_by_args(go_args!["runaway exceed tidb side"]),
            ),
            _ => None,
        }
    }

    // go: SQLKiller.FinishResultSet L1
    pub fn finish_result_set(&self) {
        let g = self.finish.lock();
        if let Some(f) = g.as_ref() {
            f();
        }
    }

    // go: SQLKiller.SetFinishFunc L1
    pub fn set_finish_func(&self, f: FinishFn) {
        *self.finish.lock() = Some(f);
    }

    // go: SQLKiller.ClearFinishFunc L1
    pub fn clear_finish_func(&self) {
        *self.finish.lock() = None;
    }

    /// Go `HandleSignal`: the error of the current kill signal, `Ok` when there is none.
    // go: SQLKiller.HandleSignal L1
    pub fn handle_signal(&self) -> Result<(), Error> {
        // Checks if the connection is alive. For performance reasons, the check interval
        // should be at least `checkConnectionAliveDur` (1 second).
        if let Some(f) = self.is_connection_alive.load().as_ref() {
            let check_connection_alive_dur = if in_test() {
                Duration::from_millis(1)
            } else {
                Duration::from_secs(1)
            };
            let now = Instant::now();
            match self.last_check_time.load().as_ref() {
                None => self.last_check_time.store(Some(Arc::new(now))),
                Some(last) if now.duration_since(**last) > check_connection_alive_dur => {
                    self.last_check_time.store(Some(Arc::new(now)));
                    if !f() {
                        self.send_kill_signal(QueryInterrupted);
                    }
                }
                Some(_) => {}
            }
        }

        let status = self.signal.load(SeqCst);
        let err = self.get_kill_error(status);
        if status == ServerMemoryExceeded {
            tracing::warn!(
                conn = self.conn_id.load(SeqCst),
                "global memory controller, NeedKill signal is received successfully"
            );
        }
        match err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    // go: SQLKiller.CheckConnectionAlive L1
    pub fn check_connection_alive(&self) {
        if let Some(f) = self.is_connection_alive.load().as_ref()
            && !f()
        {
            self.send_kill_signal(QueryInterrupted);
        }
    }

    // go: SQLKiller.Reset L1
    pub fn reset(&self) {
        if self.signal.load(SeqCst) != 0 {
            tracing::warn!(conn = self.conn_id.load(SeqCst), "kill finished");
        }
        self.signal.store(0, SeqCst);
        self.last_check_time.store(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kill_signals_map_to_go_errors() {
        let k = SqlKiller::new();
        k.conn_id.store(7, SeqCst);
        assert!(k.handle_signal().is_ok());
        k.send_kill_signal(QueryMemoryExceeded);
        // A second signal does not replace the first (CAS from 0).
        k.send_kill_signal(QueryInterrupted);
        let e = k.handle_signal().unwrap_err();
        assert_eq!(e.code(), 8175);
        assert!(String::from_utf8_lossy(&e.error()).ends_with("[conn=7]"));
        k.reset();
        assert!(k.handle_signal().is_ok());
    }

    #[test]
    fn connection_alive_check() {
        let k = SqlKiller::new();
        k.is_connection_alive
            .store(Some(Arc::new(Box::new(|| false) as AliveFn)));
        // The first call only records the time.
        assert!(k.handle_signal().is_ok());
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(k.handle_signal().unwrap_err().code(), 1317);
    }

    #[test]
    fn finish_func() {
        let k = SqlKiller::new();
        let hit = Arc::new(AtomicBool::new(false));
        let h = hit.clone();
        k.set_finish_func(Box::new(move || h.store(true, SeqCst)));
        k.finish_result_set();
        assert!(hit.load(SeqCst));
        k.clear_finish_func();
        k.finish_result_set();
    }
}
