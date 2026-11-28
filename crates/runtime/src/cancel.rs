//! Two-tier cooperative cancellation primitives for statements and operators (N3 §3.3, #542):
//!
//! - [`KillCancel`]: resettable session-level notification bound to `SqlKiller.signal`
//!   (`Arc<AtomicU32>`). `SqlKiller::send_kill_signal` calls [`KillCancel::notify`] and
//!   `SqlKiller::reset` calls [`KillCancel::on_reset`]; [`KillCancel::cancelled`] always
//!   re-checks `signal != 0` after any watch wakeup so a reset (e.g. `ResetContextOfStmt`,
//!   `Tracker::detach`, or `SLEEP`) never leaves a latched cancellation or causes a spurious
//!   wakeup.
//! - [`ChildToken`]: one-shot, non-resettable operator-internal token (Go's private `closeCh` /
//!   `finishCh`, e.g. `Limit` early completion or `FinishResultSet`).

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering::SeqCst;

use tokio::sync::watch;

/// Session-level, resettable cancellation token bound to `SqlKiller.signal`.
#[derive(Clone, Debug)]
pub struct KillCancel {
    signal: Arc<AtomicU32>,
    notify_tx: Arc<watch::Sender<u64>>,
}

impl Default for KillCancel {
    // glue: standalone KillCancel for contexts/tests without a session SqlKiller
    fn default() -> Self {
        Self::new(Arc::new(AtomicU32::new(0)))
    }
}

impl KillCancel {
    /// Creates a `KillCancel` bound to `signal` (`killer.signal_handle()`).
    // glue: N3 §3.3 item 2 (bind cancellation wakeup to SqlKiller.Signal)
    pub fn new(signal: Arc<AtomicU32>) -> Self {
        let (tx, _) = watch::channel(0u64);
        Self {
            signal,
            notify_tx: Arc::new(tx),
        }
    }

    /// Returns true when the underlying `SqlKiller` signal is non-zero.
    // glue: N3 §3.3 item 2 (signal == 0 is the single source of truth)
    #[inline]
    pub fn is_killed(&self) -> bool {
        self.signal.load(SeqCst) != 0
    }

    /// Returns the raw `SqlKiller` signal code (`0` when unset).
    // glue: N3 §3.3 item 2
    #[inline]
    pub fn signal(&self) -> u32 {
        self.signal.load(SeqCst)
    }

    /// Returns the shared signal handle.
    // glue: N3 §3.3 item 2
    pub fn signal_handle(&self) -> Arc<AtomicU32> {
        Arc::clone(&self.signal)
    }

    /// Wakes waiters after `SqlKiller::send_kill_signal` stores a non-zero signal.
    // glue: N3 §3.3 item 2 (on_kill hook)
    pub fn notify(&self) {
        self.notify_tx
            .send_modify(|epoch| *epoch = epoch.wrapping_add(1));
    }

    /// Advances the watch generation after `SqlKiller::reset` swaps `signal` back to `0`.
    /// Waiters re-check `signal != 0`, so they remain suspended until the next kill.
    // glue: N3 §3.3 item 2 (on_reset hook)
    pub fn on_reset(&self) {
        self.notify_tx
            .send_modify(|epoch| *epoch = epoch.wrapping_add(1));
    }

    /// Suspends until the underlying `SqlKiller` signal is non-zero.
    ///
    /// Re-verifies `is_killed()` after every watch wakeup so a concurrent or intervening
    /// `SqlKiller::reset()` (`SLEEP`, `Tracker::detach`, statement boundary reset) never
    /// triggers a spurious cancellation.
    // glue: N3 §3.3 item 2 (race-safe wait on resettable KillCancel)
    pub async fn cancelled(&self) {
        if self.is_killed() {
            return;
        }
        let mut rx = self.notify_tx.subscribe();
        loop {
            if rx.wait_for(|_| self.is_killed()).await.is_ok() {
                if self.is_killed() {
                    return;
                }
            } else {
                // Unreachable while `self` holds `notify_tx`, kept for totality.
                std::future::pending::<()>().await;
            }
        }
    }
}

/// Operator-internal, one-shot non-resettable cancellation token (Go's `closeCh` / `finishCh`).
#[derive(Clone, Debug)]
pub struct ChildToken {
    cancelled: Arc<AtomicBool>,
    tx: Arc<watch::Sender<bool>>,
}

