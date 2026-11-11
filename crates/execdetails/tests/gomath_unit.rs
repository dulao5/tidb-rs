//! Go's `math.Min` / `math.Max` special cases (the documented results of `archMin` /
//! `archMax`): the t-digest relies on NaN propagating, which Rust's `f64::min` / `f64::max`
//! do not do. The ordinary results are covered by the govec `execdetails` vectors.

use tidb_base::gostd::math::{max, min};

#[test]
fn go_min_max_special_cases() {
    let nan = f64::NAN;
    assert!(min(1.0, nan).is_nan() && min(nan, 1.0).is_nan());
    assert!(max(1.0, nan).is_nan() && max(nan, 1.0).is_nan());
    assert_eq!(min(nan, f64::NEG_INFINITY), f64::NEG_INFINITY);
    assert_eq!(min(f64::NEG_INFINITY, nan), f64::NEG_INFINITY);
    assert_eq!(max(nan, f64::INFINITY), f64::INFINITY);
    assert_eq!(max(f64::INFINITY, nan), f64::INFINITY);
    assert!(min(0.0, -0.0).is_sign_negative() && min(-0.0, 0.0).is_sign_negative());
    assert!(max(0.0, -0.0).is_sign_positive() && max(-0.0, 0.0).is_sign_positive());
    assert_eq!(min(2.0, 3.0), 2.0);
    assert_eq!(max(2.0, 3.0), 3.0);
}
