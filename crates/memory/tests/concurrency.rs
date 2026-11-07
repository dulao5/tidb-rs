//! Concurrent use of the tracker tree (Go: "only BytesConsumed, Consume and AttachTo are
//! thread-safe"): totals and MaxConsumed under many threads, and the GC-aware release path,
//! which the Go-generated vectors leave out (D15.1 plan §4).

use std::sync::Arc;
use std::sync::Barrier;
use std::sync::atomic::Ordering::SeqCst;

use tidb_memory::tracker::EnableGCAwareMemoryTrack;
use tidb_memory::tracker::LabelForGlobalAnalyzeMemory;
use tidb_memory::tracker::new_global_tracker;
use tidb_memory::tracker::new_tracker;

const THREADS: usize = 16;
const ROUNDS: i64 = 20_000;

#[test]
fn consume_from_many_threads_keeps_totals() {
    // Go TestConsume, scaled up: one tracker, threads adding and removing.
    let t = new_tracker(1, -1);
    t.consume(100).unwrap();
    let barrier = Arc::new(Barrier::new(THREADS));
    let hs: Vec<_> = (0..THREADS)
        .map(|i| {
            let t = t.clone();
            let b = barrier.clone();
            std::thread::spawn(move || {
                b.wait();
                let d = if i % 2 == 0 { 10 } else { -10 };
                for _ in 0..ROUNDS {
                    t.consume(d).unwrap();
                }
            })
        })
        .collect();
    for h in hs {
        h.join().unwrap();
    }
    assert_eq!(t.bytes_consumed(), 100);
}

#[test]
fn tree_totals_and_max_under_contention() {
    let root = new_tracker(1, -1);
    let barrier = Arc::new(Barrier::new(THREADS));
    let hs: Vec<_> = (0..THREADS)
        .map(|i| {
            let root = root.clone();
            let b = barrier.clone();
            std::thread::spawn(move || {
                let mid = new_tracker(2, -1);
                let leaf = new_tracker(3, -1);
                mid.attach_to(&root).unwrap();
                leaf.attach_to(&mid).unwrap();
                b.wait();
                for r in 0..ROUNDS {
                    leaf.consume(1 + (r % 7)).unwrap();
                }
                for r in 0..ROUNDS {
                    leaf.consume(-(1 + (r % 7))).unwrap();
                }
                assert_eq!(leaf.bytes_consumed(), 0);
                assert_eq!(mid.bytes_consumed(), 0);
                assert!(mid.max_consumed() >= (0..ROUNDS).map(|r| 1 + (r % 7)).sum::<i64>());
                // Re-attach under load: the bytes move with the tracker.
                leaf.consume(i as i64 + 1).unwrap();
                leaf.attach_to(&root).unwrap();
                leaf
            })
        })
        .collect();
    let leaves: Vec<_> = hs.into_iter().map(|h| h.join().unwrap()).collect();
    let expect: i64 = (1..=THREADS as i64).sum();
    assert_eq!(root.bytes_consumed(), expect);
    // The mids still hold their (now empty) subtrees; every leaf is a direct child now.
    assert_eq!(root.get_children_for_test().len(), 2 * THREADS);
    for l in &leaves {
        l.detach().unwrap();
    }
    assert_eq!(root.bytes_consumed(), 0);
    assert!(root.max_consumed() >= expect);
}

#[test]
fn gc_aware_release_is_immediate() {
    // Go TestRelease: with EnableGCAwareMemoryTrack the global analyze tracker keeps the
    // released bytes until Go's GC runs a finalizer. Rust frees at Drop: they are released
    // at once, so BytesReleased is back to 0 right after Release (Go: after a GC).
    let parent = new_global_tracker(LabelForGlobalAnalyzeMemory, -1);
    let t = new_tracker(1, -1);
    t.attach_to_global_tracker(Some(&parent)).unwrap();
    EnableGCAwareMemoryTrack.store(true, SeqCst);
    t.consume(100).unwrap();
    assert_eq!(parent.bytes_consumed(), 100);
    t.release(100).unwrap();
    assert_eq!(t.bytes_consumed(), 0);
    assert_eq!(parent.bytes_consumed(), 0);
    assert_eq!(parent.bytes_released(), 0);
    EnableGCAwareMemoryTrack.store(false, SeqCst);
}
