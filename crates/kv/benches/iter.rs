//! glue: benchmark (D07.0 tasks T9) — the per-row cost of `kv::Iterator::next`, which returns
//! a boxed future (an allocation and a vtable call per row), against a synchronous loop over
//! the same in-memory rows. Go's `Iterator.Next` on an in-memory batch is a plain method call.

use std::hint::black_box;

use criterion::Criterion;
use criterion::criterion_group;
use criterion::criterion_main;
use futures::FutureExt;
use tidb_kv::key::Key;
use tidb_kv::kv::Iterator;
use tidb_kv::kv::KvFuture;

const ROWS: usize = 1024;

/// An in-memory batch, as the memdb or a fetched scan batch would hold it.
struct MemIter {
    keys: Vec<Key>,
    vals: Vec<Vec<u8>>,
    pos: usize,
}

impl MemIter {
    // glue: benchmark fixture
    fn new() -> MemIter {
        MemIter {
            keys: (0..ROWS)
                .map(|i| Key(format!("t_r{i:08}").into_bytes()))
                .collect(),
            vals: (0..ROWS).map(|i| vec![i as u8; 16]).collect(),
            pos: 0,
        }
    }

    // glue: the synchronous alternative (one call, no allocation)
    #[inline(never)]
    fn next_sync(&mut self) {
        self.pos += 1;
    }
}

impl Iterator for MemIter {
    // glue: benchmark fixture
    fn valid(&self) -> bool {
        self.pos < self.keys.len()
    }
    // glue: benchmark fixture
    fn key(&self) -> &Key {
        &self.keys[self.pos]
    }
    // glue: benchmark fixture
    fn value(&self) -> &[u8] {
        &self.vals[self.pos]
    }
    // glue: benchmark fixture
    fn next(&mut self) -> KvFuture<'_, ()> {
        self.pos += 1;
        async { Ok(()) }.boxed()
    }
    // glue: benchmark fixture
    fn close(&mut self) {}
}

// glue: benchmark
fn bench(c: &mut Criterion) {
    let mut g = c.benchmark_group("kv_iter_1024_rows");
    g.bench_function("async_next_dyn", |b| {
        let mut m = MemIter::new();
        b.iter(|| {
            let it: &mut dyn Iterator = &mut m;
            futures::executor::block_on(async {
                let mut n = 0usize;
                while it.valid() {
                    n += it.key().0.len() + it.value().len();
                    it.next().await.unwrap();
                }
                black_box(n);
            });
            m.pos = 0;
        })
    });
    g.bench_function("sync_next", |b| {
        let mut it = MemIter::new();
        b.iter(|| {
            let mut n = 0usize;
            while it.valid() {
                n += it.key().0.len() + it.value().len();
                it.next_sync();
            }
            black_box(n);
            it.pos = 0;
        })
    });
    g.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
