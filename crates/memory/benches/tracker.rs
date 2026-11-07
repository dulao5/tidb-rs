//! Tracker and TrackedAlloc costs (D15.1 plan §6), next to Go's `pkg/util/memory/bench_test.go`.
//!
//! `consume_parallel` is Go's `BenchmarkConsume`: `RunParallel` over GOMAXPROCS goroutines,
//! each with its own child of one root, `Consume(256 << 20)`; ns/op = wall time / total ops.

use std::hint::black_box;
use std::sync::Arc;
use std::sync::Barrier;
use std::time::Duration;
use std::time::Instant;

use allocator_api2::vec::Vec as AVec;
use criterion::Criterion;
use criterion::criterion_group;
use criterion::criterion_main;
use tidb_memory::TrackedAlloc;
use tidb_memory::meminfo;
use tidb_memory::tracker::new_tracker;

#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn consume_parallel(iters: u64, threads: usize) -> Duration {
    let root = new_tracker(1, -1);
    let per = iters.div_ceil(threads as u64);
    let barrier = Arc::new(Barrier::new(threads + 1));
    let handles: Vec<_> = (0..threads)
        .map(|_| {
            let root = root.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let child = new_tracker(2, -1);
                child.attach_to(&root).unwrap();
                barrier.wait();
                for _ in 0..per {
                    child.consume(256 << 20).unwrap();
                }
            })
        })
        .collect();
    barrier.wait();
    let start = Instant::now();
    for h in handles {
        h.join().unwrap();
    }
    start.elapsed()
}

fn benches(c: &mut Criterion) {
    let threads = std::thread::available_parallelism().map_or(8, |n| n.get());
    c.bench_function(&format!("consume_parallel/{threads}"), |b| {
        b.iter_custom(|iters| consume_parallel(iters, threads))
    });

    let root = new_tracker(1, -1);
    let mid = new_tracker(2, -1);
    let leaf = new_tracker(3, -1);
    mid.attach_to(&root).unwrap();
    leaf.attach_to(&mid).unwrap();
    c.bench_function("consume_3_levels", |b| {
        b.iter(|| {
            leaf.consume(black_box(64)).unwrap();
            leaf.consume(black_box(-64)).unwrap();
        })
    });

    c.bench_function("mem_total", |b| {
        b.iter(|| black_box(meminfo::mem_total().unwrap()))
    });
    c.bench_function("mem_used", |b| {
        b.iter(|| black_box(meminfo::mem_used().unwrap()))
    });

    // TrackedAlloc vs the global allocator: allocate and free a 4 KiB buffer.
    c.bench_function("alloc_4k/global", |b| {
        b.iter(|| {
            let v: Vec<u8> = Vec::with_capacity(black_box(4096));
            black_box(v.as_ptr());
        })
    });
    let a = TrackedAlloc::new(leaf.clone());
    c.bench_function("alloc_4k/tracked", |b| {
        b.iter(|| {
            let v: AVec<u8, TrackedAlloc> = AVec::with_capacity_in(black_box(4096), a.clone());
            black_box(v.as_ptr());
        })
    });
}

criterion_group!(group, benches);
criterion_main!(group);
