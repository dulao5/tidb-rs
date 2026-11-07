//! Go's float64 → integer conversions (`int64(f)`, `uint8(f)`, ...), mapping rule M-F2I.
//!
//! The Go spec leaves the result implementation-dependent when the value does not fit the
//! target type (and for NaN). gc's result depends on the target, so the port follows the
//! instructions gc emits on each (`cmd/compile/internal/ssagen`, `fpConvOpToSSA`):
//!
//! | Go | amd64 (GOAMD64=v1) | arm64 |
//! |---|---|---|
//! | `int64(f)`, `int(f)` | `CVTTSD2SQ`: out of range or NaN → `MinInt64` | `FCVTZS`: saturates, NaN → 0 |
//! | `int32(f)` | `CVTTSD2SL`: out of range or NaN → `MinInt32` | saturates to int32, NaN → 0 |
//! | `int16`, `int8`, `uint16`, `uint8` | the int32 conversion, then truncated | the int32 conversion, then truncated |
//! | `uint32(f)` | the int64 conversion, then truncated | the int64 conversion, then truncated |
//! | `uint64(f)`, `uint(f)` | `f < 2^63` ? `int64(f)` : `int64(f - 2^63) | 1<<63` (NaN takes the second branch: 2^63; ≥ 2^64 also 2^63) | `FCVTZU`: saturates, negative and NaN → 0 |
//!
//! Rust's `as` saturates and maps NaN to 0, which is the arm64 behaviour; other targets are
//! not supported by the vectors.
//! Values that fit are truncated toward zero on every target.

#[cfg(target_arch = "x86_64")]
const TWO_63: f64 = 9_223_372_036_854_775_808.0;

/// `int64(f)` (and `int(f)`).
// go-std: cmd/compile float64->int64 conversion L1 [M-F2I]
pub fn f64_to_i64(f: f64) -> i64 {
    #[cfg(target_arch = "x86_64")]
    if !(-TWO_63..TWO_63).contains(&f) {
        return i64::MIN; // also NaN
    }
    f as i64
}

/// The int32 conversion (`Cvt64Fto32`) that `int32`, `int16`, `int8`, `uint16`, `uint8`
/// conversions start from.
// go-std: cmd/compile float64->int32 conversion L1 [M-F2I]
fn cvt_to_i32(f: f64) -> i32 {
    #[cfg(target_arch = "x86_64")]
    if !(f > -2_147_483_649.0 && f < 2_147_483_648.0) {
        return i32::MIN; // also NaN
    }
    f as i32
}

/// `int32(f)`.
// go-std: cmd/compile float64->int32 conversion L1 [M-F2I]
pub fn f64_to_i32(f: f64) -> i32 {
    cvt_to_i32(f)
}

/// `int16(f)`.
// go-std: cmd/compile float64->int16 conversion L1 [M-F2I]
pub fn f64_to_i16(f: f64) -> i16 {
    cvt_to_i32(f) as i16
}

/// `int8(f)`.
// go-std: cmd/compile float64->int8 conversion L1 [M-F2I]
pub fn f64_to_i8(f: f64) -> i8 {
    cvt_to_i32(f) as i8
}

/// `uint16(f)`.
// go-std: cmd/compile float64->uint16 conversion L1 [M-F2I]
pub fn f64_to_u16(f: f64) -> u16 {
    cvt_to_i32(f) as u16
}

/// `uint8(f)`.
// go-std: cmd/compile float64->uint8 conversion L1 [M-F2I]
pub fn f64_to_u8(f: f64) -> u8 {
    cvt_to_i32(f) as u8
}

/// `uint32(f)`.
// go-std: cmd/compile float64->uint32 conversion L1 [M-F2I]
pub fn f64_to_u32(f: f64) -> u32 {
    f64_to_i64(f) as u32
}

/// `uint64(f)` (and `uint(f)`).
// go-std: cmd/compile float64->uint64 conversion L1 [M-F2I]
pub fn f64_to_u64(f: f64) -> u64 {
    #[cfg(target_arch = "x86_64")]
    {
        // ssagen float64ToUint64: x < 2^63 ? cvt(x) : cvt(x - 2^63) | 1<<63
        if f < TWO_63 {
            return f64_to_i64(f) as u64;
        }
        (f64_to_i64(f - TWO_63) as u64) | (1 << 63)
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        f as u64
    }
}
