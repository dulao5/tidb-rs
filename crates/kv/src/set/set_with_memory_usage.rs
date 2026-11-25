//! go-file: pkg/util/set/set_with_memory_usage.go
//!
//! The sets embed `hack.MemAwareMap` (M-SELF: a field and forwarding methods) and return
//! Go's estimated memory delta. Each set takes the allocator of its table (A1 3.3): the
//! aggregate functions pass a `TrackedAlloc` so that the set is accounted by its real
//! allocations and ignore the returned delta (A1, spec D12.4 §5.3).
//!
//! Go's constructors are variadic (`ss ...string`) and size the map with `len(ss)`; every
//! caller passes no argument, and `MemAwareMap.Init` only models an unhinted empty map, so
//! the ports take no elements (L2).
//!
//! The keys of the string set are boxed in the set's allocator, so that a set in a
//! `TrackedAlloc` accounts the key bytes too (Go's callers add `len(key)` to memDelta, e.g.
//! `func_sum.go:400`). `Insert` therefore takes the key bytes and copies them (L2).

use std::sync::Arc;

use allocator_api2::alloc::Allocator;
use allocator_api2::alloc::Global;
use allocator_api2::boxed::Box;
use allocator_api2::vec::Vec;
use tidb_memory::Tracker;

use super::float64_set::Float64Key;
use super::float64_set::make_float64_set;
use super::int_set::make_int64_set;
use super::string_set::make_string_set;
use crate::hack::MemAwareMap;

/// StringSetWithMemoryUsage is a string set with memory usage.
#[derive(Debug, Default)]
pub struct StringSetWithMemoryUsage<A: Allocator + Clone = Global> {
    pub map: MemAwareMap<Box<[u8], A>, (), A>,

    // For tracking large memory usage in time.
    // If tracker is non-nil, memDelta will track immediately and reset to 0. Otherwise,
    // memDelta will return and lazy track.
    tracker: Option<Arc<Tracker>>,
}

/// NewStringSetWithMemoryUsage builds a string set.
// go: NewStringSetWithMemoryUsage L2 (no elements, module docs; the allocator is A1's)
pub fn new_string_set_with_memory_usage<A: Allocator + Clone>(
    alloc: A,
) -> (StringSetWithMemoryUsage<A>, i64) {
    let set = make_string_set(alloc.clone());
    let mut set_with_memory_usage = StringSetWithMemoryUsage {
        map: MemAwareMap::zero_in(alloc),
        tracker: None,
    };
    set_with_memory_usage.map.init(set);
    let bytes = set_with_memory_usage.map.bytes as i64;
    (set_with_memory_usage, bytes)
}

impl<A: Allocator + Clone> StringSetWithMemoryUsage<A> {
    /// Insert inserts `val` into `s` and return memDelta.
    // go: StringSetWithMemoryUsage.Insert L2 (Consume → record: Insert cannot fail, the
    // actions run at the caller's next checkpoint, A1 3.3; the key is copied into the set's
    // allocator, module docs)
    pub fn insert(&mut self, val: &[u8]) -> i64 {
        let mut key = Vec::with_capacity_in(val.len(), self.map.m.allocator().clone());
        key.extend_from_slice(val);
        let mut delta = self.map.set(key.into_boxed_slice(), ());
        if delta != 0
            && let Some(t) = &self.tracker
        {
            t.record(delta);
            delta = 0;
        }
        delta
    }

    /// SetTracker sets memory tracker for StringSetWithMemoryUsage
    // go: StringSetWithMemoryUsage.SetTracker L1
    pub fn set_tracker(&mut self, t: Option<Arc<Tracker>>) {
        self.tracker = t;
    }

    /// `Exist` of the embedded map (M-SELF).
    // glue: forwarding to the embedded MemAwareMap (M-SELF)
    pub fn exist(&self, val: &[u8]) -> bool {
        self.map.m.contains_key(val)
    }

    /// `Count` of the embedded map (M-SELF).
    // glue: forwarding to the embedded MemAwareMap (M-SELF)
    pub fn count(&self) -> isize {
        self.map.count()
    }
}

/// Float64SetWithMemoryUsage is a float64 set with memory usage.
#[derive(Debug, Default)]
pub struct Float64SetWithMemoryUsage<A: Allocator + Clone = Global> {
    pub map: MemAwareMap<Float64Key, (), A>,
}

