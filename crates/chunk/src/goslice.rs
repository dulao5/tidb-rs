//! Go slice semantics for the column buffers (N5a 3.2; mapping rules `M-ALIAS`, `M-MEM`).
//!
//! A Go `[]byte` / `[]int64` has a length and a capacity, and `append` grows the capacity
//! with the runtime's `growslice` rule (doubling, then 1.25x, rounded up to an allocator size
//! class). The chunk code depends on both in ways that are observable:
//!
//! - `Column.resize` reslices in place when the capacity suffices (exposing the bytes that
//!   were there, e.g. for `ResizeInt64(n, true)`) and allocates a zeroed buffer otherwise;
//!   the bytes under NULL cells end up in `Codec.Encode`'s output.
//! - `Chunk.MemoryUsage` sums the capacities.
//!
//! [`GoSlice`] therefore keeps Go's length and capacity, computes the capacity of every
//! growth with [`growslice_cap`], and backs a capacity of `n` elements with an allocation of
//! exactly `n * E` bytes, initialised to zero as Go's allocator does (so the bytes accounted
//! by `TrackedAlloc` are Go's capacity). Bytes written and then cut off by a reslice stay in
//! the buffer and reappear on a reslice up, as in Go.
//!
//! Go's slice expressions `s[a:b]` may reach past the length up to the capacity; the chunk
//! code relies on it (e.g. `Int64s` returns `c.length` elements whatever `len(c.data)` is).
//! [`GoSlice::capb`] gives the whole capacity for those reads; index expressions `s[i]` check
//! the length ([`GoSlice::bytes`]).
//!
//! A buffer is either owned or a zero-copy slice of a shared response buffer (Go: a
//! sub-slice of the gRPC message, `SharedBytes`). Go writes into such a slice in place; here
//! the first write copies it into an owned buffer of the same capacity (copy on write). Go
//! limits the capacity of these slices to their length (three-index slices), so an append
//! reallocates in both languages.

use std::alloc::Layout;
use std::fmt;
use std::ptr::NonNull;

use allocator_api2::alloc::AllocError;
use allocator_api2::alloc::Allocator;
use allocator_api2::alloc::Global;
use allocator_api2::vec::Vec as AVec;
use bytes::Bytes;
use tidb_memory::TrackedAlloc;

/// The allocator of owned column buffers: the global allocator, or a [`TrackedAlloc`] that
/// accounts the real bytes in an operator's tracker (A1 3.3, N5a 3.7).
#[derive(Clone, Default, Debug)]
pub enum ColAlloc {
    /// Not accounted (tests, constant folding, metadata).
    #[default]
    Global,
    /// Accounted in a memory tracker.
    Tracked(TrackedAlloc),
}

