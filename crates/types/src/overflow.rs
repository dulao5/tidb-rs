//! go-file: pkg/types/overflow.go
//!
//! Overflow-checked integer and duration arithmetic (block D01.4, ported with D01.1).
//!
//! Mapping: Go integer arithmetic wraps silently; every operation whose operands the checks
//! do not bound uses `wrapping_*` (`-b` for `b == MinInt64`, `uint64(-b)`). A zero divisor
//! panics in Go ("integer divide by zero") and in Rust ("attempt to divide by zero"); the
//! callers check the divisor first, as in Go.

use tidb_base::gofmt::Arg;

use crate::errors::{ErrOverflow, Error};

/// `ErrOverflow.GenWithStackByArgs(tp, fmt.Sprintf("(%d, %d)", a, b))`.
// glue: the error every function of overflow.go builds
fn overflow_err(tp: &'static str, a: impl std::fmt::Display, b: impl std::fmt::Display) -> Error {
    ErrOverflow.gen_with_stack_by_args(vec![Arg::from(tp), Arg::from(format!("({a}, {b})"))])
}

/// AddUint64 adds uint64 a and b if no overflow, else returns error.
// go: AddUint64 L1
pub fn add_uint64(a: u64, b: u64) -> Result<u64, Error> {
    if u64::MAX - a < b {
        return Err(overflow_err("BIGINT UNSIGNED", a, b));
    }
    Ok(a + b)
}

/// AddInt64 adds int64 a and b if no overflow, otherwise returns error.
// go: AddInt64 L1
pub fn add_int64(a: i64, b: i64) -> Result<i64, Error> {
    if (a > 0 && b > 0 && i64::MAX - a < b) || (a < 0 && b < 0 && i64::MIN - a > b) {
        return Err(overflow_err("BIGINT", a, b));
    }
    Ok(a + b)
}

/// AddDuration adds time.Duration a and b if no overflow, otherwise returns error.
/// (Go `time.Duration` is an int64 count of nanoseconds.)
// go: AddDuration L1
pub fn add_duration(a: i64, b: i64) -> Result<i64, Error> {
    if (a > 0 && b > 0 && i64::MAX - a < b) || (a < 0 && b < 0 && i64::MIN - a > b) {
        return Err(overflow_err("BIGINT", a, b));
    }
    Ok(a + b)
}

/// SubDuration subtracts time.Duration a with b and returns time.Duration if no overflow error.
/// (`%d` does not call `Duration.String`, so the message shows the nanosecond counts.)
// go: SubDuration L1
pub fn sub_duration(a: i64, b: i64) -> Result<i64, Error> {
    if (a > 0 && b < 0 && i64::MAX - a < b.wrapping_neg())
        || (a < 0 && b > 0 && i64::MIN - a > b.wrapping_neg())
        || (a == 0 && b == i64::MIN)
    {
        return Err(overflow_err("BIGINT", a, b));
    }
    Ok(a - b)
}

/// AddInteger adds uint64 a and int64 b and returns uint64 if no overflow error.
// go: AddInteger L1
pub fn add_integer(a: u64, b: i64) -> Result<u64, Error> {
    if b >= 0 {
        return add_uint64(a, b as u64);
    }
    if b.wrapping_neg() as u64 > a {
        return Err(overflow_err("BIGINT UNSIGNED", a, b));
    }
    Ok(a - b.wrapping_neg() as u64)
}

/// SubUint64 subtracts uint64 a with b and returns uint64 if no overflow error.
// go: SubUint64 L1
pub fn sub_uint64(a: u64, b: u64) -> Result<u64, Error> {
    if a < b {
        return Err(overflow_err("BIGINT UNSIGNED", a, b));
    }
    Ok(a - b)
}

/// SubInt64 subtracts int64 a with b and returns int64 if no overflow error.
// go: SubInt64 L1
pub fn sub_int64(a: i64, b: i64) -> Result<i64, Error> {
    if (a > 0 && b < 0 && i64::MAX - a < b.wrapping_neg())
        || (a < 0 && b > 0 && i64::MIN - a > b.wrapping_neg())
        || (a == 0 && b == i64::MIN)
    {
        return Err(overflow_err("BIGINT", a, b));
    }
    Ok(a - b)
}

