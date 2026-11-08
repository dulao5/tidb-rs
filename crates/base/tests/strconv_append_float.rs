//! `gostd::strconv::append_float` writes the shortest 'e' / 'f' float64 forms without going
//! through `format_float`; both must give the same bytes.

use tidb_base::gostd::strconv::{append_float, format_float};

fn check(f: f64) {
    for fmt in [b'e', b'f'] {
        let mut got = b"x".to_vec();
        append_float(&mut got, f, fmt, -1, 64);
        let want = format!("x{}", format_float(f, fmt, -1, 64));
        assert_eq!(got, want.as_bytes(), "f={f:e} bits={:#x} fmt={}", f.to_bits(), fmt as char);
    }
}

#[test]
fn append_float_matches_format_float() {
    let edge = [
        0.0, -0.0, 1.0, -1.0, 0.1, 2.5, 1e-6, 9.99e-7, 1e20, 1e21, 1e22, 123456789.0, 1e-7,
        1e100, 1e-100, 1e300, 5e-324, 2.2250738585072014e-308, f64::MAX, f64::MIN_POSITIVE,
        9007199254740993.0, 0.3, 1.0 / 3.0,
    ];
    for f in edge {
        check(f);
        check(-f);
    }
    // xorshift over raw bit patterns (finite only) and over "human" decimals.
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    for i in 0..200_000 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let f = f64::from_bits(x);
        if f.is_finite() {
            check(f);
        }
        check((x % 1_000_000) as f64 / 10f64.powi((i % 12) as i32));
    }
    // Non-shortest / other forms fall back to format_float.
    for (fmt, prec) in [(b'g', -1), (b'e', 3), (b'f', 2)] {
        let mut got = Vec::new();
        append_float(&mut got, 1234.5678, fmt, prec, 64);
        assert_eq!(got, format_float(1234.5678, fmt, prec, 64).as_bytes());
    }
}
