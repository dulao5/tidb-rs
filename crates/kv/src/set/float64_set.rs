//! go-file: pkg/util/set/float64_set.go
//!
//! `Float64Set` is Go's `map[float64]struct{}`. A Go map compares float keys with `==`:
//! `+0` and `-0` are the same key, and a NaN equals nothing, so every NaN inserted is a new
//! element that no lookup finds. [`Float64Key`] gives a Rust map the same semantics.

use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::Relaxed;

use allocator_api2::alloc::Allocator;
use hashbrown::DefaultHashBuilder;
use hashbrown::HashMap;

use crate::hack::GoLayout;

/// A Go `float64` map key: the bits of a non-NaN value with `-0` folded into `+0`, or a NaN
/// made unique by a serial number (Go's NaN keys never compare equal).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Float64Key {
    Bits(u64),
    NaN(u64),
}

static NAN_SERIAL: AtomicU64 = AtomicU64::new(0);

impl Float64Key {
    /// The key Go's map would use for `v` on an insert: every NaN gets a key of its own.
    // glue: Go map[float64] key equality (+0 == -0, NaN != NaN)
    pub fn for_insert(v: f64) -> Float64Key {
        if v.is_nan() {
            Float64Key::NaN(NAN_SERIAL.fetch_add(1, Relaxed))
        } else {
            Float64Key::for_lookup(v).unwrap_or(Float64Key::Bits(0))
        }
    }

    /// The key a Go map lookup of `v` compares with; `None` for a NaN, which no key equals.
    // glue: Go map[float64] key equality (+0 == -0, NaN != NaN)
    pub fn for_lookup(v: f64) -> Option<Float64Key> {
        if v.is_nan() {
            None
        } else if v == 0.0 {
            Some(Float64Key::Bits(0))
        } else {
            Some(Float64Key::Bits(v.to_bits()))
        }
    }
}

/// A Go `float64`.
impl GoLayout for Float64Key {
    const GO_SIZE: u64 = 8;
    const GO_ALIGN: u64 = 8;
}

/// Float64Set is a float64 set.
// go: Float64Set L2 (a map type; the allocator parameter is A1's)
pub type Float64Set<A> = HashMap<Float64Key, (), DefaultHashBuilder, A>;

/// The empty `make(Float64Set, 0)` in `alloc`.
// glue: make(Float64Set) in an allocator
pub(crate) fn make_float64_set<A: Allocator + Clone>(alloc: A) -> Float64Set<A> {
    HashMap::with_hasher_in(DefaultHashBuilder::default(), alloc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_float_key_equality() {
        assert_eq!(Float64Key::for_lookup(0.0), Float64Key::for_lookup(-0.0));
        assert_eq!(Float64Key::for_insert(-0.0), Float64Key::for_insert(0.0));
        assert_eq!(Float64Key::for_lookup(f64::NAN), None);
        assert_ne!(
            Float64Key::for_insert(f64::NAN),
            Float64Key::for_insert(f64::NAN)
        );
        assert_ne!(Float64Key::for_lookup(1.0), Float64Key::for_lookup(-1.0));
    }
}
