//! Runtime metrics (N3 3.6 "观测").
//!
//! Go has no counterpart for most of these: they describe the Rust runtime (queues, poll
//! stalls, CPU slices), so they get their own prefix `tidb_rs_runtime_` and never reuse a Go
//! name. The two Go/client-go series that belong here are kept under their Go names:
//! `tidb_tikvclient_get_store_limit_token_error` (client-go `TiKVStoreLimitErrorCounter`). The
//! statement-admission series of Go `server.getToken` (`tidb_server_tokens`,
//! `tidb_server_get_token_duration_seconds`) stay in `tidb-metrics`: Go records them in the
//! server, not in the limiter.
//!
//! Everything is registered in the `prometheus` default registry, like `tidb-metrics`, so
//! `tidb_metrics::gather_text` exports it. Hot paths use pre-resolved children.

use std::sync::LazyLock;
use std::time::Duration;

use prometheus::Histogram;
use prometheus::HistogramOpts;
use prometheus::HistogramVec;
use prometheus::IntCounter;
use prometheus::IntCounterVec;
use prometheus::IntGauge;
use prometheus::IntGaugeVec;
use prometheus::Opts;
use prometheus::exponential_buckets;

/// The three queues of N3 3.6.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Queue {
    /// Q1, statement admission.
    Admission,
    /// Q2, the per-store in-flight window.
    StoreWindow,
    /// Q3, the blocking pool.
    Blocking,
}

impl Queue {
    // glue: metric label
    pub fn as_str(self) -> &'static str {
        match self {
            Queue::Admission => "admission",
            Queue::StoreWindow => "store_window",
            Queue::Blocking => "blocking",
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

static QUEUE_WAITING: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    register(
        IntGaugeVec::new(
            Opts::new(
                "queue_waiting",
                "Tasks waiting in a runtime queue (N3 Q1/Q2/Q3)",
            )
            .namespace("tidb_rs")
            .subsystem("runtime"),
            &["queue"],
        )
        .expect("valid metric"),
    )
});

static QUEUE_RUNNING: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    register(
        IntGaugeVec::new(
            Opts::new(
                "queue_running",
                "Permits held in a runtime queue (N3 Q1/Q2/Q3)",
            )
            .namespace("tidb_rs")
            .subsystem("runtime"),
            &["queue"],
        )
        .expect("valid metric"),
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

static STORE_INFLIGHT: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    register(
        IntGaugeVec::new(
            Opts::new(
                "store_inflight",
                "In-flight requests to a TiKV store (N3 Q2)",
            )
            .namespace("tidb_rs")
            .subsystem("runtime"),
            &["store"],
        )
        .expect("valid metric"),
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

static CPU_SLICE: LazyLock<Histogram> = LazyLock::new(|| {
    register(
        Histogram::with_opts(
            HistogramOpts::new(
                "cpu_slice_seconds",
                "Run time of a task between two yields at a checkpoint (time slice)",
            )
            .namespace("tidb_rs")
            .subsystem("runtime")
            .buckets(buckets_1us()),
        )
        .expect("valid metric"),
    )
});

static TASKS: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    register(
        IntGaugeVec::new(
            Opts::new(
                "tasks",
                "Live tasks started through the ExecPool, by task name",
            )
            .namespace("tidb_rs")
            .subsystem("runtime"),
            &["task"],
        )
        .expect("valid metric"),
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

/// Pre-resolved children of one queue.
pub(crate) struct QueueMetrics {
    pub waiting: IntGauge,
    pub running: IntGauge,
    pub wait: Histogram,
}

impl QueueMetrics {
    // glue: metric children
    pub(crate) fn new(q: Queue) -> QueueMetrics {
        QueueMetrics {
            waiting: QUEUE_WAITING.with_label_values(&[q.as_str()]),
            running: QUEUE_RUNNING.with_label_values(&[q.as_str()]),
            wait: QUEUE_WAIT.with_label_values(&[q.as_str()]),
        }
    }
}

// glue: metric child
pub(crate) fn store_inflight(store: u64) -> IntGauge {
    STORE_INFLIGHT.with_label_values(&[&store.to_string()])
}

// glue: metric child
pub(crate) fn store_schedule_wait(store: u64) -> Histogram {
    STORE_SCHEDULE_WAIT.with_label_values(&[&store.to_string()])
}

// glue: client-go metrics.TiKVStoreLimitErrorCounter.WithLabelValues(addr, store).Inc()
pub(crate) fn store_limit_token_error(addr: &str, store: u64) {
    STORE_LIMIT_TOKEN_ERROR
        .with_label_values(&[addr, &store.to_string()])
        .inc();
}

// glue: metric child
pub(crate) fn poll_stalls(task: &str) -> IntCounter {
    POLL_STALLS.with_label_values(&[task])
}

// glue: metric
pub(crate) fn poll_stall(d: Duration) {
    POLL_STALL_DURATION.observe(d.as_secs_f64());
}

// glue: metric
pub(crate) fn cpu_slice(d: Duration) {
    CPU_SLICE.observe(d.as_secs_f64());
}

// glue: metric child
pub(crate) fn tasks(task: &str) -> IntGauge {
    TASKS.with_label_values(&[task])
}

// glue: metric
pub(crate) fn panic() {
    PANICS.inc();
}

/// Force every runtime metric, so it is exported before first use (Go registers at init).
// glue: registration
pub fn init() {
    for q in [Queue::Admission, Queue::StoreWindow, Queue::Blocking] {
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
