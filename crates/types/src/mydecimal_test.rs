//! go-file: pkg/types/mydecimal_test.go
//!
//! Only `TestReset`: the other tests of the Go file are table tests whose tables govec
//! replays (`tests/govec_decimal.rs`, see the tracker's govec README).

use crate::errors::ErrBadNumber;
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
            Err(ErrBadNumber.clone()),
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

// glue: ToFloat64's error path (states only UnmarshalJSON / from_wire can build); expected
// values from Go v8.5.8.
#[test]
fn to_float64_error_value() {
    use crate::errors::ErrOverflow;
    for (json, s, bits, err) in [
        (
            r#"{"DigitsInt":13,"DigitsFrac":9,"ResultFrac":9,"Negative":false,"WordBuf":[12,345678901,-1,0,0,0,0,0,0]}"#,
            "12345678901.00000000/",
            0u64,
            Err(ErrOverflow.clone()),
        ),
        (
            r#"{"DigitsInt":18,"DigitsFrac":0,"ResultFrac":0,"Negative":false,"WordBuf":[5,-1999999999,0,0,0,0,0,0,0]}"#,
            "5'''''''''",
            0,
            Err(ErrOverflow.clone()),
        ),
        (
            r#"{"DigitsInt":18,"DigitsFrac":0,"ResultFrac":0,"Negative":true,"WordBuf":[1000000000,999999999,0,0,0,0,0,0,0]}"#,
            "-000000000999999999",
            0xc1cdcd64ff800000,
            Ok(()),
        ),
    ] {
        let mut d = MyDecimal::default();
        assert!(d.unmarshal_json(json.as_bytes()).is_ok());
        assert_eq!(d.string(), s);
        let (f, e) = d.to_float64();
        // Go: f is ParseFloat's value, 0 for a syntax error (a range error, ±Inf, needs
        // more than 308 digits, which 9 words cannot hold).
        assert_eq!((f.to_bits(), e), (bits, err), "{json}");
    }
}

// glue: M-FMA check of ToFloat64 on a DecimalDiv result (resultFrac 6 < digitsFrac 9). Go
// gc fuses mydecimal.go:1118/1125 into FMADDD on arm64 and not on amd64 (GOAMD64=v1), so Go
// gives 1.586563 on arm64 and 1.586562 on amd64 (tracker govec `sys/fma` records).
#[test]
fn to_float64_fma_per_target() {
    let (mut a, mut b, mut q) = (
        MyDecimal::default(),
        MyDecimal::default(),
        MyDecimal::default(),
    );
    assert!(a.from_string(b"50.77").is_ok());
    assert!(b.from_string(b"32").is_ok());
    assert!(crate::mydecimal::decimal_div(&a, &b, &mut q, 4).is_ok());
    assert_eq!(q.string(), "1.586563");
    let want = if cfg!(target_arch = "aarch64") {
        1.586563
    } else {
        1.586562
    };
    assert_eq!(q.to_float64(), (want, Ok(())));
}
