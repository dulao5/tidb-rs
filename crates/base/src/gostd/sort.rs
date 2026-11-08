//! Go's `sort.Sort` (`sort/zsortinterface.go`, `sort/sort.go`): pattern-defeating quicksort,
//! not stable. Ported so that the order of equal elements and the sequence of `Less` calls
//! match Go: callers whose `Less` has side effects (for example `types.SortDatums`, which
//! records the first comparison error) or whose order is not total see Go's result.
//!
//! The Go `Interface` is a slice with a `less(&a, &b)` closure; `Swap` swaps elements.

/// The data and the `Less` method of a `sort.Interface` over a slice.
// glue: sort.Interface over a slice
struct Data<'a, T, F: FnMut(&T, &T) -> bool> {
    s: &'a mut [T],
    less: F,
}

impl<T, F: FnMut(&T, &T) -> bool> Data<'_, T, F> {
    // glue: Interface.Less
    #[inline]
    fn less(&mut self, i: usize, j: usize) -> bool {
        (self.less)(&self.s[i], &self.s[j])
    }

    // glue: Interface.Swap
    #[inline]
    fn swap(&mut self, i: usize, j: usize) {
        self.s.swap(i, j);
    }
}

/// sortedHint is a hint for pdqsort when choosing the pivot.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SortedHint {
    Unknown,
    Increasing,
    Decreasing,
}

/// xorshift paper: https://www.jstatsoft.org/article/view/v008i14/xorshift.pdf
struct Xorshift(u64);

