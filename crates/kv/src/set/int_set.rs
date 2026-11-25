//! go-file: pkg/util/set/int_set.go

use allocator_api2::alloc::Allocator;
use hashbrown::DefaultHashBuilder;
use hashbrown::HashMap;

/// Int64Set is a int64 set.
// go: Int64Set L2 (a map type; the allocator parameter is A1's)
pub type Int64Set<A> = HashMap<i64, (), DefaultHashBuilder, A>;

/// The empty `make(Int64Set, 0)` in `alloc`.
// glue: make(Int64Set) in an allocator
pub(crate) fn make_int64_set<A: Allocator + Clone>(alloc: A) -> Int64Set<A> {
    HashMap::with_hasher_in(DefaultHashBuilder::default(), alloc)
}
