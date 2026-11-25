//! go-file: pkg/util/set/string_set.go

use allocator_api2::alloc::Allocator;
use hashbrown::DefaultHashBuilder;
use hashbrown::HashMap;

/// StringSet is a string set (Go strings are bytes, M-BYTES).
// go: StringSet L2 (a map type; the allocator parameter is A1's)
pub type StringSet<A> = HashMap<Vec<u8>, (), DefaultHashBuilder, A>;

/// The empty `make(StringSet, 0)` in `alloc`.
// glue: make(StringSet) in an allocator
pub(crate) fn make_string_set<A: Allocator + Clone>(alloc: A) -> StringSet<A> {
    HashMap::with_hasher_in(DefaultHashBuilder::default(), alloc)
}
