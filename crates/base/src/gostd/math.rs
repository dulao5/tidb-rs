//! go-file: $GOROOT/src/math/bits.go
//!
//! Go `math` values that Rust's std spells differently.

/// `math.NaN()` returns an IEEE 754 "not-a-number" value: Go's is `0x7FF8000000000001`
/// (payload 1), Rust's `f64::NAN` is `0x7FF8000000000000`. The bits are observable
/// (`math.Float64bits`, vectors of float results).
// go-std: math.NaN L1
pub fn nan() -> f64 {
    f64::from_bits(0x7FF8000000000001)
}