// SAFETY: every method forwards to `Global` or to `TrackedAlloc` (itself a forwarder to
// `Global`) with the same arguments. Both variants allocate from the global allocator, and a
// buffer keeps the `ColAlloc` it was allocated with (allocator_api2 vectors own their
// allocator), so `deallocate` / `grow` / `shrink` always reach the allocator that produced
// the block.
#[allow(unsafe_code)]
unsafe impl Allocator for ColAlloc {
    // glue: allocator forwarding (A1 TrackedAlloc or the global allocator)
    #[inline]
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        match self {
            ColAlloc::Global => Global.allocate(layout),
            ColAlloc::Tracked(a) => a.allocate(layout),
        }
    }

    // glue: allocator forwarding (A1 TrackedAlloc or the global allocator)
    #[inline]
    fn allocate_zeroed(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        match self {
            ColAlloc::Global => Global.allocate_zeroed(layout),
            ColAlloc::Tracked(a) => a.allocate_zeroed(layout),
        }
    }

    // glue: allocator forwarding (A1 TrackedAlloc or the global allocator)
    #[inline]
    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        // SAFETY: the caller's contract is forwarded unchanged.
        unsafe {
            match self {
                ColAlloc::Global => Global.deallocate(ptr, layout),
                ColAlloc::Tracked(a) => a.deallocate(ptr, layout),
            }
        }
    }

    // glue: allocator forwarding (A1 TrackedAlloc or the global allocator)
    #[inline]
    unsafe fn grow(
        &self,
        ptr: NonNull<u8>,
        old_layout: Layout,
        new_layout: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        // SAFETY: the caller's contract is forwarded unchanged.
        unsafe {
            match self {
                ColAlloc::Global => Global.grow(ptr, old_layout, new_layout),
                ColAlloc::Tracked(a) => a.grow(ptr, old_layout, new_layout),
            }
        }
    }

    // glue: allocator forwarding (A1 TrackedAlloc or the global allocator)
    #[inline]
    unsafe fn shrink(
        &self,
        ptr: NonNull<u8>,
        old_layout: Layout,
        new_layout: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        // SAFETY: the caller's contract is forwarded unchanged.
        unsafe {
            match self {
                ColAlloc::Global => Global.shrink(ptr, old_layout, new_layout),
                ColAlloc::Tracked(a) => a.shrink(ptr, old_layout, new_layout),
            }
        }
    }
}

/// Go's allocator size classes (`internal/runtime/gc/sizeclasses.go`, Go 1.25).
const SIZE_CLASSES: [usize; 68] = [
    0, 8, 16, 24, 32, 48, 64, 80, 96, 112, 128, 144, 160, 176, 192, 208, 224, 240, 256, 288, 320,
    352, 384, 416, 448, 480, 512, 576, 640, 704, 768, 896, 1024, 1152, 1280, 1408, 1536, 1792,
    2048, 2304, 2688, 3072, 3200, 3456, 4096, 4864, 5376, 6144, 6528, 6784, 6912, 8192, 9472, 9728,
    10240, 10880, 12288, 13568, 14336, 16384, 18432, 19072, 20480, 21760, 24576, 27264, 28672,
    32768,
];

/// `roundupsize(size, noscan=true)`: the size of the block `mallocgc` returns for a request
/// of `size` bytes of pointer-free memory.
// go-std: runtime.roundupsize L1 (noscan: no malloc header)
pub(crate) fn roundupsize(size: usize) -> usize {
    const MAX_SMALL_SIZE: usize = 32768;
    const MALLOC_HEADER_SIZE: usize = 8;
    const PAGE_SIZE: usize = 8192;
    if size <= MAX_SMALL_SIZE - MALLOC_HEADER_SIZE {
        // The size-to-class tables map a size to the smallest class that holds it.
        let i = SIZE_CLASSES.partition_point(|&c| c < size);
        return SIZE_CLASSES[i];
    }
    match size.checked_add(PAGE_SIZE - 1) {
        Some(r) => r & !(PAGE_SIZE - 1),
        None => size,
    }
}

/// `nextslicecap`.
// go-std: runtime.nextslicecap L1
fn nextslicecap(new_len: usize, old_cap: usize) -> usize {
    let mut newcap = old_cap;
    let doublecap = newcap + newcap;
    if new_len > doublecap {
        return new_len;
    }
    const THRESHOLD: usize = 256;
    if old_cap < THRESHOLD {
        return doublecap;
    }
    loop {
        // Transition from growing 2x for small slices
        // to growing 1.25x for large slices.
        newcap += (newcap + 3 * THRESHOLD) >> 2;
        if newcap >= new_len {
            break;
        }
    }
    newcap
}

/// The capacity, in elements of `elem` bytes, of the slice `append` allocates when a slice
/// of capacity `old_cap` must hold `new_len` elements (`growslice` for pointer-free element
/// types of size 1 and 8, the two the chunk package appends to).
// go-std: runtime.growslice L2 (capacity computation only; the copy is GoSlice::grow)
pub(crate) fn growslice_cap(new_len: usize, old_cap: usize, elem: usize) -> usize {
    let newcap = nextslicecap(new_len, old_cap);
    roundupsize(newcap * elem) / elem
}

