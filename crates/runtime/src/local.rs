//! Hot-path metric primitives that do not bounce cache lines between cores (PR #8 review 3).
//!
//! A shared prometheus `Histogram::observe` or `IntGauge::inc` is an atomic read-modify-write
//! on a line every core writes: measured at ~1.9 µs per operation with 8 threads, the order
//! of a whole poll. The runtime records something on every poll, spawn and queue permit, so:
//!
//! - [`FastHistogram`]: observations go to a thread-local `LocalHistogram` and are flushed
//!   into the shared histogram at most once a second per thread (TiKV's pattern). The caller
//!   passes the `Instant` it already has, so no extra clock read.
//! - [`ShardedGauge`]: per-thread shards (one cache line each), summed when exported by
//!   [`ShardedGaugeVec`], a custom collector. A task may end on another thread than it
//!   started on (Steal topology); the sum is still right.

use std::cell::RefCell;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use parking_lot::RwLock;
use prometheus::Histogram;
use prometheus::local::LocalHistogram;
use prometheus::proto;

/// How long a thread may keep observations before flushing them.
const FLUSH_INTERVAL: Duration = Duration::from_secs(1);

static NEXT_HIST_ID: AtomicUsize = AtomicUsize::new(0);

struct Locals {
    hists: Vec<Option<LocalHistogram>>,
    next_flush: Instant,
}

impl Drop for Locals {
    // glue: flush on thread exit
    fn drop(&mut self) {
        for h in self.hists.iter().flatten() {
            h.flush();
        }
    }
}

thread_local! {
    static LOCALS: RefCell<Locals> = RefCell::new(Locals {
        hists: Vec::new(),
        next_flush: Instant::now() + FLUSH_INTERVAL,
    });
}

/// A histogram observed through thread-local buffers.
pub struct FastHistogram {
    id: usize,
    shared: Histogram,
}

impl FastHistogram {
    // glue: hot-path metric
    pub fn new(shared: Histogram) -> FastHistogram {
        FastHistogram {
            id: NEXT_HIST_ID.fetch_add(1, Ordering::Relaxed),
            shared,
        }
    }

    /// Observe `v` (in the histogram's unit) at time `now`.
    // glue: hot-path metric
    pub fn observe_at(&self, v: f64, now: Instant) {
        let done = LOCALS.try_with(|l| {
            let mut l = l.borrow_mut();
            if l.hists.len() <= self.id {
                l.hists.resize_with(self.id + 1, || None);
            }
            l.hists[self.id]
                .get_or_insert_with(|| self.shared.local())
                .observe(v);
            if now >= l.next_flush {
                for h in l.hists.iter().flatten() {
                    h.flush();
                }
                l.next_flush = now + FLUSH_INTERVAL;
            }
        });
        if done.is_err() {
            // Thread-local already destroyed (thread exit): record directly.
            self.shared.observe(v);
        }
    }

    /// The shared histogram (tests).
    // glue: accessor
    pub fn shared(&self) -> &Histogram {
        &self.shared
    }
}

/// Flush the calling thread's buffered observations now (tests, shutdown).
// glue: hot-path metric
pub fn flush_thread() {
    let _ = LOCALS.try_with(|l| {
        for h in l.borrow().hists.iter().flatten() {
            h.flush();
        }
    });
}

const SHARDS: usize = 32;

#[repr(align(128))]
#[derive(Default)]
struct Shard(AtomicI64);

static NEXT_THREAD: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static SHARD_IDX: usize = NEXT_THREAD.fetch_add(1, Ordering::Relaxed) % SHARDS;
}

// glue: hot-path metric
fn shard_idx() -> usize {
    SHARD_IDX.try_with(|i| *i).unwrap_or(0)
}

/// A gauge with per-thread shards.
pub struct ShardedGauge {
    shards: [Shard; SHARDS],
}

impl std::fmt::Debug for ShardedGauge {
    // glue: Debug
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ShardedGauge({})", self.get())
    }
}

impl Default for ShardedGauge {
    // glue: hot-path metric
    fn default() -> ShardedGauge {
        ShardedGauge {
            shards: std::array::from_fn(|_| Shard::default()),
        }
    }
}

impl ShardedGauge {
    // glue: hot-path metric
    pub fn inc(&self) {
        self.shards[shard_idx()].0.fetch_add(1, Ordering::Relaxed);
    }

    // glue: hot-path metric
    pub fn dec(&self) {
        self.shards[shard_idx()].0.fetch_sub(1, Ordering::Relaxed);
    }

    // glue: hot-path metric
    pub fn get(&self) -> i64 {
        self.shards
            .iter()
            .map(|s| s.0.load(Ordering::Relaxed))
            .sum()
    }
}

/// A gauge family with one label whose children are [`ShardedGauge`]s, exported by a custom
/// collector. Children are leaked: label values are a bounded set (queue names, task names,
/// store ids) and handles must be `'static` to be cached without reference counting.
pub struct ShardedGaugeVec {
    desc: prometheus::core::Desc,
    name: String,
    help: String,
    label: String,
    children: RwLock<Vec<(String, &'static ShardedGauge)>>,
}

impl ShardedGaugeVec {
    /// Create and register in the default registry.
    // glue: hot-path metric
    pub fn register(name: &str, help: &str, label: &str) -> &'static ShardedGaugeVec {
        let desc = prometheus::core::Desc::new(
            name.to_owned(),
            help.to_owned(),
            vec![label.to_owned()],
            Default::default(),
        )
        .expect("valid metric");
        let v: &'static ShardedGaugeVec = Box::leak(Box::new(ShardedGaugeVec {
            desc,
            name: name.to_owned(),
            help: help.to_owned(),
            label: label.to_owned(),
            children: RwLock::new(Vec::new()),
        }));
        let _ = prometheus::register(Box::new(Collector(v)));
        v
    }

    /// The child for `value`, created on first use. Not for hot paths: cache the result.
    // glue: hot-path metric
    pub fn child(&self, value: &str) -> &'static ShardedGauge {
        if let Some((_, g)) = self.children.read().iter().find(|(v, _)| v == value) {
            return g;
        }
        let mut w = self.children.write();
        if let Some((_, g)) = w.iter().find(|(v, _)| v == value) {
            return g;
        }
        let g: &'static ShardedGauge = Box::leak(Box::default());
        w.push((value.to_owned(), g));
        g
    }
}

struct Collector(&'static ShardedGaugeVec);

impl prometheus::core::Collector for Collector {
    // glue: custom collector
    fn desc(&self) -> Vec<&prometheus::core::Desc> {
        vec![&self.0.desc]
    }

    // glue: custom collector
    fn collect(&self) -> Vec<proto::MetricFamily> {
        let v = self.0;
        let mut mf = proto::MetricFamily::default();
        mf.set_name(v.name.clone());
        mf.set_help(v.help.clone());
        mf.set_field_type(proto::MetricType::GAUGE);
        let mut ms = Vec::new();
        for (value, g) in v.children.read().iter() {
            let mut lp = proto::LabelPair::default();
            lp.set_name(v.label.clone());
            lp.set_value(value.clone());
            let mut gauge = proto::Gauge::default();
            gauge.set_value(g.get() as f64);
            let mut m = proto::Metric::default();
            m.set_label(vec![lp]);
            m.set_gauge(gauge);
            ms.push(m);
        }
        mf.set_metric(ms);
        vec![mf]
    }
}
