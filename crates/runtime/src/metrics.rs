//! Runtime metrics (N3 3.6 "观测").
//!
//! Go has no counterpart for most of these: they describe the Rust runtime (queues, poll
//! stalls, CPU slices), so they get their own prefix `tidb_rs_runtime_` and never reuse a Go
//! name. The client-go series that belongs here keeps its Go name:
//! `tidb_tikvclient_get_store_limit_token_error` (client-go `TiKVStoreLimitErrorCounter`). The
//! statement-admission series of Go `server.getToken` (`tidb_server_tokens`,
//! `tidb_server_get_token_duration_seconds`) stay in `tidb-metrics`: Go records them in the
//! server, not in the limiter.
//!
//! Everything is registered in the `prometheus` default registry, like `tidb-metrics`, so
//! `tidb_metrics::gather_text` exports it. What is recorded per poll, per spawn or per queue
//! permit goes through [`crate::local`] (thread-local histograms, sharded gauges) and through
//! handles resolved once: no label lookup and no contended atomic on those paths.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Duration;
use std::time::Instant;

use parking_lot::Mutex;
use prometheus::Histogram;
use prometheus::HistogramOpts;
use prometheus::HistogramVec;
use prometheus::IntCounter;
use prometheus::IntCounterVec;
use prometheus::Opts;
use prometheus::exponential_buckets;

use crate::local::FastHistogram;
use crate::local::ShardedGauge;
use crate::local::ShardedGaugeVec;

/// The queues of N3 3.6 (Q3 has one per category).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Queue {
    /// Q1, statement admission.
    Admission,
    /// Q2, the per-store in-flight window.
    StoreWindow,
    /// Q3, spill I/O.
    BlockingSpill,
    /// Q3, other blocking work.
    BlockingOther,
}

impl Queue {
    // glue: metric label
    pub fn as_str(self) -> &'static str {
        match self {
            Queue::Admission => "admission",
            Queue::StoreWindow => "store_window",
            Queue::BlockingSpill => "blocking_spill",
            Queue::BlockingOther => "blocking_other",
        }
    }
}

// glue: registration helper
fn register<C: prometheus::core::Collector + Clone + 'static>(c: C) -> C {
    // A second registration (another collector of the same name, or tests) is harmless: the
    // series still counts, it is just exported by the first one.
    let _ = prometheus::register(Box::new(c.clone()));
    c
}

// glue: bucket helper
fn buckets_1us() -> Vec<f64> {
    // 1 µs .. ~537 s, the range of Go's get_token_duration (which is in µs).
    exponential_buckets(1e-6, 2.0, 30).expect("valid buckets")
}

static QUEUE_WAITING: LazyLock<&'static ShardedGaugeVec> = LazyLock::new(|| {
    ShardedGaugeVec::register(
        "tidb_rs_runtime_queue_waiting",
        "Tasks waiting in a runtime queue (N3 Q1/Q2/Q3)",
        "queue",
    )
});

static QUEUE_RUNNING: LazyLock<&'static ShardedGaugeVec> = LazyLock::new(|| {
    ShardedGaugeVec::register(
        "tidb_rs_runtime_queue_running",
        "Permits held in a runtime queue (N3 Q1/Q2/Q3)",
        "queue",
    )
});

static QUEUE_WAIT: LazyLock<HistogramVec> = LazyLock::new(|| {
    register(
        HistogramVec::new(
            HistogramOpts::new(
                "queue_wait_seconds",
                "Time waited for a runtime queue permit",
            )
            .namespace("tidb_rs")
            .subsystem("runtime")
            .buckets(buckets_1us()),
            &["queue"],
        )
        .expect("valid metric"),
    )
});

static STORE_INFLIGHT: LazyLock<&'static ShardedGaugeVec> = LazyLock::new(|| {
    ShardedGaugeVec::register(
        "tidb_rs_runtime_store_inflight",
        "In-flight requests to a TiKV store (N3 Q2)",
        "store",
    )
});

static STORE_SCHEDULE_WAIT: LazyLock<HistogramVec> = LazyLock::new(|| {
    register(
        HistogramVec::new(
            HistogramOpts::new(
                "store_schedule_wait_seconds",
                "TiKV read-pool schedule wait reported in coprocessor responses (N3 Q2 signal)",
            )
            .namespace("tidb_rs")
            .subsystem("runtime")
            .buckets(buckets_1us()),
            &["store"],
        )
        .expect("valid metric"),
    )
});

static STORE_LIMIT_TOKEN_ERROR: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register(
        IntCounterVec::new(
            Opts::new(
                "get_store_limit_token_error",
                "store token is up to the limit, probably because one of the stores is the hotspot or unavailable",
            )
            .namespace("tidb")
            .subsystem("tikvclient"),
            &["address", "store"],
        )
        .expect("valid metric"),
    )
});

static POLL_STALLS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register(
        IntCounterVec::new(
            Opts::new(
                "poll_stalls_total",
                "Polls that ran longer than the stall threshold",
            )
            .namespace("tidb_rs")
            .subsystem("runtime"),
            &["task"],
        )
        .expect("valid metric"),
    )
});

static POLL_STALL_DURATION: LazyLock<Histogram> = LazyLock::new(|| {
    register(
        Histogram::with_opts(
            HistogramOpts::new("poll_stall_seconds", "Duration of stalled polls")
                .namespace("tidb_rs")
                .subsystem("runtime")
                .buckets(exponential_buckets(0.001, 2.0, 20).expect("valid buckets")),
        )
        .expect("valid metric"),
    )
});