/// SubUintWithInt subtracts uint64 a with int64 b and returns uint64 if no overflow error.
// go: SubUintWithInt L1
pub fn sub_uint_with_int(a: u64, b: i64) -> Result<u64, Error> {
    if b < 0 {
        return add_uint64(a, b.wrapping_neg() as u64);
    }
    sub_uint64(a, b as u64)
}

/// SubIntWithUint subtracts int64 a with uint64 b and returns uint64 if no overflow error.
// go: SubIntWithUint L1
pub fn sub_int_with_uint(a: i64, b: u64) -> Result<u64, Error> {
    if a < 0 || (a as u64) < b {
        return Err(overflow_err("BIGINT UNSIGNED", a, b));
    }
    Ok(a as u64 - b)
}

/// MulUint64 multiplies uint64 a and b and returns uint64 if no overflow error.
// go: MulUint64 L1
pub fn mul_uint64(a: u64, b: u64) -> Result<u64, Error> {
    if b > 0 && a > u64::MAX / b {
        return Err(overflow_err("BIGINT UNSIGNED", a, b));
    }
    Ok(a.wrapping_mul(b))
}

/// MulInt64 multiplies int64 a and b and returns int64 if no overflow error.
// go: MulInt64 L1
pub fn mul_int64(a: i64, b: i64) -> Result<i64, Error> {
    if a == 0 || b == 0 {
        return Ok(0);
    }
    let negative;
    let res = if a > 0 && b > 0 {
        negative = false;
        mul_uint64(a as u64, b as u64)
    } else if a < 0 && b < 0 {
        negative = false;
        mul_uint64(a.wrapping_neg() as u64, b.wrapping_neg() as u64)
    } else if a < 0 && b > 0 {
        negative = true;
        mul_uint64(a.wrapping_neg() as u64, b as u64)
    } else {
        negative = true;
        mul_uint64(a as u64, b.wrapping_neg() as u64)
    };
    let res = res?;
    if negative {
        // negative result
        if res > i64::MAX as u64 + 1 {
            return Err(overflow_err("BIGINT", a, b));
        }
        return Ok((res as i64).wrapping_neg());
    }
    // positive result
    if res > i64::MAX as u64 {
        return Err(overflow_err("BIGINT", a, b));
    }
    Ok(res as i64)
}

/// MulInteger multiplies uint64 a and int64 b, and returns uint64 if no overflow error.
// go: MulInteger L1
pub fn mul_integer(a: u64, b: i64) -> Result<u64, Error> {
    if a == 0 || b == 0 {
        return Ok(0);
    }
    if b < 0 {
        return Err(overflow_err("BIGINT UNSIGNED", a, b));
    }
    mul_uint64(a, b as u64)
}

/// DivInt64 divides int64 a with b, returns int64 if no overflow error.
/// It just checks overflow, if b is zero, a "divide by zero" panic throws.
// go: DivInt64 L1
pub fn div_int64(a: i64, b: i64) -> Result<i64, Error> {
    if a == i64::MIN && b == -1 {
        return Err(overflow_err("BIGINT", a, b));
    }
    Ok(a / b)
}

/// DivUintWithInt divides uint64 a with int64 b, returns uint64 if no overflow error.
/// It just checks overflow, if b is zero, a "divide by zero" panic throws.
// go: DivUintWithInt L1
pub fn div_uint_with_int(a: u64, b: i64) -> Result<u64, Error> {
    if b < 0 {
        if a != 0 && b.wrapping_neg() as u64 <= a {
            return Err(overflow_err("BIGINT UNSIGNED", a, b));
        }
        return Ok(0);
    }
    Ok(a / b as u64)
}

/// DivIntWithUint divides int64 a with uint64 b, returns uint64 if no overflow error.
/// It just checks overflow, if b is zero, a "divide by zero" panic throws.
// go: DivIntWithUint L1
pub fn div_int_with_uint(a: i64, b: u64) -> Result<u64, Error> {
    if a < 0 {
        if a.wrapping_neg() as u64 >= b {
            return Err(overflow_err("BIGINT UNSIGNED", a, b));
        }
        return Ok(0);
    }
    Ok(a as u64 / b)
}
