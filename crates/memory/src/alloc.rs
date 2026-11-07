//! `TrackedAlloc` (A1 3.3, mapping rule `M-MEM`): the allocator of every large buffer (chunk
//! columns, row containers, hash tables, aggregation state, sort buffers). It records the
//! **real** number of bytes allocated and freed in a tracker and forwards to the global
//! allocator (jemalloc in tidb-server); it does not manage memory itself.
//!
//! Hot path (A1 3.3 item 2): an allocation only adds to the handle's pending counter; every
//! 64 KiB (either way) the pending bytes are recorded in the tracker tree with
//! [`Tracker::record`], which adds along the parent chain and updates `MaxConsumed`, but runs
//! no action, takes no action lock and never fails. The actions run at the next
//! [`Tracker::checkpoint`] (item 3), placed where Go calls `Consume` with a positive delta.
//! The batching error is at most 64 KiB per live handle; [`TrackedAlloc::flush`] records the
//! rest (and dropping the last clone of a handle does).
//!
//! The pending counter is per handle (shared by its clones), not per thread: one atomic add
//! per allocation. A per-thread split is an optimisation left to the benchmark (A1 §5
//! "记账开销").

use std::alloc::Layout;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering::Relaxed;

use allocator_api2::alloc::AllocError;
use allocator_api2::alloc::Allocator;
use allocator_api2::alloc::Global;

use crate::tracker::Tracker;

/// The batch after which a handle's pending bytes are recorded in the tracker tree.
pub const TRACKED_ALLOC_BATCH: i64 = 64 << 10;

struct Account {
    tracker: Arc<Tracker>,
    pending: AtomicI64,
}

impl Drop for Account {
    // glue: records the remaining pending bytes when the last handle goes away
    fn drop(&mut self) {
        let d = self.pending.swap(0, Relaxed);
        if d != 0 {
            self.tracker.record(d);
        }
    }
}

/// A handle that allocates through the global allocator and accounts the bytes in a tracker.
/// Cheap to clone; clones share the pending counter.
#[derive(Clone)]
pub struct TrackedAlloc {
    account: Arc<Account>,
}

impl std::fmt::Debug for TrackedAlloc {
    // glue: Debug
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrackedAlloc")
            .field("tracker", &self.account.tracker)
            .field("pending", &self.pending())
            .finish()
    }
}

impl TrackedAlloc {
    /// A handle accounting in `tracker` (a leaf, normally an operator's tracker).
    // glue: A1 TrackedAlloc
    pub fn new(tracker: Arc<Tracker>) -> TrackedAlloc {
        TrackedAlloc {
            account: Arc::new(Account {
                tracker,
                pending: AtomicI64::new(0),
            }),
        }
    }

    /// The tracker the bytes are accounted in.
    // glue: A1 TrackedAlloc
    pub fn tracker(&self) -> &Arc<Tracker> {
        &self.account.tracker
    }

    /// Bytes allocated (positive) or freed (negative) and not yet recorded in the tracker.
    // glue: A1 TrackedAlloc
    pub fn pending(&self) -> i64 {
        self.account.pending.load(Relaxed)
    }

    /// Records the pending bytes in the tracker tree now.
    // glue: A1 TrackedAlloc
    pub fn flush(&self) {
        let d = self.account.pending.swap(0, Relaxed);
        if d != 0 {
            self.account.tracker.record(d);
        }
    }

    // glue: A1 TrackedAlloc hot path (one atomic add; a tree walk every 64 KiB)
    #[inline]
    fn account(&self, delta: i64) {
        let p = self.account.pending.fetch_add(delta, Relaxed) + delta;
        if !(-TRACKED_ALLOC_BATCH..TRACKED_ALLOC_BATCH).contains(&p) {
            self.flush();
        }
    }
}

