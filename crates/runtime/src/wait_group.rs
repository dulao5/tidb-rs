//! Asynchronous counting wait group (`sync.WaitGroup` for tasks, mapping rule M-WG).
//!
//! Waiters register with [`tokio::sync::Notify`] before checking the counter so a drop to
//! zero between the check and the await cannot lose a wakeup; when the counter is already
//! zero, [`WaitGroup::wait`] returns immediately without registering.

use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::Notify;

/// Go's `sync.WaitGroup` for async tasks (mapping rule M-WG): a counter and waiters woken
/// when it drops to zero.
///
/// Unlike Go's `sync.WaitGroup`, `Close` still awaits task `JoinHandle`s; this type handles
/// cross-task synchronization (such as final workers waiting for partial workers, or a
/// fetcher waiting for in-flight chunks to finish before spilling).
#[derive(Debug, Default)]
pub struct WaitGroup {
    count: Mutex<i64>,
    zero: Notify,
}

impl WaitGroup {
    /// Creates a new `WaitGroup` with counter 0.
    // glue: Go zero value sync.WaitGroup{} (M-WG)
    pub fn new() -> Self {
        Self::default()
    }

    /// `Add`: adds `delta` to the counter; a negative counter panics, as Go's does.
    // glue: sync.WaitGroup.Add (M-WG)
    pub fn add(&self, delta: i64) {
        let mut count = self.count.lock();
        *count += delta;
        if *count < 0 {
            panic!("sync: negative WaitGroup counter");
        }
        if *count == 0 {
            self.zero.notify_waiters();
        }
    }

    /// `Done`: decrements the counter by one.
    // glue: sync.WaitGroup.Done (M-WG)
    pub fn done(&self) {
        self.add(-1);
    }

    /// `Wait`: returns immediately when the counter is already zero, otherwise waits until it
    /// drops to zero.
    // glue: sync.WaitGroup.Wait (M-WG)
    pub async fn wait(&self) {
        if *self.count.lock() == 0 {
            return;
        }
        loop {
            let notified = self.zero.notified();
            tokio::pin!(notified);
            // Registered before the counter is read, so a drop to zero after the read wakes it.
            notified.as_mut().enable();
            if *self.count.lock() == 0 {
                return;
            }
            notified.await;
        }
    }

    /// The current counter value, for tests.
    // glue: test accessor
    pub fn count(&self) -> i64 {
        *self.count.lock()
    }
}

/// Calls [`WaitGroup::done`] when dropped: Go's `defer wg.Done()`, which runs on normal
/// exit, error, panic, and task cancellation.
pub struct DoneGuard(pub Arc<WaitGroup>);

impl Drop for DoneGuard {
    // glue: defer wg.Done() (M-WG)
    fn drop(&mut self) {
        self.0.done();
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::task::{Context, Poll, Waker};

    use super::{DoneGuard, WaitGroup};

    #[tokio::test]
    async fn wait_returns_immediately_when_zero() {
        let wg = WaitGroup::new();
        let mut fut = Box::pin(wg.wait());
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        assert!(matches!(
            Pin::as_mut(&mut fut).poll(&mut cx),
            Poll::Ready(())
        ));
    }

    #[tokio::test]
    async fn wait_wakes_all_waiters_when_counter_reaches_zero() {
        let wg = Arc::new(WaitGroup::new());
        wg.add(2);

        let w1 = Arc::clone(&wg);
        let t1 = tokio::spawn(async move { w1.wait().await });
        let w2 = Arc::clone(&wg);
        let t2 = tokio::spawn(async move { w2.wait().await });

        tokio::task::yield_now().await;
        wg.done();
        assert_eq!(wg.count(), 1);
        wg.done();
        assert_eq!(wg.count(), 0);

        t1.await.unwrap();
        t2.await.unwrap();
    }

    #[tokio::test]
    async fn drop_mid_wait_does_not_lose_wakeup_for_other_waiters() {
        let wg = Arc::new(WaitGroup::new());
        wg.add(1);

        // Start one waiter, poll it once so it registers on the Notify list, then drop it
        // before the counter reaches zero.
        {
            let mut dropped_wait = Box::pin(wg.wait());
            let waker = Waker::noop();
            let mut cx = Context::from_waker(waker);
            assert!(matches!(
                Pin::as_mut(&mut dropped_wait).poll(&mut cx),
                Poll::Pending
            ));
        }

        // Another waiter started concurrently still wakes when the counter drops to zero,
        // and the WaitGroup can be reused for a subsequent round.
        let wg_waiter = Arc::clone(&wg);
        let handle = tokio::spawn(async move { wg_waiter.wait().await });
        tokio::task::yield_now().await;
        wg.done();
        handle.await.unwrap();

        // Subsequent round after the mid-wait drop.
        wg.add(1);
        let wg_next = Arc::clone(&wg);
        let h2 = tokio::spawn(async move { wg_next.wait().await });
        tokio::task::yield_now().await;
        wg.done();
        h2.await.unwrap();
    }

    #[test]
    #[should_panic(expected = "sync: negative WaitGroup counter")]
    fn negative_counter_panics() {
        let wg = WaitGroup::new();
        wg.done();
    }

    #[test]
    fn done_guard_runs_on_drop_and_panic() {
        let wg = Arc::new(WaitGroup::new());
        wg.add(2);
        {
            let _g = DoneGuard(Arc::clone(&wg));
        }
        assert_eq!(wg.count(), 1);
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = DoneGuard(Arc::clone(&wg));
            panic!("boom");
        }));
        assert!(res.is_err());
        assert_eq!(wg.count(), 0);
    }
}
