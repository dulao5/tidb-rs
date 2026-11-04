//! go-file: pkg/types/mydecimal_test.go
//!
//! Only `TestReset`: the other tests of the Go file are table tests whose tables govec
//! replays (`tests/govec_decimal.rs`, see the tracker's govec README).

use crate::mydecimal::{MyDecimal, decimal_add};

// go: TestReset L1
#[test]
fn test_reset() {
    let (mut x1, mut y1, mut z1) = (
        MyDecimal::default(),
        MyDecimal::default(),
        MyDecimal::default(),
    );
    assert!(x1.from_string(b"38520.130741106671").is_ok());
    assert!(y1.from_string(b"9863.944799797851").is_ok());
    assert!(decimal_add(&x1, &y1, &mut z1).is_ok());

    let (mut x2, mut y2, mut z2) = (
        MyDecimal::default(),
        MyDecimal::default(),
        MyDecimal::default(),
    );
    assert!(x2.from_string(b"121519.080207244").is_ok());
    assert!(y2.from_string(b"54982.444519146").is_ok());
    assert!(decimal_add(&x2, &y2, &mut z2).is_ok());

    assert!(decimal_add(&x2, &y2, &mut z1).is_ok());
    assert_eq!(z2, z1);
}
