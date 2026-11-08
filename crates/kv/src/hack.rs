//! go-file: pkg/util/hack/map_abi.go
//!
//! `hack.MemAwareMap`, the part `pkg/kv` uses (D00.3 tool code, ported with its first user).
//!
//! Go's `MemAwareMap` reads the Go 1.25 swiss-table header of the map it wraps (`Used`,
//! the directory length) and estimates the map's bytes from the group size of its key and
//! element types. Its accounting depends only on the element count, the group size and the
//! map's initial state, so the port computes the same numbers without a Go map:
//! - the group size comes from the Go layout of `K` and `V` ([`GoLayout`]): a group is an
//!   8-byte control word and 8 slots of `struct { key K; elem V }`, a key or element larger
//!   than 128 bytes being stored indirectly (Go 1.25 `internal/runtime/maps`);
//! - `Init` is only called on empty maps created with no size hint (all uses in TiDB), whose
//!   Go size is the map header plus one group (`swissMap.Size` with `dirLen == 0`);
//! - `Used` is the element count.
//!
//! The Rust `HashMap` itself is accounted by nobody (the numbers are Go's, as the trackers
//! that consume them expect).

// Go identifiers for the constants (M-GONAME).
#![allow(non_upper_case_globals)]

use std::collections::HashMap;
use std::hash::Hash;

/// The Go type layout of a key or element type, for the map accounting.
pub trait GoLayout {
    /// `unsafe.Sizeof` of the Go type.
    const GO_SIZE: u64;
    /// `unsafe.Alignof` of the Go type.
    const GO_ALIGN: u64;
}

impl GoLayout for i64 {
    const GO_SIZE: u64 = 8;
    const GO_ALIGN: u64 = 8;
}

impl GoLayout for u64 {
    const GO_SIZE: u64 = 8;
    const GO_ALIGN: u64 = 8;
}

impl GoLayout for bool {
    const GO_SIZE: u64 = 1;
    const GO_ALIGN: u64 = 1;
}

/// A byte-string key is a Go `string` (pointer and length).
impl GoLayout for Vec<u8> {
    const GO_SIZE: u64 = 16;
    const GO_ALIGN: u64 = 8;
}

/// A Go `struct{}`.
impl GoLayout for () {
    const GO_SIZE: u64 = 0;
    const GO_ALIGN: u64 = 1;
}

const maxTableCapacity: u64 = 1024;
const swissMapGroupSlots: u64 = 8;
/// `unsafe.Sizeof(swissMap{})`: Used, seed, dirPtr, dirLen, four bytes, clearSeq.
const swissMapSize: u64 = 48;
/// Keys and elements larger than this are stored indirectly (`abi.SwissMapMaxKeyBytes`,
/// `abi.SwissMapMaxElemBytes`).
const swissMapMaxSlotBytes: u64 = 128;

/// `abi.SwissMapType.GroupSize` for `map[K]V` (the compiler's `cmd/compile/internal/
/// reflectdata.SwissMapGroupType`).
// go-std: cmd/compile/internal/reflectdata.SwissMapGroupType L2 (size computation only)
pub(crate) fn group_size<K: GoLayout, V: GoLayout>() -> u64 {
    let (ks, ka) = if K::GO_SIZE > swissMapMaxSlotBytes {
        (8, 8)
    } else {
        (K::GO_SIZE, K::GO_ALIGN)
    };
    let (vs, va) = if V::GO_SIZE > swissMapMaxSlotBytes {
        (8, 8)
    } else {
        (V::GO_SIZE, V::GO_ALIGN)
    };
    let align = ka.max(va).max(1);
    let elem_off = ks.div_ceil(va.max(1)) * va.max(1);
    let slot = (elem_off + vs).div_ceil(align) * align;
    // The control word is a uint64; the group is aligned to max(8, slot alignment).
    let group_align = align.max(8);
    (8 + swissMapGroupSlots * slot).div_ceil(group_align) * group_align
}

// go: approxSize L1
fn approx_size(group_size: u64, max_len: u64) -> u64 {
    // 204 can fit the `split`/`rehash` behavior of different kinds of swisstable
    const ratio: u64 = 204;
    group_size * max_len * ratio / 1000
}

