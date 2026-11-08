//! The Go runtime's slice growth (`runtime.growslice`, `runtime.roundupsize`, Go 1.25.12).
//!
//! Go code whose observable results depend on slice capacities (memory accounting with
//! `cap(...)`, bytes exposed by reslicing) needs the capacity `append` would choose. Moved
//! here from `tidb-chunk`'s `goslice` (D01.7) when `tidb-kv` (D07.0) needed it too.

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
pub fn roundupsize(size: usize) -> usize {
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
/// types; the chunk package appends to 1- and 8-byte elements, pkg/kv to bytes).
// go-std: runtime.growslice L2 (capacity computation only; the copy is GoSlice::grow)
pub fn growslice_cap(new_len: usize, old_cap: usize, elem: usize) -> usize {
    let newcap = nextslicecap(new_len, old_cap);
    roundupsize(newcap * elem) / elem
}