/// NewFloat64SetWithMemoryUsage builds a float64 set.
// go: NewFloat64SetWithMemoryUsage L2 (no elements, module docs; the allocator is A1's)
pub fn new_float64_set_with_memory_usage<A: Allocator + Clone>(
    alloc: A,
) -> (Float64SetWithMemoryUsage<A>, i64) {
    let set = make_float64_set(alloc.clone());
    let mut set_with_memory_usage = Float64SetWithMemoryUsage {
        map: MemAwareMap::zero_in(alloc),
    };
    set_with_memory_usage.map.init(set);
    let bytes = set_with_memory_usage.map.bytes as i64;
    (set_with_memory_usage, bytes)
}

impl<A: Allocator + Clone> Float64SetWithMemoryUsage<A> {
    /// Insert inserts `val` into `s` and return memDelta.
    // go: Float64SetWithMemoryUsage.Insert L1
    pub fn insert(&mut self, val: f64) -> i64 {
        self.map.set(Float64Key::for_insert(val), ())
    }

    /// `Exist` of the embedded map (M-SELF), with Go's float key equality.
    // glue: forwarding to the embedded MemAwareMap (M-SELF)
    pub fn exist(&self, val: f64) -> bool {
        Float64Key::for_lookup(val).is_some_and(|k| self.map.m.contains_key(&k))
    }

    /// `Count` of the embedded map (M-SELF).
    // glue: forwarding to the embedded MemAwareMap (M-SELF)
    pub fn count(&self) -> isize {
        self.map.count()
    }
}

/// Int64SetWithMemoryUsage is a int set with memory usage.
#[derive(Debug, Default)]
pub struct Int64SetWithMemoryUsage<A: Allocator + Clone = Global> {
    pub map: MemAwareMap<i64, (), A>,
}

/// NewInt64SetWithMemoryUsage builds an int64 set.
// go: NewInt64SetWithMemoryUsage L2 (no elements, module docs; the allocator is A1's)
pub fn new_int64_set_with_memory_usage<A: Allocator + Clone>(
    alloc: A,
) -> (Int64SetWithMemoryUsage<A>, i64) {
    let set = make_int64_set(alloc.clone());
    let mut set_with_memory_usage = Int64SetWithMemoryUsage {
        map: MemAwareMap::zero_in(alloc),
    };
    set_with_memory_usage.map.init(set);
    let bytes = set_with_memory_usage.map.bytes as i64;
    (set_with_memory_usage, bytes)
}

impl<A: Allocator + Clone> Int64SetWithMemoryUsage<A> {
    /// Insert inserts `val` into `s` and return memDelta.
    // go: Int64SetWithMemoryUsage.Insert L1
    pub fn insert(&mut self, val: i64) -> i64 {
        self.map.set(val, ())
    }

    /// `Exist` of the embedded map (M-SELF).
    // glue: forwarding to the embedded MemAwareMap (M-SELF)
    pub fn exist(&self, val: i64) -> bool {
        self.map.exist(&val)
    }

    /// `Count` of the embedded map (M-SELF).
    // glue: forwarding to the embedded MemAwareMap (M-SELF)
    pub fn count(&self) -> isize {
        self.map.count()
    }
}

#[cfg(test)]
mod tests {
    use tidb_memory::TrackedAlloc;
    use tidb_memory::tracker::new_tracker;

    use super::*;

    #[test]
    fn float64_set_memory_usage() {
        let (mut s, bytes) = new_float64_set_with_memory_usage(Global);
        // swissMap header and one group of map[float64]struct{}.
        assert_eq!(bytes, 48 + 72);
        for v in [1.0, 2.0, 0.0, -0.0, f64::NAN, f64::NAN] {
            s.insert(v);
        }
        // +0 and -0 are one key, each NaN is a key of its own.
        assert_eq!(s.count(), 5);
        assert!(s.exist(-0.0));
        assert!(!s.exist(f64::NAN));
    }

    #[test]
    fn sets_in_a_tracked_alloc() {
        let t = new_tracker(1, -1);
        {
            let a = TrackedAlloc::new(t.clone());
            let (mut s, _) = new_string_set_with_memory_usage(a.clone());
            let (mut f, _) = new_float64_set_with_memory_usage(a.clone());
            let (mut i, _) = new_int64_set_with_memory_usage(a.clone());
            for n in 0..1000i64 {
                s.insert(n.to_string().as_bytes());
                f.insert(n as f64);
                i.insert(n);
            }
            a.flush();
            assert!(t.bytes_consumed() > 0);
        }
        assert_eq!(t.bytes_consumed(), 0);
    }
}