impl Default for ChildToken {
    // glue: N3 §3.3 item 4 (one-shot ChildToken)
    fn default() -> Self {
        Self::new()
    }
}

impl ChildToken {
    /// Creates an uncancelled one-shot `ChildToken`.
    // glue: N3 §3.3 item 4
    pub fn new() -> Self {
        let (tx, _) = watch::channel(false);
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            tx: Arc::new(tx),
        }
    }

    /// Returns true once [`ChildToken::cancel`] has been called.
    // glue: N3 §3.3 item 4
    #[inline]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(SeqCst)
    }

    /// Marks the token cancelled permanently and wakes all waiters.
    // glue: N3 §3.3 item 4
    pub fn cancel(&self) {
        if !self.cancelled.swap(true, SeqCst) {
            let _ = self.tx.send(true);
        }
    }

    /// Suspends until [`ChildToken::cancel`] is called.
    // glue: N3 §3.3 item 4
    pub async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        let mut rx = self.tx.subscribe();
        let _ = rx.wait_for(|_| self.is_cancelled()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn kill_cancel_reset_does_not_spuriously_wake() {
        let sig = Arc::new(AtomicU32::new(0));
        let cancel = KillCancel::new(Arc::clone(&sig));

        // 1. Spurious on_reset while signal == 0 never completes cancelled().
        let c1 = cancel.clone();
        let h1 = tokio::spawn(async move {
            c1.cancelled().await;
        });
        tokio::task::yield_now().await;
        cancel.on_reset();
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(
            !h1.is_finished(),
            "on_reset must not trigger spurious cancellation"
        );

        // 2. Kill signal set -> cancelled() completes immediately; new waiters also return.
        sig.store(1, SeqCst);
        cancel.notify();
        h1.await.unwrap();
        assert!(cancel.is_killed());

        // Already-killed fast path.
        tokio::time::timeout(Duration::from_millis(50), cancel.cancelled())
            .await
            .expect("already killed should resolve at once");

        // 3. Reset signal -> 0 -> new waiter stays suspended across on_reset().
        sig.store(0, SeqCst);
        cancel.on_reset();
        assert!(!cancel.is_killed());

        let c2 = cancel.clone();
        let h2 = tokio::spawn(async move {
            c2.cancelled().await;
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(
            !h2.is_finished(),
            "reset KillCancel must wait for next kill"
        );

        // 4. Subsequent kill wakes waiter again.
        sig.store(2, SeqCst);
        cancel.notify();
        tokio::time::timeout(Duration::from_millis(50), h2)
            .await
            .expect("second kill wakes waiter")
            .unwrap();
    }

    #[tokio::test]
    async fn kill_cancel_concurrent_notify_and_reset_before_poll() {
        let sig = Arc::new(AtomicU32::new(0));
        let cancel = KillCancel::new(Arc::clone(&sig));
        let c = cancel.clone();
        let h = tokio::spawn(async move {
            c.cancelled().await;
        });
        tokio::task::yield_now().await;

        // Notify and immediately reset before the spawned task gets polled (simulates SLEEP() resetting).
        sig.store(1, SeqCst);
        cancel.notify();
        sig.store(0, SeqCst);
        cancel.on_reset();

        tokio::time::sleep(Duration::from_millis(15)).await;
        assert!(
            !h.is_finished(),
            "waiter re-checks atomic signal != 0 and stays suspended after reset"
        );

        sig.store(1, SeqCst);
        cancel.notify();
        tokio::time::timeout(Duration::from_millis(50), h)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn child_token_is_one_shot() {
        let tok = ChildToken::new();
        assert!(!tok.is_cancelled());
        let t2 = tok.clone();
        let h = tokio::spawn(async move {
            t2.cancelled().await;
        });
        tokio::task::yield_now().await;
        assert!(!h.is_finished());

        tok.cancel();
        assert!(tok.is_cancelled());
        tokio::time::timeout(Duration::from_millis(50), h)
            .await
            .unwrap()
            .unwrap();
        // Subsequent waits return immediately.
        tokio::time::timeout(Duration::from_millis(50), tok.cancelled())
            .await
            .unwrap();
    }
}