#[derive(Clone)]
enum Store {
    /// `v.len()` is `start + cap * E`: the whole capacity, initialised.
    Owned(AVec<u8, ColAlloc>),
    /// `cap * E` bytes of a shared buffer, starting at element 0.
    Shared(Bytes),
}

/// A Go slice of `E`-byte elements (`[]byte` for `E = 1`, `[]int64` stored little-endian
/// for `E = 8`).
/// A Go slice of `E`-byte elements (`[]byte` for `E = 1`, `[]int64` stored little-endian
/// for `E = 8`).
///
/// `ptr` caches the address of element 0 (`store` plus `start`), so reads and writes within
/// the capacity do not match on `store` (N5a §5: the accessors are on the per-cell path).
/// Invariant: `ptr` is valid for `cap * E` initialised bytes; it is recomputed by
/// [`GoSlice::sync`] whenever `store` or `start` changes. The bytes of an owned buffer are
/// only reached through `ptr` (never by indexing the vector), so the pointer stays valid
/// under the aliasing rules until the vector is replaced.
pub(crate) struct GoSlice<const E: usize> {
    store: Store,
    ptr: NonNull<u8>,
    /// Byte offset of element 0 in an owned buffer (Go: a slice that does not start at the
    /// beginning of its array, after `s = s[k:]`).
    start: usize,
    len: usize,
    cap: usize,
}

// SAFETY: `ptr` only points into `store` (an owned vector or a `Bytes`, both Send + Sync),
// and it is written or read only through `&mut self` / `&self` like the store itself.
#[allow(unsafe_code)]
unsafe impl<const E: usize> Send for GoSlice<E> {}
// SAFETY: see `Send`: `&GoSlice` only reads through `ptr`.
#[allow(unsafe_code)]
unsafe impl<const E: usize> Sync for GoSlice<E> {}

impl<const E: usize> Clone for GoSlice<E> {
    // glue: Clone (the cached pointer must point into the clone's own buffer)
    fn clone(&self) -> Self {
        GoSlice::from_store(self.store.clone(), self.start, self.len, self.cap)
    }
}

impl<const E: usize> fmt::Debug for GoSlice<E> {
    // glue: Debug
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GoSlice")
            .field("len", &self.len)
            .field("cap", &self.cap)
            .field("shared", &matches!(self.store, Store::Shared(_)))
            .finish()
    }
}

impl<const E: usize> GoSlice<E> {
    /// A slice over `store` with element 0 at byte `start` (`ptr` computed).
    // glue: constructor maintaining the cached pointer
    #[inline]
    fn from_store(mut store: Store, start: usize, len: usize, cap: usize) -> Self {
        let ptr = Self::elem0(&mut store, start);
        GoSlice {
            store,
            ptr,
            start,
            len,
            cap,
        }
    }

    /// Recomputes `ptr` after `store` or `start` changed.
    // glue: cached element pointer
    #[inline]
    fn sync(&mut self) {
        self.ptr = Self::elem0(&mut self.store, self.start);
    }

    /// The address of element 0 of `store` with element 0 at byte `start`.
    // glue: cached element pointer
    #[inline]
    fn elem0(store: &mut Store, start: usize) -> NonNull<u8> {
        match store {
            // `as_mut_ptr` does not materialise a reference to the elements, and
            // `start <= v.len()`, so the pointer is in bounds (or one past the end when the
            // capacity is 0). A vector's pointer is never null.
            Store::Owned(v) => {
                NonNull::new(v.as_mut_ptr().wrapping_add(start)).unwrap_or(NonNull::dangling())
            }
            Store::Shared(b) => NonNull::from(b.as_ref()).cast::<u8>(),
        }
    }

