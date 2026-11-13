//! glue: benchmark (D07.0 tasks T9) — the per-row cost of `kv::Iterator::next`, which returns
//! a boxed future (an allocation and a vtable call per row), against a synchronous loop over
//! the same in-memory rows. Go's `Iterator.Next` on an in-memory batch is a plain method call.
//!
//! T11 candidates (specs/D07.0-kv/iter-next-fill.md): C, a synchronous `advance` plus an
//! asynchronous `fill` per batch; D, `next` returning an enum future that allocates only to
//! fetch a batch. Both refill every `BATCH` rows with an already completed fetch.

use std::future::Future;
use std::hint::black_box;
use std::pin::Pin;
use std::task::Context as TaskContext;
use std::task::Poll;

use criterion::Criterion;
use criterion::criterion_group;
use criterion::criterion_main;
use futures::FutureExt;
use tidb_kv::key::Key;
use tidb_kv::kv::Iterator;
use tidb_kv::kv::KvFuture;

/// Rows per fetched batch for the batched candidates (client-go's scanner default
/// `scanBatchSize` is 256).
const BATCH: usize = 256;

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

// ---------------------------------------------------------------- candidate C: next + fill

/// Candidate C: a synchronous `advance` within the current batch and an asynchronous `fill`
/// for the next batch (one boxed future per batch).
trait SplitIter: Send {
    fn valid(&self) -> bool;
    fn key(&self) -> &Key;
    fn value(&self) -> &[u8];
    /// Moves within the current batch; false when the batch is used up and `fill` is due.
    fn advance(&mut self) -> bool;
    fn fill(&mut self) -> KvFuture<'_, ()>;
}

/// The in-memory rows, served in batches of `BATCH` as a scanner would fetch them.
struct Batched {
    m: MemIter,
    /// The end of the current batch.
    end: usize,
}

impl Batched {
    // glue: benchmark fixture
    fn new() -> Batched {
        Batched {
            m: MemIter::new(),
            end: BATCH,
        }
    }
    // glue: benchmark fixture
    fn reset(&mut self) {
        self.m.pos = 0;
        self.end = BATCH;
    }
}

impl SplitIter for Batched {
    // glue: benchmark fixture
    fn valid(&self) -> bool {
        self.m.pos < self.m.keys.len()
    }
    // glue: benchmark fixture
    fn key(&self) -> &Key {
        &self.m.keys[self.m.pos]
    }
    // glue: benchmark fixture
    fn value(&self) -> &[u8] {
        &self.m.vals[self.m.pos]
    }
    // glue: benchmark fixture
    #[inline(never)]
    fn advance(&mut self) -> bool {
        self.m.pos += 1;
        self.m.pos < self.end || self.m.pos >= self.m.keys.len()
    }
    // glue: benchmark fixture (a fetch that is already complete)
    fn fill(&mut self) -> KvFuture<'_, ()> {
        self.end += BATCH;
        async { Ok(()) }.boxed()
    }
}

// ---------------------------------------------------------------- candidate D: enum future

/// Candidate D: `next` keeps its `it.next().await?` call shape but returns this future,
/// which is ready without an allocation while the batch lasts and boxes only a fetch.
enum Next<'a> {
    Ready,
    Fetch(KvFuture<'a, ()>),
}

impl Future for Next<'_> {
    type Output = Result<(), tidb_base::Error>;
    // glue: benchmark fixture
    fn poll(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        match self.get_mut() {
            Next::Ready => Poll::Ready(Ok(())),
            Next::Fetch(f) => f.as_mut().poll(cx),
        }
    }
}

trait EnumIter: Send {
    fn valid(&self) -> bool;
    fn key(&self) -> &Key;
    fn value(&self) -> &[u8];
    fn next(&mut self) -> Next<'_>;
}

impl EnumIter for Batched {
    // glue: benchmark fixture
    fn valid(&self) -> bool {
        SplitIter::valid(self)
    }
    // glue: benchmark fixture
    fn key(&self) -> &Key {
        SplitIter::key(self)
    }
    // glue: benchmark fixture
    fn value(&self) -> &[u8] {
        SplitIter::value(self)
    }
    // glue: benchmark fixture
    #[inline(never)]
    fn next(&mut self) -> Next<'_> {
        if self.advance() {
            Next::Ready
        } else {
            Next::Fetch(self.fill())
        }
    }
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
    g.bench_function("split_advance_fill_dyn", |b| {
        let mut m = Batched::new();
        b.iter(|| {
            let it: &mut dyn SplitIter = &mut m;
            futures::executor::block_on(async {
                let mut n = 0usize;
                while it.valid() {
                    n += it.key().0.len() + it.value().len();
                    if !it.advance() {
                        it.fill().await.unwrap();
                    }
                }
                black_box(n);
            });
            m.reset();
        })
    });
    g.bench_function("enum_next_dyn", |b| {
        let mut m = Batched::new();
        b.iter(|| {
            let it: &mut dyn EnumIter = &mut m;
            futures::executor::block_on(async {
                let mut n = 0usize;
                while it.valid() {
                    n += it.key().0.len() + it.value().len();
                    it.next().await.unwrap();
                }
                black_box(n);
            });
            m.reset();
        })
    });
    g.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