// SAFETY: every method forwards to `Global` with the same arguments, so the allocator
// contract (valid, correctly sized and aligned blocks; `deallocate` / `grow` / `shrink` only
// of blocks from this allocator, i.e. from `Global`) is `Global`'s. The accounting only reads
// `layout.size()`. Clones share the account and allocate from the same `Global`, so a block
// may be freed through any clone.
#[allow(unsafe_code)]
unsafe impl Allocator for TrackedAlloc {
    // glue: A1 TrackedAlloc (forwards to the global allocator, accounts the size)
    #[inline]
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let p = Global.allocate(layout)?;
        self.account(layout.size() as i64);
        Ok(p)
    }

    // glue: A1 TrackedAlloc (forwards to the global allocator, accounts the size)
    #[inline]
    fn allocate_zeroed(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let p = Global.allocate_zeroed(layout)?;
        self.account(layout.size() as i64);
        Ok(p)
    }

    // glue: A1 TrackedAlloc (forwards to the global allocator, accounts the size)
    #[inline]
    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        // SAFETY: the caller's contract for `deallocate` is the same as `Global`'s.
        unsafe { Global.deallocate(ptr, layout) };
        self.account(-(layout.size() as i64));
    }

    // glue: A1 TrackedAlloc (forwards to the global allocator, accounts the size change)
    #[inline]
    unsafe fn grow(
        &self,
        ptr: NonNull<u8>,
        old_layout: Layout,
        new_layout: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        // SAFETY: the caller's contract for `grow` is the same as `Global`'s.
        let p = unsafe { Global.grow(ptr, old_layout, new_layout) }?;
        self.account(new_layout.size() as i64 - old_layout.size() as i64);
        Ok(p)
    }

    // glue: A1 TrackedAlloc (forwards to the global allocator, accounts the size change)
    #[inline]
    unsafe fn grow_zeroed(
        &self,
        ptr: NonNull<u8>,
        old_layout: Layout,
        new_layout: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        // SAFETY: the caller's contract for `grow_zeroed` is the same as `Global`'s.
        let p = unsafe { Global.grow_zeroed(ptr, old_layout, new_layout) }?;
        self.account(new_layout.size() as i64 - old_layout.size() as i64);
        Ok(p)
    }

    // glue: A1 TrackedAlloc (forwards to the global allocator, accounts the size change)
    #[inline]
    unsafe fn shrink(
        &self,
        ptr: NonNull<u8>,
        old_layout: Layout,
        new_layout: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        // SAFETY: the caller's contract for `shrink` is the same as `Global`'s.
        let p = unsafe { Global.shrink(ptr, old_layout, new_layout) }?;
        self.account(new_layout.size() as i64 - old_layout.size() as i64);
        Ok(p)
    }
}

#[cfg(test)]
mod tests {
    use allocator_api2::vec::Vec;

    use super::*;
    use crate::tracker::new_tracker;

    #[test]
    fn real_bytes_and_zero_after_drop() {
        let root = new_tracker(1, -1);
        let leaf = new_tracker(2, -1);
        leaf.attach_to(&root).unwrap();
        let a = TrackedAlloc::new(leaf.clone());
        {
            let mut v: Vec<u8, TrackedAlloc> = Vec::with_capacity_in(1 << 20, a.clone());
            v.extend(std::iter::repeat_n(7u8, 1 << 20));
            // 1 MiB > the batch: recorded at once.
            assert_eq!(leaf.bytes_consumed() + a.pending(), 1 << 20);
            assert_eq!(root.bytes_consumed(), leaf.bytes_consumed());
            v.reserve(1 << 20); // grow
            assert!(leaf.bytes_consumed() + a.pending() >= 2 << 20);
        }
        a.flush();
        assert_eq!(leaf.bytes_consumed(), 0);
        assert_eq!(root.bytes_consumed(), 0);
        assert!(root.max_consumed() >= 2 << 20);
    }

    #[test]
    fn small_allocations_are_batched() {
        let t = new_tracker(1, -1);
        let a = TrackedAlloc::new(t.clone());
        let v: Vec<u8, TrackedAlloc> = Vec::with_capacity_in(100, a.clone());
        assert_eq!(t.bytes_consumed(), 0);
        assert_eq!(a.pending(), 100);
        drop(v);
        assert_eq!(a.pending(), 0);
        let w: Vec<u8, TrackedAlloc> = Vec::with_capacity_in(100, a.clone());
        drop(a);
        // The vector holds a clone: the account lives until it is dropped.
        drop(w);
        assert_eq!(t.bytes_consumed(), 0);
    }

    #[test]
    fn hard_limit_acts_at_checkpoint_only() {
        let t = new_tracker(1, 1000);
        let hit = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let log = crate::action::LogOnExceed::new(9);
        let h = hit.clone();
        log.set_log_hook(Some(Arc::new(move |_| {
            h.fetch_add(1, Relaxed);
        })));
        t.set_action_on_exceed(Some(log));
        let a = TrackedAlloc::new(t.clone());
        let v: Vec<u8, TrackedAlloc> = Vec::with_capacity_in(1 << 17, a.clone());
        assert!(t.bytes_consumed() >= 1 << 17);
        assert_eq!(hit.load(Relaxed), 0, "no action in the allocation path");
        t.checkpoint().unwrap();
        assert_eq!(hit.load(Relaxed), 1);
        drop(v);
    }
}