    /// A nil slice.
    // glue: Go nil slice
    pub(crate) fn nil(alloc: ColAlloc) -> Self {
        Self::from_store(Store::Owned(AVec::new_in(alloc)), 0, 0, 0)
    }

    /// `make([]T, len, cap)`: zeroed, capacity exactly `cap`.
    // glue: Go make
    pub(crate) fn make(len: usize, cap: usize, alloc: ColAlloc) -> Self {
        debug_assert!(len <= cap);
        let mut v = AVec::with_capacity_in(cap * E, alloc);
        v.resize(cap * E, 0);
        Self::from_store(Store::Owned(v), 0, len, cap)
    }

    /// A slice of a shared buffer, with capacity equal to its length (Go's three-index
    /// slice `buf[:n:n]`). `b.len()` must be a multiple of `E`.
    // glue: Go three-index slice of a shared buffer (SharedBytes, N5a 3.4)
    pub(crate) fn shared(b: Bytes) -> Self {
        let n = b.len() / E;
        // Computed before the struct is built (no store-then-reload of the new slice).
        let ptr = NonNull::from(b.as_ref()).cast::<u8>();
        GoSlice {
            store: Store::Shared(b),
            ptr,
            start: 0,
            len: n,
            cap: n,
        }
    }

    /// `s = buf[from:to:to]` in place: the same as `*self = GoSlice::shared(buf.slice(..))`,
    /// written field by field (the decode path; no temporary slice moved into place).
    // glue: Go three-index slice of a shared buffer (SharedBytes, N5a 3.4)
    #[inline]
    pub(crate) fn set_shared(&mut self, buf: &Bytes, from: usize, to: usize) {
        let b = buf.slice(from..to);
        let n = b.len() / E;
        self.ptr = NonNull::from(b.as_ref()).cast::<u8>();
        self.start = 0;
        self.len = n;
        self.cap = n;
        self.store = Store::Shared(b);
    }

    /// `len(s)`.
    // glue: Go len
    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// `cap(s)`.
    // glue: Go cap
    #[inline]
    pub(crate) fn cap(&self) -> usize {
        self.cap
    }

    /// Whether the slice references a shared buffer.
    // glue: N5a ColBuf::Shared
    #[inline]
    pub(crate) fn is_shared(&self) -> bool {
        matches!(self.store, Store::Shared(_))
    }

    /// The allocator of an owned buffer (`Global` for a shared one).
    // glue: N5a 3.2
    pub(crate) fn alloc(&self) -> ColAlloc {
        match &self.store {
            Store::Owned(v) => v.allocator().clone(),
            Store::Shared(_) => ColAlloc::Global,
        }
    }

