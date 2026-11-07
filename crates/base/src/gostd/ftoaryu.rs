//! go-file: $GOROOT/src/strconv/ftoaryu.go
//!
//! Go's shortest float formatting (`ryuFtoaShortest`): the shortest decimal digits that
//! round-trip, with Go's tie rule (round half to even when the exact value is halfway
//! between two shortest candidates). Rust's `{:e}` rounds such ties up, so the digits are
//! ported, not borrowed (review of tidb-rs PR #6).

use super::powers::{
    DETAILED_POWERS_OF_TEN, DETAILED_POWERS_OF_TEN_MAX_EXP10, DETAILED_POWERS_OF_TEN_MIN_EXP10,
};

/// `strconv.decimalSlice`: digits `d[start..start+nd]`, decimal point position `dp`.
pub(crate) struct DecimalSlice {
    d: [u8; 32],
    start: usize,
    pub(crate) nd: isize,
    pub(crate) dp: isize,
}

impl DecimalSlice {
    // glue: decimalSlice{d: buf[:]} with an offset for Go's d.d = d.d[n:]
    pub(crate) fn new() -> Self {
        DecimalSlice {
            d: [0; 32],
            start: 0,
            nd: 0,
            dp: 0,
        }
    }

    /// The digits (ASCII).
    // glue: d.d[:d.nd]
    pub(crate) fn digits(&self) -> &[u8] {
        &self.d[self.start..self.start + self.nd as usize]
    }

    // glue: d.d[i] with the offset
    fn at(&mut self, i: isize) -> &mut u8 {
        &mut self.d[self.start + i as usize]
    }
}

/// `strconv.floatInfo`.
pub(crate) struct FloatInfo {
    pub(crate) mantbits: u32,

    pub(crate) bias: i32,
}

pub(crate) const FLOAT32INFO: FloatInfo = FloatInfo {
    mantbits: 23,
    bias: -127,
};
pub(crate) const FLOAT64INFO: FloatInfo = FloatInfo {
    mantbits: 52,
    bias: -1023,
};

