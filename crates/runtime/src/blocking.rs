//! Q3, the blocking pool (N3 3.6; mapping rule M-BLOCK; background tasks N3 3.7).
//!
//! Executor and background code must not block a runtime thread. Spill I/O, `SLEEP` /
//! `GET_LOCK` (N4 D20) and long background loops that cannot yield go here. Go simply runs
//! them on a goroutine (the Go runtime parks the thread); here they run on tokio's blocking
//! threads, and the pool is **bounded**: at most `max_concurrent` closures run at once, the
//! rest queue (N3 3.6: "有界，满了排队"), with the queue's depth and wait time exported.

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Semaphore;
use tokio::task::JoinError;

use crate::admission::Waiting;
use crate::metrics::Queue;
use crate::metrics::QueueMetrics;

/// A held slot: the permit and the `running` gauge, released together.
struct Slot {
    running: prometheus::IntGauge,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl Drop for Slot {
    // glue: queue metric guard
    fn drop(&mut self) {
        self.running.dec();
    }
}

/// Default bound: tokio's default `max_blocking_threads` (512).
pub const DEFAULT_MAX_CONCURRENT: usize = 512;

/// Q3: the server's blocking pool. Cheap to clone.
#[derive(Clone)]
pub struct BlockingPool {
    sem: Arc<Semaphore>,
    metrics: Arc<QueueMetrics>,
}

impl Default for BlockingPool {
    // glue: N3 3.6 Q3 default
    fn default() -> BlockingPool {
        BlockingPool::new(DEFAULT_MAX_CONCURRENT)
    }
}

impl BlockingPool {
    // glue: N3 3.6 Q3
    pub fn new(max_concurrent: usize) -> BlockingPool {
        BlockingPool {
            sem: Arc::new(Semaphore::new(max_concurrent.max(1))),
            metrics: Arc::new(QueueMetrics::new(Queue::Blocking)),
        }
    }

    /// Run blocking `f` on a blocking thread, after waiting for a slot. The slot is held until
    /// `f` returns, even if the caller stops waiting (a started closure cannot be cancelled).
    /// A panic in `f` comes back as the `JoinError`.
    // glue: N3 3.6 Q3 (M-BLOCK)
    pub async fn spawn_blocking<F, R>(&self, f: F) -> Result<R, JoinError>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let start = Instant::now();
        let waiting = Waiting::new(&self.metrics.waiting);
        let permit = self
            .sem
            .clone()
            .acquire_owned()
            .await
            .expect("the blocking-pool semaphore is never closed");
        drop(waiting);
        self.metrics.wait.observe(start.elapsed().as_secs_f64());
        let running = self.metrics.running.clone();
        running.inc();
        let slot = Slot {
            running,
            _permit: permit,
        };
        tokio::task::spawn_blocking(move || {
            // Released when `f` returns or unwinds.
            let _slot = slot;
            f()
        })
        .await
    }

    /// Free slots.
    // glue: accessor
    pub fn available(&self) -> usize {
        self.sem.available_permits()
    }
}
