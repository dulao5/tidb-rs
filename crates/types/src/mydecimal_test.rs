//! go-file: pkg/types/mydecimal_test.go
//!
//! Only `TestReset`: the other tests of the Go file are table tests whose tables govec
//! replays (`tests/govec_decimal.rs`, see the tracker's govec README).

use crate::errors::ERR_BAD_NUMBER;
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

// glue: from_wire/as_wire have no Go counterpart (Go casts the chunk bytes with unsafe).
#[test]
fn wire_round_trip_and_invalid_bool() {
    let mut d = MyDecimal::default();
    assert!(d.from_string(b"-123456789012.000000000345").is_ok());
    let w = d.as_wire();
    // Go's layout: int8 digitsInt, digitsFrac, resultFrac; bool negative; [9]int32 LE.
    assert_eq!(&w[..4], &[12, 12, 12, 1]);
    assert_eq!(&w[4..8], &123i32.to_le_bytes());
    assert_eq!(&w[8..12], &456789012i32.to_le_bytes());
    assert_eq!(&w[12..16], &0i32.to_le_bytes());
    assert_eq!(&w[16..20], &345_000_000i32.to_le_bytes());
    assert_eq!(MyDecimal::from_wire(&w), Ok(d));

    for b in [2u8, 0x80, 0xff] {
        let mut bad = w;
        bad[3] = b;
        assert_eq!(
            MyDecimal::from_wire(&bad),
            Err(ERR_BAD_NUMBER),
            "negative byte {b}"
        );
    }
    // Other fields are not checked (as Go): a negative digitsInt is taken as is.
    let mut odd = w;
    odd[0] = 0xf7;
    let o = MyDecimal::from_wire(&odd).unwrap();
    assert_eq!(o.get_digits_int(), -9);
    assert_eq!(o.as_wire(), odd);
}