/// `ryuFtoaShortest` formats mant*2^exp with prec decimal digits.
// go-std: strconv.ryuFtoaShortest L1 [M-INT]
pub(crate) fn ryu_ftoa_shortest(d: &mut DecimalSlice, mut mant: u64, exp: i32, flt: &FloatInfo) {
    if mant == 0 {
        d.nd = 0;
        d.dp = 0;
        return;
    }
    // If input is an exact integer with fewer bits than the mantissa,
    // the previous and next integer are not admissible representations.
    if exp <= 0 && mant.trailing_zeros() as i32 >= -exp {
        mant >>= -exp as u32;
        ryu_digits(d, mant, mant, mant, true, false);
        return;
    }
    let (ml, mc, mu, mut e2) = compute_bounds(mant, exp, flt);
    if e2 == 0 {
        ryu_digits(d, ml, mc, mu, true, false);
        return;
    }
    // Find 10^q *larger* than 2^-e2
    let q = mul_by_log2_log10(-e2) + 1;

    // We are going to multiply by 10^q using 128-bit arithmetic.
    // The exponent is the same for all 3 numbers.
    let (mut dl, mut dc, mut du): (u64, u64, u64);
    let (mut dl0, mut dc0, mut du0): (bool, bool, bool);
    if flt.mantbits == 23 {
        let (dl32, _, l0) = mult64bit_pow10(ml as u32, e2, q);
        let (dc32, _, c0) = mult64bit_pow10(mc as u32, e2, q);
        let (du32, ne2, u0) = mult64bit_pow10(mu as u32, e2, q);
        e2 = ne2;
        (dl, dc, du) = (u64::from(dl32), u64::from(dc32), u64::from(du32));
        (dl0, dc0, du0) = (l0, c0, u0);
    } else {
        let (a, _, l0) = mult128bit_pow10(ml, e2, q);
        let (b, _, c0) = mult128bit_pow10(mc, e2, q);
        let (c, ne2, u0) = mult128bit_pow10(mu, e2, q);
        e2 = ne2;
        (dl, dc, du) = (a, b, c);
        (dl0, dc0, du0) = (l0, c0, u0);
    }
    if e2 >= 0 {
        panic!("not enough significant bits after mult128bitPow10");
    }
    // Is it an exact computation?
    if q > 55 {
        // Large positive powers of ten are not exact
        dl0 = false;
        dc0 = false;
        du0 = false;
    }
    if (-24..0).contains(&q) {
        // Division by a power of ten may be exact.
        // (note that 5^25 is a 59-bit number so division by 5^25 is never exact).
        if divisible_by_power5(ml, -q) {
            dl0 = true;
        }
        if divisible_by_power5(mc, -q) {
            dc0 = true;
        }
        if divisible_by_power5(mu, -q) {
            du0 = true;
        }
    }
    // Express the results (dl, dc, du)*2^e2 as integers.
    // Extra bits must be removed and rounding hints computed.
    let extra = (-e2) as u32;
    let extra_mask = (1u64 << extra) - 1;
    // Now compute the floored, integral base 10 mantissas.
    let fracl = dl & extra_mask;
    dl >>= extra;
    let fracc = dc & extra_mask;
    dc >>= extra;
    let fracu = du & extra_mask;
    du >>= extra;
    // Is it allowed to use 'du' as a result?
    // It is always allowed when it is truncated, but also
    // if it is exact and the original binary mantissa is even
    // When disallowed, we can subtract 1.
    let mut uok = !du0 || fracu > 0;
    if du0 && fracu == 0 {
        uok = mant & 1 == 0;
    }
    if !uok {
        du -= 1;
    }
    // Is 'dc' the correctly rounded base 10 mantissa?
    // The correct rounding might be dc+1
    let cup = if dc0 {
        // If we computed an exact product, the half integer
        // should round to next (even) integer if 'dc' is odd.
        fracc > 1 << (extra - 1) || (fracc == 1 << (extra - 1) && dc & 1 == 1)
    } else {
        // otherwise, the result is a lower truncation of the ideal
        // result.
        fracc >> (extra - 1) == 1
    };
    // Is 'dl' an allowed representation?
    // Only if it is an exact value, and if the original binary mantissa
    // was even.
    let lok = dl0 && fracl == 0 && (mant & 1 == 0);
    if !lok {
        dl += 1;
    }
    // We need to remember whether the trimmed digits of 'dc' are zero.
    let c0 = dc0 && fracc == 0;
    // render digits
    ryu_digits(d, dl, dc, du, c0, cup);
    d.dp -= q as isize;
}

/// `mulByLog2Log10` returns math.Floor(x * log(2)/log(10)) for an integer x in
/// the range -1600 <= x && x <= +1600.
// go-std: strconv.mulByLog2Log10 L1
fn mul_by_log2_log10(x: i32) -> i32 {
    // log(2)/log(10) ≈ 0.30102999566 ≈ 78913 / 2^18
    (x * 78913) >> 18
}

/// `mulByLog10Log2` returns math.Floor(x * log(10)/log(2)) for an integer x in
/// the range -500 <= x && x <= +500.
// go-std: strconv.mulByLog10Log2 L1
fn mul_by_log10_log2(x: i32) -> i32 {
    // log(10)/log(2) ≈ 3.32192809489 ≈ 108853 / 2^15
    (x * 108853) >> 15
}

/// `computeBounds` returns a floating-point vector (l, c, u)×2^e2
/// where the mantissas are 55-bit (or 26-bit) integers, describing the interval
/// represented by the input float64 or float32.
// go-std: strconv.computeBounds L1
fn compute_bounds(mant: u64, exp: i32, flt: &FloatInfo) -> (u64, u64, u64, i32) {
    if mant != 1 << flt.mantbits || exp == flt.bias + 1 - flt.mantbits as i32 {
        // regular case (or denormals)
        (2 * mant - 1, 2 * mant, 2 * mant + 1, exp - 1)
    } else {
        // border of an exponent
        (4 * mant - 1, 4 * mant, 4 * mant + 2, exp - 2)
    }
}