static CPU_SLICE: LazyLock<FastHistogram> = LazyLock::new(|| {
    FastHistogram::new(register(
        Histogram::with_opts(
            HistogramOpts::new("cpu_slice_seconds", "Run time of one poll of a task")
                .namespace("tidb_rs")
                .subsystem("runtime")
                .buckets(buckets_1us()),
        )
        .expect("valid metric"),
    ))
});

static TASKS: LazyLock<&'static ShardedGaugeVec> = LazyLock::new(|| {
    ShardedGaugeVec::register(
        "tidb_rs_runtime_tasks",
        "Live tasks started through the ExecPool, by task name",
        "task",
    )
});

static PANICS: LazyLock<IntCounter> = LazyLock::new(|| {
    register(
        IntCounter::with_opts(
            Opts::new(
                "task_panics_total",
                "Tasks that panicked (recovered at the task boundary)",
            )
            .namespace("tidb_rs")
            .subsystem("runtime"),
        )
        .expect("valid metric"),
    )
});

/// Handles of one queue, resolved once.
pub(crate) struct QueueMetrics {
    pub waiting: &'static ShardedGauge,
    pub running: &'static ShardedGauge,
    pub wait: FastHistogram,
}

impl QueueMetrics {
    // glue: metric children
    pub(crate) fn new(q: Queue) -> QueueMetrics {
        QueueMetrics {
            waiting: QUEUE_WAITING.child(q.as_str()),
            running: QUEUE_RUNNING.child(q.as_str()),
            wait: FastHistogram::new(QUEUE_WAIT.with_label_values(&[q.as_str()])),
        }
    }

    /// A permit was obtained after waiting since `start`.
    // glue: queue metrics
    pub(crate) fn acquired(&self, start: Instant) {
        let now = Instant::now();
        self.wait
            .observe_at(now.saturating_duration_since(start).as_secs_f64(), now);
        self.running.inc();
    }
}

// glue: metric child (once per store)
pub(crate) fn store_inflight(store: u64) -> &'static ShardedGauge {
    STORE_INFLIGHT.child(&store.to_string())
}

// glue: metric child (once per store)
pub(crate) fn store_schedule_wait(store: u64) -> FastHistogram {
    FastHistogram::new(STORE_SCHEDULE_WAIT.with_label_values(&[&store.to_string()]))
}

// glue: client-go metrics.TiKVStoreLimitErrorCounter.WithLabelValues(addr, store).Inc()
pub(crate) fn store_limit_token_error(addr: &str, store: u64) {
    STORE_LIMIT_TOKEN_ERROR
        .with_label_values(&[addr, &store.to_string()])
        .inc();
}

/// Metric handles of one task name, resolved once per name per thread.
pub(crate) struct TaskMetrics {
    pub name: &'static str,
    pub live: &'static ShardedGauge,
}

impl TaskMetrics {
    /// Record a stalled poll (rare: resolved on demand).
    // glue: N3 3.6 stall metric
    pub(crate) fn stall(&self, d: Duration) {
        POLL_STALLS.with_label_values(&[self.name]).inc();
        POLL_STALL_DURATION.observe(d.as_secs_f64());
    }
}

static TASK_METRICS: LazyLock<Mutex<HashMap<&'static str, &'static TaskMetrics>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

thread_local! {
    static TASK_METRICS_CACHE: RefCell<HashMap<&'static str, &'static TaskMetrics>> =
        RefCell::new(HashMap::new());
}

/// Handles for task name `name`: a thread-local map hit on the hot path; the shared map
/// (and the label lookup) only the first time a thread sees the name.
// glue: metric handles per task name
pub(crate) fn task(name: &'static str) -> &'static TaskMetrics {
    let cached = TASK_METRICS_CACHE
        .try_with(|c| c.borrow().get(name).copied())
        .ok()
        .flatten();
    if let Some(m) = cached {
        return m;
    }
    let m = *TASK_METRICS.lock().entry(name).or_insert_with(|| {
        Box::leak(Box::new(TaskMetrics {
            name,
            live: TASKS.child(name),
        }))
    });
    let _ = TASK_METRICS_CACHE.try_with(|c| c.borrow_mut().insert(name, m));
    m
}

// glue: per-poll metric (thread-local histogram)
pub(crate) fn cpu_slice(d: Duration, now: Instant) {
    CPU_SLICE.observe_at(d.as_secs_f64(), now);
}

// glue: metric
pub(crate) fn panic() {
    PANICS.inc();
}

/// Live tasks named `name` (summed over shards).
// glue: accessor (tests, N3 §5 "task 不泄漏")
pub fn live_tasks(name: &'static str) -> i64 {
    task(name).live.get()
}

/// Force every runtime metric, so it is exported before first use (Go registers at init).
// glue: registration
pub fn init() {
    for q in [
        Queue::Admission,
        Queue::StoreWindow,
        Queue::BlockingSpill,
        Queue::BlockingOther,
    ] {
        let _ = QueueMetrics::new(q);
    }
    LazyLock::force(&STORE_INFLIGHT);
    LazyLock::force(&STORE_SCHEDULE_WAIT);
    LazyLock::force(&STORE_LIMIT_TOKEN_ERROR);
    LazyLock::force(&POLL_STALLS);
    LazyLock::force(&POLL_STALL_DURATION);
    LazyLock::force(&CPU_SLICE);
    LazyLock::force(&TASKS);
    LazyLock::force(&PANICS);
}
