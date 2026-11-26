//! go-file: pkg/util/selection/selection.go
//!
//! Introselect (`Select`, block D00.3): quickselect with a randomized pivot that falls back
//! to deterministic median-of-medians when recursion depth runs out. Used by
//! `APPROX_PERCENTILE` (`crates/executor/src/aggfuncs/func_percentile.rs`).
//!
//! Per D12.4 spec §5.6 and §9.1 item 7, the random source is passed in as a parameter so
//! production callers use true randomness while unit tests and `govec` replay inject a fixed
//! seed.

/// Interface is alias of `sort.Interface`.
// glue: Go type Interface = sort.Interface
pub trait Interface {
    /// Number of elements in the collection.
    // glue: sort.Interface.Len
    fn len(&self) -> isize;
    /// Whether the collection is empty.
    // glue: Rust clippy::len_without_is_empty companion to sort.Interface.Len
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Reports whether the element with index `i` must sort before the element with index `j`.
    // glue: sort.Interface.Less
    fn less(&self, i: isize, j: isize) -> bool;
    /// Swaps the elements with indexes `i` and `j`.
    // glue: sort.Interface.Swap
    fn swap(&mut self, i: isize, j: isize);
}

/// Select performs introselect algorithm on `data` and returns the index of the `k`-th
/// smallest value (1-based `k`), or `-1` if `data` is empty.
///
/// `next_rand` returns a non-negative random integer (corresponding to Go's `rand.Int()`).
// go: Select L2 (random source passed as parameter per spec D12.4 §5.6, §9.1 item 7)
pub fn select<T: Interface + ?Sized, R: FnMut() -> usize>(
    data: &mut T,
    k: isize,
    next_rand: &mut R,
) -> isize {
    if data.len() > 0 {
        return introselect(data, 0, data.len() - 1, k - 1, 6, next_rand);
    }
    -1
}

/// introselect will perform quickselect at beginning, and switch to linear-time algorithm if
/// it recurses too many times.
/// Source paper: http://www.cs.rpi.edu/~musser/gp/introsort.ps
// go: introselect L2 (random source threaded to random_pivot)
fn introselect<T: Interface + ?Sized, R: FnMut() -> usize>(
    data: &mut T,
    left: isize,
    right: isize,
    k: isize,
    depth: isize,
    next_rand: &mut R,
) -> isize {
    if left == right {
        return left;
    }
    if depth <= 0 {
        // Use median of medians algorithm (linear-time selection) when recursing too many times.
        return median_of_medians(data, left, right, k);
    }
    // TODO: use a better pivot function
    let pivot_index = random_pivot(data, left, right, next_rand);
    let pivot_index = partition(data, left, right, pivot_index);
    if k == pivot_index {
        k
    } else if k < pivot_index {
        introselect(data, left, pivot_index - 1, k, depth - 1, next_rand)
    } else {
        introselect(data, pivot_index + 1, right, k, depth - 1, next_rand)
    }
}

/// quickselect is used in test for comparison.
// go: quickselect L2 (random source threaded to random_pivot)
#[cfg(test)]
fn quickselect<T: Interface + ?Sized, R: FnMut() -> usize>(
    data: &mut T,
    left: isize,
    right: isize,
    k: isize,
    next_rand: &mut R,
) -> isize {
    if left == right {
        return left;
    }
    let pivot_index = random_pivot(data, left, right, next_rand);
    let pivot_index = partition(data, left, right, pivot_index);
    if k == pivot_index {
        k
    } else if k < pivot_index {
        quickselect(data, left, pivot_index - 1, k, next_rand)
    } else {
        quickselect(data, pivot_index + 1, right, k, next_rand)
    }
}

// go: medianOfMedians L1
fn median_of_medians<T: Interface + ?Sized>(
    data: &mut T,
    left: isize,
    right: isize,
    k: isize,
) -> isize {
    if left == right {
        return left;
    }
    let pivot_index = median_of_medians_pivot(data, left, right);
    let pivot_index = partition_intro(data, left, right, pivot_index, k);
    if k == pivot_index {
        k
    } else if k < pivot_index {
        median_of_medians(data, left, pivot_index - 1, k)
    } else {
        median_of_medians(data, pivot_index + 1, right, k)
    }
}

// go: randomPivot L2 (uses injected random source instead of package-global math/rand)
fn random_pivot<T: Interface + ?Sized, R: FnMut() -> usize>(
    _data: &T,
    left: isize,
    right: isize,
    next_rand: &mut R,
) -> isize {
    let span = (right - left + 1) as usize;
    left + (next_rand() % span) as isize
}

// go: medianOfMediansPivot L1
fn median_of_medians_pivot<T: Interface + ?Sized>(
    data: &mut T,
    left: isize,
    right: isize,
) -> isize {
    if right - left < 5 {
        return partition5(data, left, right);
    }
    let mut i = left;
    while i <= right {
        let mut sub_right = i + 4;
        if sub_right > right {
            sub_right = right;
        }
        let median5 = partition5(data, i, sub_right);
        data.swap(median5, left + (i - left) / 5);
        i += 5;
    }
    let mid = (right - left) / 10 + left + 1;
    median_of_medians(data, left, left + (right - left) / 5, mid)
}