// go-std: strconv.ryuDigits L1
fn ryu_digits(
    d: &mut DecimalSlice,
    lower: u64,
    central: u64,
    upper: u64,
    mut c0: bool,
    mut cup: bool,
) {
    let (mut lhi, llo) = divmod1e9(lower);
    let (chi, clo) = divmod1e9(central);
    let (uhi, ulo) = divmod1e9(upper);
    if uhi == 0 {
        // only low digits (for denormals)
        ryu_digits32(d, llo, clo, ulo, c0, cup, 8);
    } else if lhi < uhi {
        // truncate 9 digits at once.
        if llo != 0 {
            lhi += 1;
        }
        c0 = c0 && clo == 0;
        cup = (clo > 500_000_000) || (clo == 500_000_000 && cup);
        ryu_digits32(d, lhi, chi, uhi, c0, cup, 8);
        d.dp += 9;
    } else {
        d.nd = 0;
        // emit high part
        let mut n: usize = 9;
        let mut v = chi;
        while v > 0 {
            let (v1, v2) = (v / 10, v % 10);
            v = v1;
            n -= 1;
            d.d[d.start + n] = v2 as u8 + b'0';
        }
        d.start += n;
        d.nd = (9 - n) as isize;
        // emit low part
        let endindex = d.nd + 8;
        ryu_digits32(d, llo, clo, ulo, c0, cup, endindex);
    }
    // trim trailing zeros
    while d.nd > 0 && *d.at(d.nd - 1) == b'0' {
        d.nd -= 1;
    }
    // trim initial zeros
    while d.nd > 0 && *d.at(0) == b'0' {
        d.nd -= 1;
        d.dp -= 1;
        d.start += 1;
    }
}

/// `ryuDigits32` emits decimal digits for a number less than 1e9.
// go-std: strconv.ryuDigits32 L1
fn ryu_digits32(
    d: &mut DecimalSlice,
    mut lower: u32,
    mut central: u32,
    mut upper: u32,
    mut c0: bool,
    mut cup: bool,
    mut endindex: isize,
) {
    if upper == 0 {
        d.dp = endindex + 1;
        return;
    }
    let mut trimmed = 0isize;
    // Remember last trimmed digit to check for round-up.
    // c0 will be used to remember zeroness of following digits.
    let mut c_next_digit = 0u32;
    while upper > 0 {
        // Repeatedly compute:
        // l = Ceil(lower / 10^k)
        // c = Round(central / 10^k)
        // u = Floor(upper / 10^k)
        // and stop when c goes out of the (l, u) interval.
        let l = lower.div_ceil(10);
        let (mut c, mut cdigit) = (central / 10, central % 10);
        let u = upper / 10;
        if l > u {
            // don't trim the last digit as it is forbidden to go below l
            // other, trim and exit now.
            break;
        }
        // Check that we didn't cross the lower boundary.
        // The case where l < u but c == l-1 is essentially impossible,
        // but may happen if:
        //    lower   = ..11
        //    central = ..19
        //    upper   = ..31
        // and means that 'central' is very close but less than
        // an integer ending with many zeros, and usually
        // the "round-up" logic hides the problem.
        if l == c + 1 && c < u {
            c += 1;
            cdigit = 0;
            cup = false;
        }
        trimmed += 1;
        // Remember trimmed digits of c
        c0 = c0 && c_next_digit == 0;
        c_next_digit = cdigit;
        lower = l;
        central = c;
        upper = u;
    }
    // should we round up?
    if trimmed > 0 {
        cup = c_next_digit > 5
            || (c_next_digit == 5 && !c0)
            || (c_next_digit == 5 && c0 && central & 1 == 1);
    }
    if central < upper && cup {
        central += 1;
    }
    // We know where the number ends, fill directly
    endindex -= trimmed;
    let mut v = central;
    let mut n = endindex;
    while n > d.nd {
        let (v1, v2) = (v / 100, v % 100);
        *d.at(n) = b'0' + (v2 % 10) as u8;
        *d.at(n - 1) = b'0' + (v2 / 10) as u8;
        n -= 2;
        v = v1;
    }
    if n == d.nd {
        *d.at(n) = v as u8 + b'0';
    }
    d.nd = endindex + 1;
    d.dp = d.nd + trimmed;
}

