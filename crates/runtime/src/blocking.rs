//! Q3, the blocking pool (N3 3.6; mapping rule M-BLOCK; background tasks N3 3.7; D43).
//!
//! Executor and background code must not block a runtime thread. Genuinely blocking work goes
//! here: spill I/O, and other blocking calls (file I/O of background tasks, long loops that
//! cannot yield). Go simply runs them on a goroutine (the Go runtime parks the thread); here
//! they run on tokio's blocking threads, and the pool is **bounded per category**: each
//! [`BlockingKind`] has its own limit and queue, so one category cannot starve another (a
//! flood of one kind of work must not hold up spills, which are what bring memory back
//! down). Depth and wait time are exported per category.
//!
//! `SLEEP` and `GET_LOCK` are not blocking work here (D43): `SLEEP` waits on an async timer
//! and checks KILL as Go's sleep loop does, `GET_LOCK` is async; neither takes a Q3 slot.

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio::task::JoinError;

use crate::admission::Waiting;
use crate::local::ShardedGauge;
use crate::metrics::Queue;
use crate::metrics::QueueMetrics;

/// Category of blocking work; each has its own limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockingKind {
    /// Spill to disk and reading spilled data back (A1's spill action).
    Spill,
    /// Any other genuinely blocking call.
    Other,
}

/// Per-category limits. Default: 256 each, 512 together (tokio's default
/// `max_blocking_threads` per runtime).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockingConfig {
    pub spill: usize,
    pub other: usize,
}

impl Default for BlockingConfig {
    // glue: N3 3.6 Q3 defaults
    fn default() -> BlockingConfig {
        BlockingConfig {
            spill: 256,
            other: 256,
        }
    }
}

/// A held slot: the permit and the `running` gauge, released together.
struct Slot {
    running: &'static ShardedGauge,
    _permit: OwnedSemaphorePermit,
}

impl Drop for Slot {
    // glue: queue metric guard
    fn drop(&mut self) {
        self.running.dec();
    }
}

struct Category {
    sem: Arc<Semaphore>,
    metrics: QueueMetrics,
}

impl Category {
    // glue: N3 3.6 Q3
    fn new(limit: usize, q: Queue) -> Category {
        Category {
            sem: Arc::new(Semaphore::new(limit.max(1))),
            metrics: QueueMetrics::new(q),
        }
    }
}

/// Q3: the server's blocking pool. Cheap to clone.
#[derive(Clone)]
pub struct BlockingPool {
    spill: Arc<Category>,
    other: Arc<Category>,
}

impl Default for BlockingPool {
    // glue: N3 3.6 Q3 default
    fn default() -> BlockingPool {
        BlockingPool::new(BlockingConfig::default())
    }
}

impl BlockingPool {
    // glue: N3 3.6 Q3
    pub fn new(cfg: BlockingConfig) -> BlockingPool {
        BlockingPool {
            spill: Arc::new(Category::new(cfg.spill, Queue::BlockingSpill)),
            other: Arc::new(Category::new(cfg.other, Queue::BlockingOther)),
        }
    }

    // glue: category lookup
    fn category(&self, kind: BlockingKind) -> &Category {
        match kind {
            BlockingKind::Spill => &self.spill,
            BlockingKind::Other => &self.other,
        }
    }

    /// Run blocking `f` on a blocking thread, after waiting for a slot of `kind`. The slot is
    /// held until `f` returns, even if the caller stops waiting (a started closure cannot be
    /// cancelled). A panic in `f` comes back as the `JoinError` and frees the slot.
    // glue: N3 3.6 Q3 (M-BLOCK)
    pub async fn spawn_blocking<F, R>(&self, kind: BlockingKind, f: F) -> Result<R, JoinError>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let c = self.category(kind);
        let start = Instant::now();
        let waiting = Waiting::new(c.metrics.waiting);
        let permit = c
            .sem
            .clone()
            .acquire_owned()
            .await
            .expect("the blocking-pool semaphore is never closed");
        drop(waiting);
        c.metrics.acquired(start);
        let slot = Slot {
            running: c.metrics.running,
            _permit: permit,
        };
        tokio::task::spawn_blocking(move || {
            // Released when `f` returns or unwinds.
            let _slot = slot;
            f()
        })
        .await
    }

    /// Free slots of `kind`.
    // glue: accessor
    pub fn available(&self, kind: BlockingKind) -> usize {
        self.category(kind).sem.available_permits()
    }
}