// go: partition L1
fn partition<T: Interface + ?Sized>(
    data: &mut T,
    left: isize,
    right: isize,
    pivot_index: isize,
) -> isize {
    data.swap(pivot_index, right);
    let mut store_index = left;
    for i in left..right {
        if data.less(i, right) {
            data.swap(store_index, i);
            store_index += 1;
        }
    }
    data.swap(right, store_index);
    store_index
}

// go: partitionIntro L1
fn partition_intro<T: Interface + ?Sized>(
    data: &mut T,
    left: isize,
    right: isize,
    pivot_index: isize,
    k: isize,
) -> isize {
    data.swap(pivot_index, right);
    let mut store_index = left;
    // Move all elements smaller than pivot to left side
    for i in left..right {
        if data.less(i, right) {
            data.swap(store_index, i);
            store_index += 1;
        }
    }
    let mut store_index_eq = store_index;
    // Move all elements equal to pivot right after
    for i in store_index..right {
        // data[i] == data[right]
        if !data.less(i, right) && !data.less(right, i) {
            data.swap(store_index_eq, i);
            store_index_eq += 1;
        }
    }
    // Move pivot to final place
    data.swap(right, store_index_eq);
    if k < store_index {
        return store_index;
    }
    if k <= store_index_eq {
        return k;
    }
    store_index_eq
}

// go: partition5 L1
fn partition5<T: Interface + ?Sized>(data: &mut T, left: isize, right: isize) -> isize {
    let mut i = left + 1;
    while i <= right {
        let mut j = i;
        while j > left && data.less(j, j - 1) {
            data.swap(j, j - 1);
            j -= 1;
        }
        i += 1;
    }
    (left + right) / 2
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestSlice(Vec<i64>);

    impl Interface for TestSlice {
        fn len(&self) -> isize {
            self.0.len() as isize
        }
        fn less(&self, i: isize, j: isize) -> bool {
            self.0[i as usize] < self.0[j as usize]
        }
        fn swap(&mut self, i: isize, j: isize) {
            self.0.swap(i as usize, j as usize);
        }
    }

    fn seeded_rng(mut state: u64) -> impl FnMut() -> usize {
        move || {
            state = state.wrapping_add(0x9e3779b97f4a7c15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
            ((z ^ (z >> 31)) >> 1) as usize
        }
    }

    #[test]
    fn test_selection() {
        let mut rng = seeded_rng(1);
        let mut empty = TestSlice(vec![]);
        assert_eq!(select(&mut empty, 1, &mut rng), -1);

        let mut data = TestSlice(vec![1, 2, 3, 4, 5]);
        let index = select(&mut data, 3, &mut rng);
        assert_eq!(data.0[index as usize], 3);
    }

    #[test]
    fn test_selection_with_duplicate() {
        let mut rng = seeded_rng(2);
        let mut data = TestSlice(vec![1, 2, 3, 3, 5]);
        let index = select(&mut data, 3, &mut rng);
        assert_eq!(data.0[index as usize], 3);
        let index = select(&mut data, 5, &mut rng);
        assert_eq!(data.0[index as usize], 5);
    }

    #[test]
    fn test_selection_with_random_case() {
        let mut data_rng = seeded_rng(42);
        let n = 50_000usize;
        let raw: Vec<i64> = (0..n).map(|_| (data_rng() % 100) as i64).collect();
        let mut data = TestSlice(raw.clone());
        let mut rng = seeded_rng(123);
        let index = select(&mut data, (n / 2) as isize, &mut rng);
        let actual = data.0[index as usize];

        let mut sorted = raw.clone();
        sorted.sort();
        assert_eq!(actual, sorted[n / 2 - 1]);

        // Also cross-check quickselect on the same input.
        let mut qdata = TestSlice(raw);
        let mut qrng = seeded_rng(123);
        let qidx = quickselect(
            &mut qdata,
            0,
            (n - 1) as isize,
            (n / 2 - 1) as isize,
            &mut qrng,
        );
        assert_eq!(qdata.0[qidx as usize], sorted[n / 2 - 1]);
    }

    #[test]
    fn test_selection_with_serial_case_and_median_of_medians() {
        let n = 10_000usize;
        let mut raw: Vec<i64> = (0..n as i64).rev().collect();
        let mut data = TestSlice(raw.clone());
        // Adversarial pivot generator (always picks index 0) forces depth <= 0 -> medianOfMedians.
        let mut worst_rng = || 0usize;
        let index = select(&mut data, (n / 2) as isize, &mut worst_rng);
        let actual = data.0[index as usize];
        raw.sort();
        assert_eq!(actual, raw[n / 2 - 1]);

        // Also test median_of_medians directly on all-equal and small slices.
        let mut dups = TestSlice(vec![7; 50]);
        let idx = median_of_medians(&mut dups, 0, 49, 25);
        assert_eq!(dups.0[idx as usize], 7);
    }
}