/// `mult64bitPow10` takes a floating-point input with a 25-bit
/// mantissa and multiplies it with 10^q. The resulting mantissa
/// is m*P >> 57 where P is a 64-bit element of the detailedPowersOfTen tables.
/// It is typically 31 or 32-bit wide.
/// The returned boolean is true if all trimmed bits were zero.
// go-std: strconv.mult64bitPow10 L1 [M-INT]
fn mult64bit_pow10(m: u32, mut e2: i32, q: i32) -> (u32, i32, bool) {
    if q == 0 {
        // P == 1<<63
        return (m << 6, e2 - 6, true);
    }
    if q < DETAILED_POWERS_OF_TEN_MIN_EXP10 as i32 || (DETAILED_POWERS_OF_TEN_MAX_EXP10 as i32) < q
    {
        // This never happens due to the range of float32/float64 exponent
        panic!("mult64bitPow10: power of 10 is out of range");
    }
    let mut pow = DETAILED_POWERS_OF_TEN[(q - DETAILED_POWERS_OF_TEN_MIN_EXP10 as i32) as usize][1];
    if q < 0 {
        // Inverse powers of ten must be rounded up.
        pow += 1;
    }
    let p = u128::from(m) * u128::from(pow);
    let (hi, lo) = ((p >> 64) as u64, p as u64);
    e2 += mul_by_log10_log2(q) - 63 + 57;
    ((hi << 7 | lo >> 57) as u32, e2, lo << 7 == 0)
}

/// `mult128bitPow10` takes a floating-point input with a 55-bit
/// mantissa and multiplies it with 10^q. The resulting mantissa
/// is m*P >> 119 where P is a 128-bit element of the detailedPowersOfTen tables.
/// It is typically 63 or 64-bit wide.
/// The returned boolean is true is all trimmed bits were zero.
// go-std: strconv.mult128bitPow10 L1 [M-INT]
fn mult128bit_pow10(m: u64, mut e2: i32, q: i32) -> (u64, i32, bool) {
    if q == 0 {
        // P == 1<<127
        return (m << 8, e2 - 8, true);
    }
    if q < DETAILED_POWERS_OF_TEN_MIN_EXP10 as i32 || (DETAILED_POWERS_OF_TEN_MAX_EXP10 as i32) < q
    {
        // This never happens due to the range of float32/float64 exponent
        panic!("mult128bitPow10: power of 10 is out of range");
    }
    let mut pow = DETAILED_POWERS_OF_TEN[(q - DETAILED_POWERS_OF_TEN_MIN_EXP10 as i32) as usize];
    if q < 0 {
        // Inverse powers of ten must be rounded up.
        pow[0] = pow[0].wrapping_add(1);
    }
    e2 += mul_by_log10_log2(q) - 127 + 119;

    // long multiplication
    let p1 = u128::from(m) * u128::from(pow[0]);
    let (l1, l0) = ((p1 >> 64) as u64, p1 as u64);
    let p2 = u128::from(m) * u128::from(pow[1]);
    let (mut h1, h0) = ((p2 >> 64) as u64, p2 as u64);
    let (mid, carry) = l1.overflowing_add(h0);
    h1 = h1.wrapping_add(u64::from(carry));
    (h1 << 9 | mid >> 55, e2, mid << 9 == 0 && l0 == 0)
}

// go-std: strconv.divisibleByPower5 L1
fn divisible_by_power5(mut m: u64, k: i32) -> bool {
    if m == 0 {
        return true;
    }
    for _ in 0..k {
        if !m.is_multiple_of(5) {
            return false;
        }
        m /= 5;
    }
    true
}

/// `divmod1e9` computes quotient and remainder of division by 1e9.
// go-std: strconv.divmod1e9 L1
fn divmod1e9(x: u64) -> (u32, u32) {
    ((x / 1_000_000_000) as u32, (x % 1_000_000_000) as u32)
}
