//! Go's `container/heap` (the functions the top-n heap uses): a min-heap over any type that
//! implements [`Interface`], ordered by its `less`.
//!
//! glue: a port of the Go standard library package for this module (the workspace has no
//! shared port of it yet).

/// `heap.Interface`: `sort.Interface` (`Len`, `Less`, `Swap`) plus `Pop` (`Push` is not
/// used by the functions ported here).
#[allow(clippy::len_without_is_empty)] // Go's sort.Interface has no IsEmpty
pub trait Interface {
    // glue: Go interface method sort.Interface.Len
    fn len(&self) -> usize;
    // glue: Go interface method sort.Interface.Less
    fn less(&self, i: usize, j: usize) -> bool;
    // glue: Go interface method sort.Interface.Swap
    fn swap(&mut self, i: usize, j: usize);
    /// `Pop`: removes the last element (the value Go returns is not used here).
    // glue: Go interface method heap.Interface.Pop
    fn pop(&mut self);
}

/// Init establishes the heap invariants required by the other routines in this package.
/// Init is idempotent with respect to the heap invariants and may be called whenever the
/// heap invariants may have been invalidated. The complexity is O(n) where n = h.Len().
// go-std: heap.Init L1
pub fn init(h: &mut impl Interface) {
    // heapify
    let n = h.len() as isize;
    let mut i = n / 2 - 1;
    while i >= 0 {
        down(h, i, n);
        i -= 1;
    }
}

/// Pop removes and returns the minimum element (according to Less) from the heap. The
/// complexity is O(log n) where n = h.Len(). Pop is equivalent to Remove(h, 0).
// go-std: heap.Pop L1
pub fn pop(h: &mut impl Interface) {
    let n = h.len() as isize - 1;
    h.swap(0, n as usize);
    down(h, 0, n);
    h.pop();
}

/// Fix re-establishes the heap ordering after the element at index i has changed its value.
/// Changing the value of the element at index i and then calling Fix is equivalent to, but
/// less expensive than, calling Remove(h, i) followed by a Push of the new value. The
/// complexity is O(log n) where n = h.Len().
// go-std: heap.Fix L1
pub fn fix(h: &mut impl Interface, i: usize) {
    if !down(h, i as isize, h.len() as isize) {
        up(h, i as isize);
    }
}

// go-std: heap.up L1
fn up(h: &mut impl Interface, mut j: isize) {
    loop {
        let i = (j - 1) / 2; // parent
        if i == j || !h.less(j as usize, i as usize) {
            break;
        }
        h.swap(i as usize, j as usize);
        j = i;
    }
}

// go-std: heap.down L1
fn down(h: &mut impl Interface, i0: isize, n: isize) -> bool {
    let mut i = i0;
    loop {
        let j1 = 2 * i + 1;
        if j1 >= n || j1 < 0 {
            // j1 < 0 after int overflow
            break;
        }
        let mut j = j1; // left child
        let j2 = j1 + 1;
        if j2 < n && h.less(j2 as usize, j1 as usize) {
            j = j2; // = 2*i + 2  // right child
        }
        if !h.less(j as usize, i as usize) {
            break;
        }
        h.swap(i as usize, j as usize);
        i = j;
    }
    i > i0
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Ints(Vec<i64>);

    impl Interface for Ints {
        fn len(&self) -> usize {
            self.0.len()
        }
        fn less(&self, i: usize, j: usize) -> bool {
            self.0[i] < self.0[j]
        }
        fn swap(&mut self, i: usize, j: usize) {
            self.0.swap(i, j);
        }
        fn pop(&mut self) {
            self.0.pop();
        }
    }

    /// Go's `heap_test.go` `verify`: every child is not less than its parent.
    fn verify(h: &Ints) {
        for i in 0..h.len() {
            for c in [2 * i + 1, 2 * i + 2] {
                if c < h.len() {
                    assert!(!h.less(c, i), "heap invariant broken at {i}/{c}: {:?}", h.0);
                }
            }
        }
    }

    #[test]
    fn init_pop_fix_keep_the_heap() {
        let mut h = Ints(vec![20, 3, 17, 8, 8, 1, 30, 0, 12, 5]);
        init(&mut h);
        verify(&h);
        let mut out = Vec::new();
        while h.len() > 0 {
            let min = h.0[0];
            pop(&mut h);
            verify(&h);
            out.push(min);
        }
        assert_eq!(out, vec![0, 1, 3, 5, 8, 8, 12, 17, 20, 30]);

        let mut h = Ints((0..20).rev().collect());
        init(&mut h);
        for i in [0usize, 5, 13] {
            h.0[i] = 100 + i as i64;
            fix(&mut h, i);
            verify(&h);
        }
        h.0[7] = -1;
        fix(&mut h, 7);
        verify(&h);
        assert_eq!(h.0[0], -1);
    }
}