impl Xorshift {
    // go-std: sort.xorshift.Next L1
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// `bits.Len(uint(x))`.
// go-std: math/bits.Len L1
fn bits_len(x: usize) -> u32 {
    usize::BITS - x.leading_zeros()
}

// go-std: sort.nextPowerOfTwo L1
fn next_power_of_two(length: usize) -> usize {
    let shift = bits_len(length);
    1usize << shift
}

/// Sort sorts data in ascending order as determined by `less`. It makes one call to
/// data.Len to determine n and O(n*log(n)) calls to data.Less and data.Swap. The sort is
/// not guaranteed to be stable.
// go-std: sort.Sort L1
pub fn sort<T>(s: &mut [T], less: impl FnMut(&T, &T) -> bool) {
    let n = s.len();
    if n <= 1 {
        return;
    }
    let limit = bits_len(n) as isize;
    let mut data = Data { s, less };
    pdqsort(&mut data, 0, n, limit);
}

/// insertionSort sorts data[a:b] using insertion sort.
// go-std: sort.insertionSort L1
fn insertion_sort<T, F: FnMut(&T, &T) -> bool>(data: &mut Data<'_, T, F>, a: usize, b: usize) {
    for i in a + 1..b {
        let mut j = i;
        while j > a && data.less(j, j - 1) {
            data.swap(j, j - 1);
            j -= 1;
        }
    }
}

/// siftDown implements the heap property on data[lo:hi].
/// first is an offset into the array where the root of the heap lies.
// go-std: sort.siftDown L1
fn sift_down<T, F: FnMut(&T, &T) -> bool>(
    data: &mut Data<'_, T, F>,
    lo: usize,
    hi: usize,
    first: usize,
) {
    let mut root = lo;
    loop {
        let mut child = 2 * root + 1;
        if child >= hi {
            break;
        }
        if child + 1 < hi && data.less(first + child, first + child + 1) {
            child += 1;
        }
        if !data.less(first + root, first + child) {
            return;
        }
        data.swap(first + root, first + child);
        root = child;
    }
}

// go-std: sort.heapSort L1
fn heap_sort<T, F: FnMut(&T, &T) -> bool>(data: &mut Data<'_, T, F>, a: usize, b: usize) {
    let first = a;
    let lo = 0;
    let hi = b - a;

    // Build heap with greatest element at top.
    let mut i = (hi as isize - 1) / 2;
    while i >= 0 {
        sift_down(data, i as usize, hi, first);
        i -= 1;
    }

    // Pop elements, largest first, into end of data.
    let mut i = hi as isize - 1;
    while i >= 0 {
        data.swap(first, first + i as usize);
        sift_down(data, lo, i as usize, first);
        i -= 1;
    }
}

/// pdqsort sorts data[a:b].
/// The algorithm based on pattern-defeating quicksort(pdqsort), but without the optimizations from BlockQuicksort.
/// pdqsort paper: https://arxiv.org/pdf/2106.05123.pdf
/// limit is the number of allowed bad (very unbalanced) pivots before falling back to heapsort.
// go-std: sort.pdqsort L1
fn pdqsort<T, F: FnMut(&T, &T) -> bool>(
    data: &mut Data<'_, T, F>,
    mut a: usize,
    mut b: usize,
    mut limit: isize,
) {
    const MAX_INSERTION: usize = 12;

    let mut was_balanced = true; // whether the last partitioning was reasonably balanced
    let mut was_partitioned = true; // whether the slice was already partitioned

    loop {
        let length = b - a;

        if length <= MAX_INSERTION {
            insertion_sort(data, a, b);
            return;
        }

        // Fall back to heapsort if too many bad choices were made.
        if limit == 0 {
            heap_sort(data, a, b);
            return;
        }

        // If the last partitioning was imbalanced, we need to breaking patterns.
        if !was_balanced {
            break_patterns(data, a, b);
            limit -= 1;
        }

        let (mut pivot, mut hint) = choose_pivot(data, a, b);
        if hint == SortedHint::Decreasing {
            reverse_range(data, a, b);
            // The chosen pivot was pivot-a elements after the start of the array.
            // After reversing it is pivot-a elements before the end of the array.
            // The idea came from Rust's implementation.
            pivot = (b - 1) - (pivot - a);
            hint = SortedHint::Increasing;
        }

        // The slice is likely already sorted.
        if was_balanced
            && was_partitioned
            && hint == SortedHint::Increasing
            && partial_insertion_sort(data, a, b)
        {
            return;
        }

        // Probably the slice contains many duplicate elements, partition the slice into
        // elements equal to and elements greater than the pivot.
        if a > 0 && !data.less(a - 1, pivot) {
            let mid = partition_equal(data, a, b, pivot);
            a = mid;
            continue;
        }

        let (mid, already_partitioned) = partition(data, a, b, pivot);
        was_partitioned = already_partitioned;

        let (left_len, right_len) = (mid - a, b - mid);
        let balance_threshold = length / 8;
        if left_len < right_len {
            was_balanced = left_len >= balance_threshold;
            pdqsort(data, a, mid, limit);
            a = mid + 1;
        } else {
            was_balanced = right_len >= balance_threshold;
            pdqsort(data, mid + 1, b, limit);
            b = mid;
        }
    }
}

/// partition does one quicksort partition.
/// Let p = data[pivot]
/// Moves elements in data[a:b] around, so that data[i]<p and data[j]>=p for i<newpivot and j>newpivot.
/// On return, data[newpivot] = p
// go-std: sort.partition L1
fn partition<T, F: FnMut(&T, &T) -> bool>(
    data: &mut Data<'_, T, F>,
    a: usize,
    b: usize,
    pivot: usize,
) -> (usize, bool) {
    data.swap(a, pivot);
    // i and j are inclusive of the elements remaining to be partitioned (isize: j may reach a).
    let (mut i, mut j) = (a as isize + 1, b as isize - 1);

    while i <= j && data.less(i as usize, a) {
        i += 1;
    }
    while i <= j && !data.less(j as usize, a) {
        j -= 1;
    }
    if i > j {
        data.swap(j as usize, a);
        return (j as usize, true);
    }
    data.swap(i as usize, j as usize);
    i += 1;
    j -= 1;

    loop {
        while i <= j && data.less(i as usize, a) {
            i += 1;
        }
        while i <= j && !data.less(j as usize, a) {
            j -= 1;
        }
        if i > j {
            break;
        }
        data.swap(i as usize, j as usize);
        i += 1;
        j -= 1;
    }
    data.swap(j as usize, a);
    (j as usize, false)
}

/// partitionEqual partitions data[a:b] into elements equal to data[pivot] followed by elements greater than data[pivot].
/// It assumed that data[a:b] does not contain elements smaller than the data[pivot].
// go-std: sort.partitionEqual L1
fn partition_equal<T, F: FnMut(&T, &T) -> bool>(
    data: &mut Data<'_, T, F>,
    a: usize,
    b: usize,
    pivot: usize,
) -> usize {
    data.swap(a, pivot);
    let (mut i, mut j) = (a as isize + 1, b as isize - 1);

    loop {
        while i <= j && !data.less(a, i as usize) {
            i += 1;
        }
        while i <= j && data.less(a, j as usize) {
            j -= 1;
        }
        if i > j {
            break;
        }
        data.swap(i as usize, j as usize);
        i += 1;
        j -= 1;
    }
    i as usize
}

/// partialInsertionSort partially sorts a slice, returns true if the slice is sorted at the end.
// go-std: sort.partialInsertionSort L1
fn partial_insertion_sort<T, F: FnMut(&T, &T) -> bool>(
    data: &mut Data<'_, T, F>,
    a: usize,
    b: usize,
) -> bool {
    const MAX_STEPS: usize = 5; // maximum number of adjacent out-of-order pairs that will get shifted
    const SHORTEST_SHIFTING: usize = 50; // don't shift any elements on short arrays
    let mut i = a + 1;
    for _ in 0..MAX_STEPS {
        while i < b && !data.less(i, i - 1) {
            i += 1;
        }

        if i == b {
            return true;
        }

        if b - a < SHORTEST_SHIFTING {
            return false;
        }

        data.swap(i, i - 1);

        // Shift the smaller one to the left.
        if i - a >= 2 {
            let mut j = i - 1;
            while j >= 1 {
                if !data.less(j, j - 1) {
                    break;
                }
                data.swap(j, j - 1);
                j -= 1;
            }
        }
        // Shift the greater one to the right.
        if b - i >= 2 {
            for j in i + 1..b {
                if !data.less(j, j - 1) {
                    break;
                }
                data.swap(j, j - 1);
            }
        }
    }
    false
}

/// breakPatterns scatters some elements around in an attempt to break some patterns
/// that might cause imbalanced partitions in quicksort.
// go-std: sort.breakPatterns L1
fn break_patterns<T, F: FnMut(&T, &T) -> bool>(data: &mut Data<'_, T, F>, a: usize, b: usize) {
    let length = b - a;
    if length >= 8 {
        let mut random = Xorshift(length as u64);
        let modulus = next_power_of_two(length);

        let mut idx = a + (length / 4) * 2 - 1;
        while idx <= a + (length / 4) * 2 + 1 {
            let mut other = (random.next() as usize) & (modulus - 1);
            if other >= length {
                other -= length;
            }
            data.swap(idx, a + other);
            idx += 1;
        }
    }
}

/// choosePivot chooses a pivot in data[a:b].
/// [0,8): chooses a static pivot.
/// [8,shortestNinther): uses the simple median-of-three method.
/// [shortestNinther,∞): uses the Tukey ninther method.
// go-std: sort.choosePivot L1
fn choose_pivot<T, F: FnMut(&T, &T) -> bool>(
    data: &mut Data<'_, T, F>,
    a: usize,
    b: usize,
) -> (usize, SortedHint) {
    const SHORTEST_NINTHER: usize = 50;
    const MAX_SWAPS: usize = 4 * 3;

    let l = b - a;

    let mut swaps = 0usize;
    let mut i = a + l / 4;
    let mut j = a + l / 4 * 2;
    let mut k = a + l / 4 * 3;

    if l >= 8 {
        if l >= SHORTEST_NINTHER {
            // Tukey ninther method, the idea came from Rust's implementation.
            i = median_adjacent(data, i, &mut swaps);
            j = median_adjacent(data, j, &mut swaps);
            k = median_adjacent(data, k, &mut swaps);
        }
        // Find the median among i, j, k and stores it into j.
        j = median(data, i, j, k, &mut swaps);
    }

    match swaps {
        0 => (j, SortedHint::Increasing),
        MAX_SWAPS => (j, SortedHint::Decreasing),
        _ => (j, SortedHint::Unknown),
    }
}

/// order2 returns x,y where data[x] <= data[y], where x,y=a,b or x,y=b,a.
// go-std: sort.order2 L1
fn order2<T, F: FnMut(&T, &T) -> bool>(
    data: &mut Data<'_, T, F>,
    a: usize,
    b: usize,
    swaps: &mut usize,
) -> (usize, usize) {
    if data.less(b, a) {
        *swaps += 1;
        return (b, a);
    }
    (a, b)
}

/// median returns x where data[x] is the median of data[a],data[b],data[c], where x is a, b, or c.
// go-std: sort.median L1
fn median<T, F: FnMut(&T, &T) -> bool>(
    data: &mut Data<'_, T, F>,
    a: usize,
    b: usize,
    c: usize,
    swaps: &mut usize,
) -> usize {
    let (a, b) = order2(data, a, b, swaps);
    let (b, c) = order2(data, b, c, swaps);
    let _ = c;
    let (_a, b) = order2(data, a, b, swaps);
    b
}

/// medianAdjacent finds the median of data[a - 1], data[a], data[a + 1] and stores the index into a.
// go-std: sort.medianAdjacent L1
fn median_adjacent<T, F: FnMut(&T, &T) -> bool>(
    data: &mut Data<'_, T, F>,
    a: usize,
    swaps: &mut usize,
) -> usize {
    median(data, a - 1, a, a + 1, swaps)
}

// go-std: sort.reverseRange L1
fn reverse_range<T, F: FnMut(&T, &T) -> bool>(data: &mut Data<'_, T, F>, a: usize, b: usize) {
    let mut i = a;
    let mut j = b - 1;
    while i < j {
        data.swap(i, j);
        i += 1;
        j -= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorts_like_a_sort() {
        let mut rng = Xorshift(88172645463325252);
        for n in [0usize, 1, 2, 7, 12, 13, 49, 50, 51, 200, 1000] {
            let mut v: Vec<u32> = (0..n).map(|_| (rng.next() % 97) as u32).collect();
            let mut w = v.clone();
            sort(&mut v, |a, b| a < b);
            w.sort();
            assert_eq!(v, w);
        }
    }
}
