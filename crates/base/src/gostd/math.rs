//! go-file: $GOROOT/src/math/bits.go
//! go-file: $GOROOT/src/math/pow10.go
//!
//! Go `math` values that Rust's std spells differently.

/// `math.NaN()` returns an IEEE 754 "not-a-number" value: Go's is `0x7FF8000000000001`
/// (payload 1), Rust's `f64::NAN` is `0x7FF8000000000000`. The bits are observable
/// (`math.Float64bits`, vectors of float results).
// go-std: math.NaN L1
pub fn nan() -> f64 {
    f64::from_bits(0x7FF8000000000001)
}

/// `math.Pow10(n)` returns 10**n, the base-10 exponential of n: exact table products for
/// `-323 <= n <= 308`, `0` below and `+Inf` above (Rust's `powi` rounds differently).
// go-std: math.Pow10 L1
pub fn pow10(n: isize) -> f64 {
    const POW10TAB: [f64; 32] = [
        1e00, 1e01, 1e02, 1e03, 1e04, 1e05, 1e06, 1e07, 1e08, 1e09, 1e10, 1e11, 1e12, 1e13, 1e14,
        1e15, 1e16, 1e17, 1e18, 1e19, 1e20, 1e21, 1e22, 1e23, 1e24, 1e25, 1e26, 1e27, 1e28, 1e29,
        1e30, 1e31,
    ];
    const POW10POSTAB32: [f64; 10] = [
        1e00, 1e32, 1e64, 1e96, 1e128, 1e160, 1e192, 1e224, 1e256, 1e288,
    ];
    const POW10NEGTAB32: [f64; 11] = [
        1e-00, 1e-32, 1e-64, 1e-96, 1e-128, 1e-160, 1e-192, 1e-224, 1e-256, 1e-288, 1e-320,
    ];
    if (0..=308).contains(&n) {
        return POW10POSTAB32[n as usize / 32] * POW10TAB[n as usize % 32];
    }
    if (-323..=0).contains(&n) {
        return POW10NEGTAB32[(-n) as usize / 32] / POW10TAB[(-n) as usize % 32];
    }
    // n < -323 || 308 < n
    if n > 0 {
        return f64::INFINITY;
    }
    0.0
}
