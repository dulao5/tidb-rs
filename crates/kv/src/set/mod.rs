//! `pkg/util/set` (D00.3 tool code): the sets the aggregate functions use (D12.4 part A,
//! the aggregate functions' DISTINCT), ported with their first user.
//!
//! Only `set_with_memory_usage.go` is ported whole; of `float64_set.go`, `int_set.go` and
//! `string_set.go` the map types it builds on are ported (spec D12.4 §2.2), the generic
//! `Set` of `set.go` is not used by the read path yet.

mod float64_set;
mod int_set;
mod set_with_memory_usage;
mod string_set;

pub use float64_set::Float64Key;
pub use float64_set::Float64Set;
pub use int_set::Int64Set;
pub use set_with_memory_usage::Float64SetWithMemoryUsage;
pub use set_with_memory_usage::Int64SetWithMemoryUsage;
pub use set_with_memory_usage::StringSetWithMemoryUsage;
pub use set_with_memory_usage::new_float64_set_with_memory_usage;
pub use set_with_memory_usage::new_int64_set_with_memory_usage;
pub use set_with_memory_usage::new_string_set_with_memory_usage;
pub use string_set::StringSet;
