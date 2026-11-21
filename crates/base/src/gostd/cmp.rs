//! go-file: $GOROOT/src/cmp/cmp.go
//!
//! Go's `cmp.Compare` (Go 1.25.12).

/// `cmp.Compare`: `-1`, `0` or `+1` as `x` is less than, equal to or greater than `y`. For
/// floating-point values a NaN is less than any non-NaN, and a NaN equals a NaN.
// go-std: cmp.Compare L1
pub fn compare<T: PartialOrd>(x: T, y: T) -> isize {
    #[allow(clippy::eq_op)]
    let (x_nan, y_nan) = (x != x, y != y);
    if x_nan {
        if y_nan {
            return 0;
        }
        return -1;
    }
    if y_nan {
        return 1;
    }
    match x.partial_cmp(&y) {
        Some(std::cmp::Ordering::Less) => -1,
        Some(std::cmp::Ordering::Greater) => 1,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::compare;

    #[test]
    fn compare_matches_go() {
        assert_eq!(compare(1, 2), -1);
        assert_eq!(compare(2u64, 2), 0);
        assert_eq!(compare(3i64, -3), 1);
        assert_eq!(compare(f64::NAN, f64::NEG_INFINITY), -1);
        assert_eq!(compare(0.0, f64::NAN), 1);
        assert_eq!(compare(f64::NAN, f64::NAN), 0);
        assert_eq!(compare(-0.0, 0.0), 0);
    }
}