    /// The elements as bytes.
    // glue: Go slice read access
    #[inline]
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.capb()[..self.len * E]
    }

    /// The elements as mutable bytes (copies a shared buffer first).
    // glue: Go slice write access (copy on write of a shared buffer, N5a 3.2)
    #[inline]
    pub(crate) fn bytes_mut(&mut self) -> &mut [u8] {
        let n = self.len * E;
        &mut self.capb_mut()[..n]
    }

    /// Byte range `[from, to)` of the capacity, writable.
    // glue: Go in-place write within the capacity
    #[inline]
    fn owned_range(&mut self, from: usize, to: usize) -> &mut [u8] {
        &mut self.capb_mut()[from..to]
    }

    /// The whole capacity as mutable bytes (copies a shared buffer first).
    // glue: Go slice expression s[a:b] with b <= cap(s), written
    #[inline]
    pub(crate) fn capb_mut(&mut self) -> &mut [u8] {
        self.make_owned();
        // SAFETY: `ptr` is valid for `cap * E` initialised bytes of the owned vector (struct
        // invariant), which `&mut self` borrows exclusively.
        #[allow(unsafe_code)]
        unsafe {
            std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.cap * E)
        }
    }

    /// The whole capacity as bytes (Go's slice expressions may reach up to the capacity).
    // glue: Go slice expression s[a:b] with b <= cap(s)
    #[inline]
    pub(crate) fn capb(&self) -> &[u8] {
        // SAFETY: `ptr` is valid for `cap * E` initialised bytes (struct invariant), borrowed
        // with `&self`.
        #[allow(unsafe_code)]
        unsafe {
            std::slice::from_raw_parts(self.ptr.as_ptr(), self.cap * E)
        }
    }

    /// Makes the buffer owned, copying a shared buffer (its whole capacity) first.
    // glue: copy on write of a shared buffer (N5a 3.2)
    #[inline(always)]
    fn make_owned(&mut self) {
        if !matches!(self.store, Store::Owned(_)) {
            self.unshare();
        }
    }

    // glue: copy on write of a shared buffer (N5a 3.2)
    #[cold]
    #[inline(never)]
    fn unshare(&mut self) {
        if let Store::Shared(b) = &self.store {
            let mut v = AVec::with_capacity_in(self.cap * E, ColAlloc::Global);
            v.extend_from_slice(&b[..self.cap * E]);
            self.store = Store::Owned(v);
            self.start = 0;
            self.sync();
        }
    }

    /// `s = s[:n]` (`n <= cap(s)`).
    // glue: Go reslice
    #[inline]
    pub(crate) fn reslice(&mut self, n: usize) {
        assert!(
            n <= self.cap,
            "slice bounds out of range [:{n}] with capacity {}",
            self.cap
        );
        self.len = n;
    }

    /// `s = s[k:]`.
    // glue: Go reslice from the front
    pub(crate) fn advance(&mut self, k: usize) {
        assert!(
            k <= self.len,
            "slice bounds out of range [{k}:{}]",
            self.len
        );
        match &mut self.store {
            Store::Owned(_) => self.start += k * E,
            Store::Shared(b) => *b = b.slice(k * E..),
        }
        self.len -= k;
        self.cap -= k;
        self.sync();
    }

    /// Reallocates for at least `new_len` elements with Go's growth rule, keeping the
    /// elements (`growslice`: the old bytes beyond the length are not copied).
    // glue: Go growslice (copy part)
    #[cold]
    fn grow(&mut self, new_len: usize) {
        let newcap = growslice_cap(new_len, self.cap, E);
        let alloc = self.alloc();
        let mut v = AVec::with_capacity_in(newcap * E, alloc);
        v.extend_from_slice(self.bytes());
        v.resize(newcap * E, 0);
        self.store = Store::Owned(v);
        self.start = 0;
        self.cap = newcap;
        self.sync();
    }

    /// `s = append(s, src...)` (`src.len()` a multiple of `E`).
    // glue: Go append
    #[inline(always)]
    pub(crate) fn append(&mut self, src: &[u8]) {
        let n = src.len() / E;
        if n <= self.cap - self.len && matches!(self.store, Store::Owned(_)) {
            let at = self.len * E;
            // SAFETY: `len + n <= cap`, so `[at, at + src.len())` lies within the `cap * E`
            // initialised bytes of the owned vector behind `ptr` (struct invariant), which
            // `&mut self` borrows exclusively; `src` cannot overlap it (it is a shared borrow
            // that is live at the same time as `&mut self`).
            #[allow(unsafe_code)]
            unsafe {
                let dst = self.ptr.as_ptr().add(at);
                // Small fixed sizes (one element of a fixed-length column) without a memcpy
                // call.
                match src.len() {
                    8 => dst
                        .cast::<[u8; 8]>()
                        .write_unaligned(*src.as_ptr().cast::<[u8; 8]>()),
                    4 => dst
                        .cast::<[u8; 4]>()
                        .write_unaligned(*src.as_ptr().cast::<[u8; 4]>()),
                    1 => *dst = src[0],
                    n => std::ptr::copy_nonoverlapping(src.as_ptr(), dst, n),
                }
            }
            self.len += n;
            return;
        }
        self.append_slow(src);
    }

    // glue: Go append (growth or copy on write)
    #[cold]
    #[inline(never)]
    fn append_slow(&mut self, src: &[u8]) {
        let n = src.len() / E;
        if self.len + n > self.cap {
            self.grow(self.len + n);
        }
        let at = self.len * E;
        self.owned_range(at, at + src.len()).copy_from_slice(src);
        self.len += n;
    }

    /// `s = append(s, make([]T, n)...)`-like: appends `n` elements of byte value `b`.
    // glue: Go append of a repeated value
    pub(crate) fn append_fill(&mut self, n: usize, b: u8) {
        if self.len + n > self.cap {
            self.grow(self.len + n);
        }
        let at = self.len * E;
        self.owned_range(at, at + n * E).fill(b);
        self.len += n;
    }

    /// Byte `i` (`E = 1`).
    // glue: Go index
    #[inline(always)]
    pub(crate) fn at(&self, i: usize) -> u8 {
        assert!(
            i < self.len,
            "index out of range [{i}] with length {}",
            self.len
        );
        self.capb()[i]
    }

    /// `&mut s[i]` (`E = 1`).
    // glue: Go index (write)
    #[inline(always)]
    pub(crate) fn at_mut(&mut self, i: usize) -> &mut u8 {
        assert!(
            i < self.len,
            "index out of range [{i}] with length {}",
            self.len
        );
        &mut self.capb_mut()[i]
    }
}

