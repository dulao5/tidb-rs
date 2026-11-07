//! Per-poll and per-metric overhead of the runtime (PR #8 review 3).
//!
//! - `poll`: 8 tasks on an 8-worker Steal runtime, each yielding 10,000 times (one poll per
//!   yield), started with plain `tokio::spawn` vs `ExecPool::spawn` (instrumented: poll
//!   timing, CPU accounting, stall check, thread-local histogram). The difference per poll is
//!   the instrumentation's cost under contention.
//! - `histogram`: 8 threads observing a shared prometheus `Histogram` vs [`FastHistogram`].
//! - `gauge`: 8 threads doing inc/dec on a shared `IntGauge` vs [`ShardedGauge`].
//!
//! `cargo bench -p tidb-runtime --bench overhead`

use std::sync::Arc;
use std::sync::Barrier;
use std::time::Duration;
use std::time::Instant;

use criterion::Criterion;
use criterion::criterion_group;
use criterion::criterion_main;
use prometheus::Histogram;
use prometheus::HistogramOpts;
use prometheus::IntGauge;
use tidb_runtime::Runtime;
use tidb_runtime::RuntimeConfig;
use tidb_runtime::Topology;
use tidb_runtime::local::FastHistogram;
use tidb_runtime::local::ShardedGauge;

const THREADS: usize = 8;
const YIELDS: u32 = 10_000;

fn poll(c: &mut Criterion) {
    let rt = Runtime::new(RuntimeConfig {
        topology: Topology::Steal,
        worker_threads: THREADS,
        ..RuntimeConfig::default()
    })
    .unwrap();
    let mut g = c.benchmark_group("poll");
    g.throughput(criterion::Throughput::Elements(
        u64::from(YIELDS) * THREADS as u64,
    ));
    g.bench_function("tokio_spawn", |b| {
        b.iter(|| {
            rt.block_on(async {
                let hs: Vec<_> = (0..THREADS)
                    .map(|_| {
                        tokio::spawn(async {
                            for _ in 0..YIELDS {
                                tokio::task::yield_now().await;
                            }
                        })
                    })
                    .collect();
                for h in hs {
                    h.await.unwrap();
                }
            })
        })
    });
    let pool = rt.pool().clone();
    g.bench_function("exec_pool_spawn", |b| {
        b.iter(|| {
            let pool = pool.clone();
            rt.block_on(async move {
                let hs: Vec<_> = (0..THREADS)
                    .map(|_| {
                        pool.spawn("bench-poll", async {
                            for _ in 0..YIELDS {
                                tokio::task::yield_now().await;
                            }
                        })
                    })
                    .collect();
                for h in hs {
                    h.await.unwrap();
                }
            })
        })
    });
    g.finish();
}

/// Run `op` `iters` times on each of THREADS threads; time of the slowest thread / iters.
fn contended(iters: u64, op: impl Fn() + Send + Sync + 'static) -> Duration {
    let op = Arc::new(op);
    let barrier = Arc::new(Barrier::new(THREADS));
    let ts: Vec<_> = (0..THREADS)
        .map(|_| {
            let op = op.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let start = Instant::now();
                for _ in 0..iters {
                    op();
                }
                start.elapsed()
            })
        })
        .collect();
    ts.into_iter().map(|t| t.join().unwrap()).max().unwrap()
}

fn metrics(c: &mut Criterion) {
    let mut g = c.benchmark_group("metric_8_threads");
    let shared = Histogram::with_opts(HistogramOpts::new("bench_shared", "bench")).unwrap();
    g.bench_function("histogram_shared", |b| {
        let h = shared.clone();
        b.iter_custom(move |n| {
            let h = h.clone();
            contended(n, move || h.observe(1e-4))
        })
    });
    let fast: &'static FastHistogram = Box::leak(Box::new(FastHistogram::new(
        Histogram::with_opts(HistogramOpts::new("bench_fast", "bench")).unwrap(),
    )));
    g.bench_function("histogram_fast", |b| {
        b.iter_custom(|n| contended(n, move || fast.observe_at(1e-4, Instant::now())))
    });
    let gauge = IntGauge::new("bench_gauge", "bench").unwrap();
    g.bench_function("gauge_shared_inc_dec", |b| {
        let gauge = gauge.clone();
        b.iter_custom(move |n| {
            let gauge = gauge.clone();
            contended(n, move || {
                gauge.inc();
                gauge.dec();
            })
        })
    });
    let sharded: &'static ShardedGauge = Box::leak(Box::default());
    g.bench_function("gauge_sharded_inc_dec", |b| {
        b.iter_custom(|n| {
            contended(n, move || {
                sharded.inc();
                sharded.dec();
            })
        })
    });
    g.finish();
}

criterion_group!(benches, poll, metrics);
criterion_main!(benches);