/// MemAwareMap is a map with memory usage tracking.
#[derive(Debug)]
pub struct MemAwareMap<K, V> {
    pub m: HashMap<K, V>,
    group_size: u64,
    next_checkpoint: u64, // every `maxTableCapacity` increase in Used
    pub bytes: u64,
}

impl<K, V> Default for MemAwareMap<K, V> {
    // glue: Go zero value of MemAwareMap (Init must be called)
    fn default() -> Self {
        MemAwareMap {
            m: HashMap::new(),
            group_size: 0,
            next_checkpoint: 0,
            bytes: 0,
        }
    }
}

#[allow(clippy::len_without_is_empty)] // Go has Len and no IsEmpty
impl<K: Eq + Hash + GoLayout, V: GoLayout> MemAwareMap<K, V> {
    /// Count returns the number of elements in the map.
    // go: MemAwareMap.Count L1
    pub fn count(&self) -> isize {
        self.m.len() as isize
    }

    /// Set sets the value for the key in the map and returns the memory delta.
    // go: MemAwareMap.Set L1
    pub fn set(&mut self, key: K, value: V) -> i64 {
        let mut delta_bytes = 0i64;
        self.m.insert(key, value);
        let used = self.m.len() as u64;
        if used >= self.next_checkpoint {
            let new_bytes = self.bytes.max(approx_size(self.group_size, used));
            delta_bytes = new_bytes as i64 - self.bytes as i64;
            self.bytes = new_bytes;
            self.next_checkpoint = used.min(maxTableCapacity) + used;
        }
        delta_bytes
    }

    /// Init initializes the MemAwareMap with the given map and returns the initial memory
    /// size. The input map should NOT be nil.
    ///
    /// Only an empty map with no size hint is supported (module docs).
    // go: MemAwareMap.Init L2 (empty, unhinted maps only: their Go size is a formula)
    pub fn init(&mut self, v: HashMap<K, V>) -> i64 {
        assert!(
            v.is_empty(),
            "MemAwareMap.Init: only empty maps are supported"
        );
        // A size hint changes Go's initial layout (more groups or a directory), which the
        // formula above does not model; every production caller passes `make(map)`.
        debug_assert_eq!(
            v.capacity(),
            0,
            "MemAwareMap.Init: only maps created without a size hint are supported"
        );
        self.m = v;
        let used = self.m.len() as u64;

        self.group_size = group_size::<K, V>();
        // swissMap.Size with dirLen == 0: the header and the single group.
        self.bytes = swissMapSize + self.group_size;
        if used <= swissMapGroupSlots {
            self.next_checkpoint = swissMapGroupSlots * 2;
        } else {
            self.next_checkpoint = used.min(maxTableCapacity) + used;
        }
        self.bytes as i64
    }

    /// Get the value of the key.
    // go: MemAwareMap.Get L1
    pub fn get(&self, k: &K) -> Option<&V> {
        self.m.get(k)
    }

    /// Len returns the number of elements in the map.
    // go: MemAwareMap.Len L1
    pub fn len(&self) -> isize {
        self.m.len() as isize
    }
}

/// NewMemAwareMap creates a new MemAwareMap with the given initial capacity.
// go: NewMemAwareMap L2 (capacity 0 only, see MemAwareMap.init)
pub fn new_mem_aware_map<K: Eq + Hash + GoLayout, V: GoLayout>(
    capacity: isize,
) -> MemAwareMap<K, V> {
    assert!(
        capacity == 0,
        "NewMemAwareMap: only capacity 0 is supported"
    );
    let mut m = MemAwareMap::default();
    m.init(HashMap::new());
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_sizes() {
        // map[int64]int64: 8 + 8 * 16.
        assert_eq!(group_size::<i64, i64>(), 136);
        // map[string]struct{}: 8 + 8 * 16.
        assert_eq!(group_size::<Vec<u8>, ()>(), 136);
        // map[int64]bool: slot {int64, bool} = 16.
        assert_eq!(group_size::<i64, bool>(), 136);
    }
}