impl GoSlice<8> {
    /// `s[i]` of a `[]int64`.
    // glue: Go index of []int64 (little-endian storage)
    #[inline]
    pub(crate) fn i64_at(&self, i: usize) -> i64 {
        assert!(
            i < self.len,
            "index out of range [{i}] with length {}",
            self.len
        );
        let b = self.capb();
        i64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap())
    }

    /// `s[i] = v` of a `[]int64`.
    // glue: Go index of []int64 (little-endian storage)
    #[inline]
    pub(crate) fn set_i64(&mut self, i: usize, v: i64) {
        self.bytes_mut()[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
    }

    /// `s = append(s, v)` of a `[]int64`.
    // glue: Go append to []int64
    #[inline]
    pub(crate) fn push_i64(&mut self, v: i64) {
        self.append(&v.to_le_bytes());
    }

    /// The last element.
    // glue: Go s[len(s)-1]
    #[inline]
    pub(crate) fn last_i64(&self) -> i64 {
        self.i64_at(self.len - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn growth_matches_go() {
        // append([]byte(nil), make([]byte, n)...) caps, from Go 1.25.
        assert_eq!(growslice_cap(1, 0, 1), 8);
        assert_eq!(growslice_cap(40, 0, 1), 48);
        assert_eq!(growslice_cap(1, 0, 8), 1);
        assert_eq!(growslice_cap(9, 8, 1), 16);
        assert_eq!(growslice_cap(257, 256, 8), 512);
        assert_eq!(growslice_cap(513, 512, 8), 848);
        assert_eq!(growslice_cap(33000, 32768, 1), 49152);
    }

    #[test]
    fn reslice_exposes_written_bytes() {
        let mut s: GoSlice<1> = GoSlice::make(0, 16, ColAlloc::Global);
        s.append(&[1, 2, 3, 4]);
        s.reslice(0);
        s.reslice(6);
        assert_eq!(s.bytes(), &[1, 2, 3, 4, 0, 0]);
        let mut sh = GoSlice::<1>::shared(Bytes::from_static(&[9, 8, 7]));
        sh.advance(1);
        assert_eq!((sh.len(), sh.cap()), (2, 2));
        sh.append(&[6]);
        assert_eq!(sh.bytes(), &[8, 7, 6]);
        assert_eq!(sh.cap(), 8);
    }
}
