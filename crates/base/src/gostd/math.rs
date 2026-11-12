//! go-file: $GOROOT/src/math/bits.go
//! go-file: $GOROOT/src/math/pow10.go
//! go-file: $GOROOT/src/math/dim.go
//! go-file: $GOROOT/src/math/sin.go
//! go-file: $GOROOT/src/math/trig_reduce.go
//! go-file: $GOROOT/src/math/asin.go
//! go-file: $GOROOT/src/math/atan.go
//! go-file: $GOROOT/src/math/atan2.go
//! go-file: $GOROOT/src/math/tan.go
//! go-file: $GOROOT/src/math/exp.go
//! go-file: $GOROOT/src/math/exp_amd64.s
//! go-file: $GOROOT/src/math/exp_arm64.s
//! go-file: $GOROOT/src/math/log.go
//! go-file: $GOROOT/src/math/log_amd64.s
//! go-file: $GOROOT/src/math/log10.go
//! go-file: $GOROOT/src/math/pow.go
//! go-file: $GOROOT/src/math/frexp.go
//! go-file: $GOROOT/src/math/ldexp.go
//!
//! Go `math` values that Rust's std spells differently, and the pure-Go functions whose last
//! bit differs from Rust's libm (`Sin`, `Asin`): Go 1.25.12 has no assembly for them on amd64
//! or arm64, so the port is the Go code itself.
//!
//! Floating-point multiply-adds follow mapping rule M-FMA. These are standard-library files,
//! outside the tracker's `data/go_fma_sites.tsv` (which scans TiDB packages); the fused
//! positions below were read from `go tool objdump` of a go1.25.12 binary (GOOS=linux,
//! GOARCH=arm64): every `FMADDD` / `FMSUBD` / `FNMSUBD` maps to the marked expression.
//! amd64 (GOAMD64=v1) fuses nothing, so the non-aarch64 branch is the plain Go expression.
//!
//! Assembly (D08.5 batch 4): Go's `Exp` is assembly on amd64 (`exp_amd64.s`, with an FMA
//! path chosen at run time when the CPU has AVX and FMA, mirrored here with
//! `is_x86_feature_detected!`) and on arm64 (`exp_arm64.s`); `Log` is assembly on amd64
//! only (`log_amd64.s`). Each is ported instruction by instruction for its target; other
//! targets use the pure-Go code. The results differ between targets (tracker `targetDiffs`).

/// `x*y + z` rounded once on arm64 (gc emits `FMADDD`), twice elsewhere (mapping rule M-FMA).
#[inline]
// glue: M-FMA helper (fused on aarch64, separate multiply and add elsewhere)
fn fma_arm64(x: f64, y: f64, z: f64) -> f64 {
    #[cfg(target_arch = "aarch64")]
    {
        x.mul_add(y, z)
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        x * y + z
    }
}

/// `z - x*y` rounded once on arm64 (gc emits `FMSUBD`), twice elsewhere (mapping rule M-FMA).
#[inline]
// glue: M-FMA helper (fused on aarch64, separate multiply and subtract elsewhere)
fn fms_arm64(z: f64, x: f64, y: f64) -> f64 {
    #[cfg(target_arch = "aarch64")]
    {
        (-x).mul_add(y, z)
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        z - x * y
    }
}

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

/// `math.Min`: the smaller of x or y. `Min(x, -Inf) = Min(-Inf, x) = -Inf`,
/// `Min(x, NaN) = Min(NaN, x) = NaN`, `Min(-0, ±0) = Min(±0, -0) = -0` (Rust's `f64::min`
/// returns the non-NaN operand). amd64 and arm64 use assembly (`archMin`, `FMIND` on arm64)
/// with the same results; the NaN payload is not observable to the ported callers.
// go-std: math.Min L1 (math.min: the documented special cases of archMin)
pub fn min(x: f64, y: f64) -> f64 {
    if x == f64::NEG_INFINITY || y == f64::NEG_INFINITY {
        return f64::NEG_INFINITY;
    }
    if x.is_nan() || y.is_nan() {
        return nan();
    }
    if x == 0.0 && x == y {
        if x.is_sign_negative() {
            return x;
        }
        return y;
    }
    if x < y { x } else { y }
}

/// `math.Max`: `Max(x, +Inf) = Max(+Inf, x) = +Inf`, `Max(x, NaN) = Max(NaN, x) = NaN`,
/// `Max(+0, ±0) = Max(±0, +0) = +0`.
// go-std: math.Max L1 (math.max: the documented special cases of archMax)
pub fn max(x: f64, y: f64) -> f64 {
    if x == f64::INFINITY || y == f64::INFINITY {
        return f64::INFINITY;
    }
    if x.is_nan() || y.is_nan() {
        return nan();
    }
    if x == 0.0 && x == y {
        if x.is_sign_negative() {
            return y;
        }
        return x;
    }
    if x > y { x } else { y }
}

/// `math.Mod(x, y)`: the floating-point remainder of x/y, with the sign of x. Go computes it
/// exactly (repeated exact subtraction), so the value is IEEE `fmod`'s, which Rust's `%` is;
/// the special cases (`Mod(±Inf, y)`, `Mod(NaN, y)`, `Mod(x, 0)`, `Mod(x, NaN)`) return Go's
/// [`nan`] (payload 1), not the NaN `fmod` produces.
// go-std: math.Mod L2 (math.mod: the exact remainder is fmod; the special cases return math.NaN())
pub fn r#mod(x: f64, y: f64) -> f64 {
    if y == 0.0 || x.is_infinite() || x.is_nan() || y.is_nan() {
        return nan();
    }
    x % y
}

// sin coefficients
// Go's literals, as their bit patterns (the hex in sin.go's comments).
const SIN: [f64; 6] = [
    f64::from_bits(0x3de5d8fd1fd19ccd),
    f64::from_bits(0xbe5ae5e5a9291f5d),
    f64::from_bits(0x3ec71de3567d48a1),
    f64::from_bits(0xbf2a01a019bfdf03),
    f64::from_bits(0x3f8111111110f7d0),
    f64::from_bits(0xbfc5555555555548),
];

// cos coefficients
const COS: [f64; 6] = [
    f64::from_bits(0xbda8fa49a0861a9b),
    f64::from_bits(0x3e21ee9d7b4e3f05),
    f64::from_bits(0xbe927e4f7eac4bc6),
    f64::from_bits(0x3efa01a019c844f5),
    f64::from_bits(0xbf56c16c16c14f91),
    f64::from_bits(0x3fa555555555554b),
];

/// `math.Sin`: the sine of the radian argument x. `Sin(±0) = ±0`, `Sin(±Inf) = NaN`,
/// `Sin(NaN) = NaN`.
// go-std: math.sin L1 [M-FMA] (math.Sin: haveArchSin is false on amd64 and arm64)
pub fn sin(x: f64) -> f64 {
    const PI4A: f64 = f64::from_bits(0x3fe921fb40000000); // Pi/4 split into three parts
    const PI4B: f64 = f64::from_bits(0x3e64442d00000000);
    const PI4C: f64 = f64::from_bits(0x3ce8469898cc5170);
    let mut x = x;
    // special cases
    if x == 0.0 || x.is_nan() {
        return x; // return ±0 || NaN()
    }
    if x.is_infinite() {
        return nan();
    }

    // make argument positive but save the sign
    let mut sign = false;
    if x < 0.0 {
        x = -x;
        sign = true;
    }

    let mut j: u64;
    let mut y: f64;
    let z: f64;
    if x >= REDUCE_THRESHOLD {
        (j, z) = trig_reduce(x);
    } else {
        // integer part of x/(Pi/4), as integer for tests on the phase angle
        // Go's untyped constant 4/Pi rounds to 0x3ff45f306dc9c883, the same as 4.0 / PI.
        j = super::conv::f64_to_u64(x * (4.0 / std::f64::consts::PI)); // [M-F2I]
        y = j as f64; // integer part of x/(Pi/4), as float

        // map zeros to origin
        if j & 1 == 1 {
            j += 1;
            y += 1.0;
        }
        j &= 7; // octant modulo 2Pi radians (360 degrees)
        // Extended precision modular arithmetic
        // [M-FMA] sin.go:227 (three FMSUBD: each `- y*PI4?` is fused with the subtraction)
        z = fms_arm64(fms_arm64(fms_arm64(x, y, PI4A), y, PI4B), y, PI4C);
    }
    // reflect in x axis
    if j > 3 {
        sign = !sign;
        j -= 4;
    }
    let zz = z * z;
    if j == 1 || j == 2 {
        // [M-FMA] sin.go:236 (1 - 0.5*zz, the Horner steps, and zz*zz*poly + (1 - 0.5*zz))
        let poly = fma_arm64(
            fma_arm64(
                fma_arm64(
                    fma_arm64(fma_arm64(COS[0], zz, COS[1]), zz, COS[2]),
                    zz,
                    COS[3],
                ),
                zz,
                COS[4],
            ),
            zz,
            COS[5],
        );
        y = fma_arm64(zz * zz, poly, fms_arm64(1.0, 0.5, zz));
    } else {
        // [M-FMA] sin.go:238 (the Horner steps, and z*zz*poly + z)
        let poly = fma_arm64(
            fma_arm64(
                fma_arm64(
                    fma_arm64(fma_arm64(SIN[0], zz, SIN[1]), zz, SIN[2]),
                    zz,
                    SIN[3],
                ),
                zz,
                SIN[4],
            ),
            zz,
            SIN[5],
        );
        y = fma_arm64(z * zz, poly, z);
    }
    if sign {
        y = -y;
    }
    y
}

/// `reduceThreshold`: above it Payne-Hanek range reduction must be used.
const REDUCE_THRESHOLD: f64 = (1u64 << 29) as f64;

/// `mPi4`: the binary digits of 4/pi as a uint64 array, `4/pi = Sum mPi4[i]*2^(-64*i)`.
const M_PI4: [u64; 20] = [
    0x0000000000000001,
    0x45f306dc9c882a53,
    0xf84eafa3ea69bb81,
    0xb6c52b3278872083,
    0xfca2c757bd778ac3,
    0x6e48dc74849ba5c0,
    0x0c925dd413a32439,
    0xfc3bd63962534e7d,
    0xd1046bea5d768909,
    0xd338e04d68befc82,
    0x7323ac7306a673e9,
    0x3908bf177bf25076,
    0x3ff12fffbc0b301f,
    0xde5e2316b414da3e,
    0xda6cfd9e4f96136e,
    0x9e8c7ecd3cbfd45a,
    0xea4f758fd7cbe2f6,
    0x7a0e73ef14a525d4,
    0xd7f6bf623f1aba10,
    0xac06608df8f6d757,
];

/// Go's `x << n` for unsigned x: 0 when n >= 64.
#[inline]
// glue: Go shift semantics (count >= width gives 0)
fn shl(x: u64, n: u32) -> u64 {
    x.checked_shl(n).unwrap_or(0)
}

/// Go's `x >> n` for unsigned x: 0 when n >= 64.
#[inline]
// glue: Go shift semantics (count >= width gives 0)
fn shr(x: u64, n: u32) -> u64 {
    x.checked_shr(n).unwrap_or(0)
}

/// `trigReduce`: Payne-Hanek range reduction by Pi/4 for x > 0. It returns the integer part
/// mod 8 (j) and the fractional part (z) of x / (Pi/4).
// go-std: math.trigReduce L1
fn trig_reduce(x: f64) -> (u64, f64) {
    const PI4: f64 = std::f64::consts::PI / 4.0;
    const MASK: u64 = 0x7FF;
    const SHIFT: u32 = 64 - 11 - 1;
    const BIAS: i64 = 1023;
    if x < PI4 {
        return (0, x);
    }
    // Extract out the integer and exponent such that, x = ix * 2 ** exp.
    let mut ix = x.to_bits();
    let exp = ((ix >> SHIFT) & MASK) as i64 - BIAS - SHIFT as i64;
    ix &= !(MASK << SHIFT);
    ix |= 1 << SHIFT;
    // Use the exponent to extract the 3 appropriate uint64 digits from mPi4,
    // B ~ (z0, z1, z2), such that the product leading digit has the exponent -61.
    let digit = ((exp + 61) as u64 / 64) as usize;
    let bitshift = ((exp + 61) as u64 % 64) as u32;
    let z0 = shl(M_PI4[digit], bitshift) | shr(M_PI4[digit + 1], 64 - bitshift);
    let z1 = shl(M_PI4[digit + 1], bitshift) | shr(M_PI4[digit + 2], 64 - bitshift);
    let z2 = shl(M_PI4[digit + 2], bitshift) | shr(M_PI4[digit + 3], 64 - bitshift);
    // Multiply mantissa by the digits and extract the upper two digits (hi, lo).
    let z2hi = ((z2 as u128 * ix as u128) >> 64) as u64;
    let z1p = z1 as u128 * ix as u128;
    let (z1hi, z1lo) = ((z1p >> 64) as u64, z1p as u64);
    let z0lo = z0.wrapping_mul(ix);
    let (lo, c) = z1lo.overflowing_add(z2hi);
    let mut hi = z0lo.wrapping_add(z1hi).wrapping_add(c as u64);
    // The top 3 bits are j.
    let mut j = hi >> 61;
    // Extract the fraction and find its magnitude.
    hi = (hi << 3) | (lo >> 61);
    let lz = hi.leading_zeros();
    let e = (BIAS - (lz as i64 + 1)) as u64;
    // Clear implicit mantissa bit and shift into place.
    hi = shl(hi, lz + 1) | shr(lo, 64 - (lz + 1));
    hi >>= 64 - SHIFT;
    // Include the exponent and convert to a float.
    hi |= e << SHIFT;
    let mut z = f64::from_bits(hi);
    // Map zeros to origin.
    if j & 1 == 1 {
        j += 1;
        j &= 7;
        z -= 1.0;
    }
    // Multiply the fractional part by pi/4.
    (j, z * PI4)
}

/// `math.Asin`: the arcsine, in radians, of x. `Asin(±0) = ±0`, `Asin(x) = NaN` if x < -1
/// or x > 1.
// go-std: math.asin L1 [M-FMA] (math.Asin: haveArchAsin is false on amd64 and arm64)
pub fn asin(x: f64) -> f64 {
    let mut x = x;
    if x == 0.0 {
        return x; // special case
    }
    let mut sign = false;
    if x < 0.0 {
        x = -x;
        sign = true;
    }
    if x > 1.0 {
        return nan(); // special case
    }

    let mut temp = fms_arm64(1.0, x, x).sqrt(); // [M-FMA] asin.go:40 (1 - x*x is FMSUBD)
    if x > 0.7 {
        temp = std::f64::consts::PI / 2.0 - satan(temp / x);
    } else {
        temp = satan(x / temp);
    }

    if sign {
        temp = -temp;
    }
    temp
}

/// `xatan` evaluates a series valid in the range [0, 0.66].
// Go's literals, verbatim (Rust rounds them to the same float64).
#[allow(clippy::excessive_precision)]
// go-std: math.xatan L1 [M-FMA]
fn xatan(x: f64) -> f64 {
    const P0: f64 = -8.750608600031904122785e-01;
    const P1: f64 = -1.615753718733365076637e+01;
    const P2: f64 = -7.500855792314704667340e+01;
    const P3: f64 = -1.228866684490136173410e+02;
    const P4: f64 = -6.485021904942025371773e+01;
    const Q0: f64 = 2.485846490142306297962e+01;
    const Q1: f64 = 1.650270098316988542046e+02;
    const Q2: f64 = 4.328810604912902668951e+02;
    const Q3: f64 = 4.853903996359136964868e+02;
    const Q4: f64 = 1.945506571482613964425e+02;
    let z = x * x;
    // [M-FMA] atan.go:69 (the four Horner steps of each polynomial; `z+Q0` with z = x*x is
    // FMADDD x, x, Q0)
    let p = fma_arm64(
        fma_arm64(fma_arm64(fma_arm64(P0, z, P1), z, P2), z, P3),
        z,
        P4,
    );
    let q = fma_arm64(
        fma_arm64(
            fma_arm64(fma_arm64(fma_arm64(x, x, Q0), z, Q1), z, Q2),
            z,
            Q3,
        ),
        z,
        Q4,
    );
    let z = z * p / q;
    fma_arm64(x, z, x) // [M-FMA] atan.go:70
}

/// `satan` reduces its argument (known to be positive) to the range [0, 0.66] and calls
/// xatan.
#[allow(clippy::excessive_precision)]
// go-std: math.satan L1
fn satan(x: f64) -> f64 {
    const MOREBITS: f64 = 6.123233995736765886130e-17; // pi/2 = PIO2 + Morebits
    const TAN3PIO8: f64 = 2.41421356237309504880; // tan(3*pi/8)
    use std::f64::consts::PI;
    if x <= 0.66 {
        return xatan(x);
    }
    if x > TAN3PIO8 {
        return PI / 2.0 - xatan(1.0 / x) + MOREBITS;
    }
    PI / 4.0 + xatan((x - 1.0) / (x + 1.0)) + 0.5 * MOREBITS
}

// ------------------------------------------------------------------ D08.5 batch 4

/// `math.Abs`: the absolute value of x (clears the sign bit, also of a NaN).
// go-std: math.Abs L1
#[inline]
pub fn abs(x: f64) -> f64 {
    f64::from_bits(x.to_bits() & !(1u64 << 63))
}

/// `math.Copysign`: a value with the magnitude of f and the sign of sign.
// go-std: math.Copysign L1
#[inline]
pub fn copysign(f: f64, sign: f64) -> f64 {
    const SIGN_BIT: u64 = 1 << 63;
    f64::from_bits((f.to_bits() & !SIGN_BIT) | (sign.to_bits() & SIGN_BIT))
}

/// `normalize`: a normal number y and exponent exp satisfying x == y × 2**exp (x finite,
/// non-zero).
// go-std: math.normalize L1
fn normalize(x: f64) -> (f64, i64) {
    const SMALLEST_NORMAL: f64 = 2.2250738585072014e-308; // 2**-1022
    if abs(x) < SMALLEST_NORMAL {
        return (x * (1u64 << 52) as f64, -52);
    }
    (x, 0)
}

const MASK: u64 = 0x7FF;
const SHIFT: u64 = 64 - 11 - 1;
const BIAS: i64 = 1023;

/// `math.Frexp`: breaks f into a normalized fraction in [½, 1) and an integral power of
/// two. `Frexp(±0) = ±0, 0`, `Frexp(±Inf) = ±Inf, 0`, `Frexp(NaN) = NaN, 0`.
// go-std: math.frexp L1 (math.Frexp: haveArchFrexp is false on amd64 and arm64)
pub fn frexp(f: f64) -> (f64, i64) {
    // special cases
    if f == 0.0 || f.is_infinite() || f.is_nan() {
        return (f, 0); // correctly return -0
    }
    let (f, mut exp) = normalize(f);
    let mut x = f.to_bits();
    exp += ((x >> SHIFT) & MASK) as i64 - BIAS + 1;
    x &= !(MASK << SHIFT);
    x |= ((-1 + BIAS) as u64) << SHIFT;
    (f64::from_bits(x), exp)
}

/// `math.Ldexp`: the inverse of [`frexp`], frac × 2**exp.
// go-std: math.ldexp L1 (math.Ldexp: haveArchLdexp is false on amd64 and arm64)
pub fn ldexp(frac: f64, exp: i64) -> f64 {
    // special cases
    if frac == 0.0 || frac.is_infinite() || frac.is_nan() {
        return frac; // correctly return -0
    }
    let (frac, e) = normalize(frac);
    let mut exp = exp + e;
    let mut x = frac.to_bits();
    exp += ((x >> SHIFT) as i64 & MASK as i64) - BIAS;
    if exp < -1075 {
        return copysign(0.0, frac); // underflow
    }
    if exp > 1023 {
        // overflow
        if frac < 0.0 {
            return f64::NEG_INFINITY;
        }
        return f64::INFINITY;
    }
    let mut m: f64 = 1.0;
    if exp < -1022 {
        // denormal
        exp += 53;
        m = 1.0 / (1u64 << 53) as f64; // 2**-53
    }
    x &= !(MASK << SHIFT);
    x |= ((exp + BIAS) as u64) << SHIFT;
    m * f64::from_bits(x)
}

/// `math.Modf` of a finite f: the integer part and the fractional part (both with f's sign).
// go-std: math.Modf L2 (finite arguments only: the callers (pow, isOddInt) handle ±Inf / NaN first; the arm64 assembly and the pure-Go version agree there)
fn modf(f: f64) -> (f64, f64) {
    if f < 1.0 {
        if f < 0.0 {
            let (i, fr) = modf(-f);
            return (-i, -fr);
        }
        if f == 0.0 {
            return (f, f); // Return -0, -0 when f == -0
        }
        return (0.0, f);
    }
    let i = f.trunc();
    (i, f - i)
}

/// `math.Exp`: e**x. `Exp(+Inf) = +Inf`, `Exp(NaN) = NaN`; very large values overflow to
/// +Inf, very small to 0.
///
/// Go uses assembly on both targets, with different algorithms (so the last bit differs
/// between platforms, tracker `targetDiffs`): arm64 `archExp` (exp_arm64.s, the pure-Go
/// algorithm with fused multiply-adds) and amd64 `archExp` (exp_amd64.s, a Taylor series
/// after reducing by 1/16; an FMA variant when the CPU has AVX and FMA, Go's `useFMA`).
// go-std: math.Exp L1 (archExp on amd64 / arm64; the pure-Go exp elsewhere)
pub fn exp(x: f64) -> f64 {
    #[cfg(target_arch = "aarch64")]
    {
        exp_arm64(x)
    }
    #[cfg(target_arch = "x86_64")]
    {
        exp_amd64(x)
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        exp_go(x)
    }
}

/// The pure-Go `exp` (targets without `archExp`).
#[allow(clippy::excessive_precision, dead_code)]
// go-std: math.exp L1
fn exp_go(x: f64) -> f64 {
    const LN2HI: f64 = 6.93147180369123816490e-01;
    const LN2LO: f64 = 1.90821492927058770002e-10;
    const LOG2E: f64 = std::f64::consts::LOG2_E; // Go: 1.44269504088896338700e+00 (same bits)
    const OVERFLOW: f64 = 7.09782712893383973096e+02;
    const UNDERFLOW: f64 = -7.45133219101941108420e+02;
    const NEAR_ZERO: f64 = 1.0 / (1u64 << 28) as f64; // 2**-28
    // special cases
    if x.is_nan() || x == f64::INFINITY {
        return x;
    }
    if x == f64::NEG_INFINITY {
        return 0.0;
    }
    if x > OVERFLOW {
        return f64::INFINITY;
    }
    if x < UNDERFLOW {
        return 0.0;
    }
    if -NEAR_ZERO < x && x < NEAR_ZERO {
        return 1.0 + x;
    }
    // reduce; computed as r = hi - lo for extra precision.
    let mut k: i64 = 0;
    if x < 0.0 {
        k = super::conv::f64_to_i64(LOG2E * x - 0.5); // [M-F2I]
    } else if x > 0.0 {
        k = super::conv::f64_to_i64(LOG2E * x + 0.5); // [M-F2I]
    }
    let hi = x - k as f64 * LN2HI;
    let lo = k as f64 * LN2LO;
    // compute
    expmulti(hi, lo, k)
}

/// `expmulti`: e**r × 2**k where r = hi - lo and |r| ≤ ln(2)/2.
#[allow(clippy::excessive_precision, dead_code)]
// go-std: math.expmulti L1
fn expmulti(hi: f64, lo: f64, k: i64) -> f64 {
    const P1: f64 = 1.66666666666666657415e-01; /* 0x3FC55555; 0x55555555 */
    const P2: f64 = -2.77777777770155933842e-03; /* 0xBF66C16C; 0x16BEBD93 */
    const P3: f64 = 6.61375632143793436117e-05; /* 0x3F11566A; 0xAF25DE2C */
    const P4: f64 = -1.65339022054652515390e-06; /* 0xBEBBBD41; 0xC5D26BF1 */
    const P5: f64 = 4.13813679705723846039e-08; /* 0x3E663769; 0x72BEA4D0 */
    let r = hi - lo;
    let t = r * r;
    let c = r - t * (P1 + t * (P2 + t * (P3 + t * (P4 + t * P5))));
    let y = 1.0 - ((lo - (r * c) / (2.0 - c)) - hi);
    ldexp(y, k)
}

/// arm64 `archExp` (exp_arm64.s): the pure-Go algorithm with `FMADDD`/`FMSUBD`/`FNMSUBD`
/// and an inlined `Ldexp` that skips the Inf/NaN/zero checks.
#[cfg(target_arch = "aarch64")]
#[allow(clippy::excessive_precision)]
// go-std: math.archExp L1 (exp_arm64.s, instruction by instruction)
fn exp_arm64(x: f64) -> f64 {
    const LN2HI: f64 = 6.93147180369123816490e-01;
    const LN2LO: f64 = 1.90821492927058770002e-10;
    const LOG2E: f64 = std::f64::consts::LOG2_E; // Go: 1.44269504088896338700e+00 (same bits)
    const OVERFLOW: f64 = 7.09782712893383973096e+02;
    const UNDERFLOW: f64 = -7.45133219101941108420e+02;
    const NEAR_ZERO: f64 = f64::from_bits(0x3e30000000000000); // 2**-28
    const P1: f64 = 1.66666666666666657415e-01;
    const P2: f64 = -2.77777777770155933842e-03;
    const P3: f64 = 6.61375632143793436117e-05;
    const P4: f64 = -1.65339022054652515390e-06;
    const P5: f64 = 4.13813679705723846039e-08;
    if x.is_nan() {
        return x; // x = NaN, return NaN
    }
    if x > OVERFLOW {
        return f64::INFINITY;
    }
    if x < UNDERFLOW {
        return 0.0;
    }
    if abs(x) < NEAR_ZERO {
        return x + 1.0; // FADDD F1, F0: 1 + x
    }
    // argument reduction, x = k*ln2 + r, |r| <= 0.5*ln2; r = hi - lo for extra precision.
    let km = LOG2E.mul_add(x, -0.5); // FNMSUBD: Log2e*x - 0.5
    let kp = LOG2E.mul_add(x, 0.5); // FMADDD: Log2e*x + 0.5
    let kf = if x < 0.0 { km } else { kp };
    let k = kf as i64; // FCVTZSD (truncates, saturates)
    let kf = k as f64; // SCVTFD
    let hi = (-kf).mul_add(LN2HI, x); // FMSUBD: x - k*Ln2Hi
    let lo = kf * LN2LO;
    let r = hi - lo;
    let t = r * r;
    // compute y
    let mut c = t.mul_add(P5, P4);
    c = t.mul_add(c, P3);
    c = t.mul_add(c, P2);
    c = t.mul_add(c, P1);
    let c = (-t).mul_add(c, r); // FMSUBD: r - t*poly
    let q = (r * c) / (2.0 - c);
    let y = 1.0 - ((lo - q) - hi);
    // inline Ldexp(y, k)
    let bits = y.to_bits();
    let frac = bits & 0x000fffffffffffff;
    let mut e = (bits >> 52) as i64;
    e = e.wrapping_add(k);
    let mut m = 1.0;
    if e < 1 {
        e += 52; // denormal
        m = f64::from_bits(0x3cb0000000000000); // 2**-52
    }
    let r = f64::from_bits(((e as u64) << 52) | frac);
    m * r
}

/// Whether Go's amd64 `archExp` takes its FMA path: `useFMA = cpu.X86.HasAVX && HasFMA`.
#[cfg(target_arch = "x86_64")]
// go-std: math.useFMA L1 (exp_amd64.go)
fn use_fma() -> bool {
    use std::sync::OnceLock;
    static USE_FMA: OnceLock<bool> = OnceLock::new();
    *USE_FMA.get_or_init(|| is_x86_feature_detected!("avx") && is_x86_feature_detected!("fma"))
}

/// amd64 `archExp` (exp_amd64.s): reduce by k = round(x·log2(e)) (`CVTSD2SL`, round to
/// nearest even; out of int32 range gives the "integer indefinite" MinInt32), divide the
/// remainder by 16, a Taylor series, square four times, then scale by 2**k.
#[cfg(target_arch = "x86_64")]
// go-std: math.archExp L1 (exp_amd64.s, instruction by instruction)
fn exp_amd64(x: f64) -> f64 {
    const LOG2E: f64 = f64::from_bits(0x3ff71547652b82fe);
    const LN2U: f64 = f64::from_bits(0x3fe62e42fefa3000);
    const LN2L: f64 = f64::from_bits(0x3d53de6af278ece6);
    const OVERFLOW: f64 = f64::from_bits(0x40862e42fefa39ef);
    const C: [f64; 9] = [
        0.5,
        1.0,
        2.0,
        f64::from_bits(0x3fc5555555555555),
        f64::from_bits(0x3fa5555555555555),
        f64::from_bits(0x3f81111111111111),
        f64::from_bits(0x3f56c16c16c16c17),
        f64::from_bits(0x3f2a01a01a01a01a),
        f64::from_bits(0x3efa01a01a01a01a),
    ];
    let bx = x.to_bits();
    // test bits for not-finite
    if bx & !(1u64 << 63) >= 0x7FF0000000000000 {
        if bx == 0xFFF0000000000000 {
            return 0.0; // -Inf
        }
        return x; // NaN or +Inf
    }
    // check if argument will overflow
    if x > OVERFLOW {
        return f64::INFINITY;
    }
    let t = LOG2E * x;
    // CVTSD2SL: round to nearest even; NaN or out of range gives 0x80000000.
    let r = t.round_ties_even();
    let e: i32 = if (-2147483648.0..=2147483647.0).contains(&r) {
        r as i32
    } else {
        i32::MIN
    };
    let k = e as f64;
    let mut x0;
    if use_fma() {
        x0 = (-k).mul_add(LN2U, x); // VFNMADD231SD
        x0 = (-k).mul_add(LN2L, x0);
        x0 *= 0.0625;
        let mut x1 = C[8];
        x1 = x1.mul_add(x0, C[7]); // VFMADD213SD
        x1 = x1.mul_add(x0, C[6]);
        x1 = x1.mul_add(x0, C[5]);
        x1 = x1.mul_add(x0, C[4]);
        x1 = x1.mul_add(x0, C[3]);
        x1 = x1.mul_add(x0, C[0]);
        x1 = x1.mul_add(x0, C[1]);
        x0 *= x1;
        x1 = x0 + C[2];
        x0 *= x1;
        x1 = x0 + C[2];
        x0 *= x1;
        x1 = x0 + C[2];
        x0 *= x1;
        x1 = x0 + C[2];
        x0 = x1.mul_add(x0, C[1]); // VFMADD213SD 1.0, X1, X0
    } else {
        x0 = x - LN2U * k;
        x0 -= LN2L * k;
        x0 *= 0.0625;
        let mut x1 = C[8];
        for c in [C[7], C[6], C[5], C[4], C[3], C[0], C[1]] {
            x1 *= x0;
            x1 += c;
        }
        x0 *= x1;
        for _ in 0..4 {
            let x1 = C[2] + x0;
            x0 *= x1;
        }
        x0 += C[1];
    }
    // return fr * 2**exponent
    let mut bxe = e.wrapping_add(0x3FF);
    if bxe <= 0 {
        // denormal
        if bxe < -52 {
            return 0.0; // underflow
        }
        bxe += 0x3FE;
        x0 *= f64::from_bits((bxe as u32 as u64) << 52);
        bxe = 1;
    } else if bxe >= 0x7FF {
        return f64::INFINITY;
    }
    x0 * f64::from_bits((bxe as u32 as u64) << 52)
}

/// `math.Log`: the natural logarithm of x. `Log(+Inf) = +Inf`, `Log(0) = -Inf`,
/// `Log(x < 0) = NaN`, `Log(NaN) = NaN`.
///
/// amd64 uses assembly (log_amd64.s: no FMA, its own `Frexp` that does not normalize
/// subnormals, and `f1 <= Sqrt2/2` instead of `<`); arm64 runs the pure-Go code, where gc
/// fuses multiply-adds (log.go:124-128).
// go-std: math.Log L1 (archLog on amd64; math.log elsewhere)
pub fn log(x: f64) -> f64 {
    #[cfg(target_arch = "x86_64")]
    {
        log_amd64(x)
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        log_go(x)
    }
}

/// The pure-Go `log` (arm64: the fused positions read from `go tool objdump`).
#[allow(clippy::excessive_precision, dead_code)]
// go-std: math.log L1 [M-FMA] (log.go:124-128)
fn log_go(x: f64) -> f64 {
    const LN2HI: f64 = 6.93147180369123816490e-01; /* 3fe62e42 fee00000 */
    const LN2LO: f64 = 1.90821492927058770002e-10; /* 3dea39ef 35793c76 */
    const L1: f64 = 6.666666666666735130e-01; /* 3FE55555 55555593 */
    const L2: f64 = 3.999999999940941908e-01; /* 3FD99999 9997FA04 */
    const L3: f64 = 2.857142874366239149e-01; /* 3FD24924 94229359 */
    const L4: f64 = 2.222219843214978396e-01; /* 3FCC71C5 1D8E78AF */
    const L5: f64 = 1.818357216161805012e-01; /* 3FC74664 96CB03DE */
    const L6: f64 = 1.531383769920937332e-01; /* 3FC39A09 D078C69F */
    const L7: f64 = 1.479819860511658591e-01; /* 3FC2F112 DF3E5244 */
    const HSQRT2: f64 = f64::from_bits(0x3fe6a09e667f3bcd); // Sqrt2/2
    // special cases
    if x.is_nan() || x == f64::INFINITY {
        return x;
    }
    if x < 0.0 {
        return nan();
    }
    if x == 0.0 {
        return f64::NEG_INFINITY;
    }
    // reduce
    let (mut f1, mut ki) = frexp(x);
    if f1 < HSQRT2 {
        f1 *= 2.0;
        ki -= 1;
    }
    let f = f1 - 1.0;
    let k = ki as f64;
    // compute
    let s = f / (2.0 + f);
    let s2 = s * s;
    let s4 = s2 * s2;
    #[cfg(target_arch = "aarch64")]
    {
        // [M-FMA] log.go:124-128: the Horner steps; t1 + t2 is s2*inner1 + t2; hfsq is
        // folded into hfsq + R and hfsq - (...); the last subtraction from k*Ln2Hi.
        let inner1 = s4.mul_add(s4.mul_add(s4.mul_add(L7, L5), L3), L1);
        let t2 = s4 * s4.mul_add(s4.mul_add(L6, L4), L2);
        let r = s2.mul_add(inner1, t2);
        let h = 0.5 * f;
        let hr = h.mul_add(f, r); // hfsq + R
        let a = s.mul_add(hr, k * LN2LO); // s*(hfsq+R) + k*Ln2Lo
        let b = h.mul_add(f, -a); // hfsq - (...)
        let c = b - f;
        k.mul_add(LN2HI, -c)
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        let t1 = s2 * (L1 + s4 * (L3 + s4 * (L5 + s4 * L7)));
        let t2 = s4 * (L2 + s4 * (L4 + s4 * L6));
        let r = t1 + t2;
        let hfsq = 0.5 * f * f;
        k * LN2HI - ((hfsq - (s * (hfsq + r) + k * LN2LO)) - f)
    }
}

/// amd64 `archLog` (log_amd64.s).
#[cfg(target_arch = "x86_64")]
#[allow(clippy::excessive_precision)]
// go-std: math.archLog L1 (log_amd64.s, instruction by instruction)
fn log_amd64(x: f64) -> f64 {
    const HSQRT2: f64 = f64::from_bits(0x3fe6a09e667f3bcd); // 7.07106781186547524401e-01
    const LN2HI: f64 = 6.93147180369123816490e-01;
    const LN2LO: f64 = 1.90821492927058770002e-10;
    const L1: f64 = 6.666666666666735130e-01;
    const L2: f64 = 3.999999999940941908e-01;
    const L3: f64 = 2.857142874366239149e-01;
    const L4: f64 = 2.222219843214978396e-01;
    const L5: f64 = 1.818357216161805012e-01;
    const L6: f64 = 1.531383769920937332e-01;
    const L7: f64 = 1.479819860511658591e-01;
    let bx = x.to_bits();
    if bx & !(1u64 << 63) == 0 {
        return f64::NEG_INFINITY; // ±0
    }
    if (bx as i64) < 0 {
        return nan(); // negative (also a negative NaN)
    }
    if bx >= 0x7FF0000000000000 {
        return x; // +Inf or NaN
    }
    // f1, ki := math.Frexp(x) (the exponent field as is: subnormals are not normalized)
    let mut f1 = f64::from_bits((bx & 0x000FFFFFFFFFFFFF) | 0.5f64.to_bits());
    let mut kf = ((((bx >> 52) as i32) & 0x7FF) - 0x3FE) as f64;
    // if f1 <= math.Sqrt2/2 { k -= 1; f1 *= 2 } (CMPSD ..., 5 is "not less than":
    // HSqrt2 >= f1, where the pure-Go code tests f1 < Sqrt2/2)
    if f1 <= HSQRT2 {
        kf -= 1.0;
        f1 *= 2.0;
    }
    let f = f1 - 1.0;
    let s = f / (2.0 + f);
    let s2 = s * s;
    let s4 = s2 * s2;
    let t1 = s2 * (((L7 * s4 + L5) * s4 + L3) * s4 + L1);
    let t2 = s4 * ((L6 * s4 + L4) * s4 + L2);
    let r = t1 + t2;
    let hfsq = 0.5 * f * f;
    let x3 = s * (hfsq + r) + kf * LN2LO;
    let x0 = (hfsq - x3) - f;
    kf * LN2HI - x0
}

/// `math.Log2`: the binary logarithm of x (exact for powers of two).
// go-std: math.log2 L1 [M-FMA] (log10.go:36: `Log(frac)*(1/Ln2) + float64(exp)` is FMADDD on arm64)
pub fn log2(x: f64) -> f64 {
    const INV_LN2: f64 = f64::from_bits(0x3ff71547652b82fe); // 1/Ln2
    let (frac, exp) = frexp(x);
    // Make sure exact powers of two give an exact answer.
    // Don't depend on Log(0.5)*(1/Ln2)+exp being exactly exp-1.
    if frac == 0.5 {
        return (exp - 1) as f64;
    }
    fma_arm64(log(frac), INV_LN2, exp as f64)
}

/// `math.Log10`: the decimal logarithm of x.
// go-std: math.log10 L1
pub fn log10(x: f64) -> f64 {
    const INV_LN10: f64 = f64::from_bits(0x3fdbcb7b1526e50e); // 1/Ln10
    log(x) * INV_LN10
}

/// `isOddInt`: whether x is an odd integer.
// go-std: math.isOddInt L1
fn is_odd_int(x: f64) -> bool {
    if abs(x) >= (1u64 << 53) as f64 {
        // 1 << 53 is the largest exact integer in the float64 format.
        return false;
    }
    let (xi, xf) = modf(x);
    xf == 0.0 && (xi as i64) & 1 == 1
}

/// `math.Pow`: x**y, the base-x exponential of y (Go's special cases; `Exp(yf*Log(x))` for
/// the fractional part and repeated squaring for the integer part).
// go-std: math.pow L1 [M-FMA] (pow.go:153: `x1 += x1` after `x1 *= x1` is FMADDD x1old, x1old, x1 on arm64)
pub fn pow(x: f64, y: f64) -> f64 {
    if y == 0.0 || x == 1.0 {
        return 1.0;
    }
    if y == 1.0 {
        return x;
    }
    if x.is_nan() || y.is_nan() {
        return nan();
    }
    if x == 0.0 {
        if y < 0.0 {
            if x.is_sign_negative() && is_odd_int(y) {
                return f64::NEG_INFINITY;
            }
            return f64::INFINITY;
        }
        if y > 0.0 {
            if x.is_sign_negative() && is_odd_int(y) {
                return x;
            }
            return 0.0;
        }
    }
    if y.is_infinite() {
        if x == -1.0 {
            return 1.0;
        }
        if (abs(x) < 1.0) == (y == f64::INFINITY) {
            return 0.0;
        }
        return f64::INFINITY;
    }
    if x.is_infinite() {
        if x == f64::NEG_INFINITY {
            return pow(1.0 / x, -y); // Pow(-0, -y)
        }
        if y < 0.0 {
            return 0.0;
        }
        if y > 0.0 {
            return f64::INFINITY;
        }
    }
    if y == 0.5 {
        return x.sqrt();
    }
    if y == -0.5 {
        return 1.0 / x.sqrt();
    }

    let (mut yi, mut yf) = modf(abs(y));
    if yf != 0.0 && x < 0.0 {
        return nan();
    }
    if yi >= (1u64 << 63) as f64 {
        // yi is a large even int that will lead to overflow (or underflow to 0)
        // for all x except -1 (x == 1 was handled earlier)
        if x == -1.0 {
            return 1.0;
        }
        if (abs(x) < 1.0) == (y > 0.0) {
            return 0.0;
        }
        return f64::INFINITY;
    }

    // ans = a1 * 2**ae (= 1 for now).
    let mut a1 = 1.0;
    let mut ae: i64 = 0;

    // ans *= x**yf
    if yf != 0.0 {
        if yf > 0.5 {
            yf -= 1.0;
            yi += 1.0;
        }
        a1 = exp(yf * log(x));
    }

    // ans *= x**yi by multiplying in successive squarings of x according to bits of yi.
    let (mut x1, mut xe) = frexp(x);
    let mut i = yi as i64;
    while i != 0 {
        if !(-(1 << 12)..=(1 << 12)).contains(&xe) {
            // catch xe before it overflows the left shift below
            ae += xe;
            break;
        }
        if i & 1 == 1 {
            a1 *= x1;
            ae += xe;
        }
        #[cfg_attr(not(target_arch = "aarch64"), allow(unused_variables))]
        let old = x1;
        x1 *= x1;
        xe <<= 1;
        if x1 < 0.5 {
            // [M-FMA] pow.go:153: arm64 computes `x1 += x1` as FMADDD old, old, x1
            #[cfg(target_arch = "aarch64")]
            {
                x1 = old.mul_add(old, x1);
            }
            #[cfg(not(target_arch = "aarch64"))]
            {
                x1 += x1;
            }
            xe -= 1;
        }
        i >>= 1;
    }

    // ans = a1*2**ae; if y < 0 { ans = 1 / ans } but in the opposite order
    if y < 0.0 {
        a1 = 1.0 / a1;
        ae = -ae;
    }
    ldexp(a1, ae)
}

/// `math.Cos`: the cosine of the radian argument x. `Cos(±Inf) = NaN`, `Cos(NaN) = NaN`.
// go-std: math.cos L1 [M-FMA] (sin.go:155-170; math.Cos: haveArchCos is false on amd64 and arm64)
pub fn cos(x: f64) -> f64 {
    const PI4A: f64 = f64::from_bits(0x3fe921fb40000000);
    const PI4B: f64 = f64::from_bits(0x3e64442d00000000);
    const PI4C: f64 = f64::from_bits(0x3ce8469898cc5170);
    // special cases
    if x.is_nan() || x.is_infinite() {
        return nan();
    }
    // make argument positive
    let mut sign = false;
    let x = abs(x);
    let mut j: u64;
    let mut y: f64;
    let z: f64;
    if x >= REDUCE_THRESHOLD {
        (j, z) = trig_reduce(x);
    } else {
        j = super::conv::f64_to_u64(x * (4.0 / std::f64::consts::PI)); // [M-F2I]
        y = j as f64;
        // map zeros to origin
        if j & 1 == 1 {
            j += 1;
            y += 1.0;
        }
        j &= 7; // octant modulo 2Pi radians (360 degrees)
        // [M-FMA] sin.go:155
        z = fms_arm64(fms_arm64(fms_arm64(x, y, PI4A), y, PI4B), y, PI4C);
    }
    if j > 3 {
        j -= 4;
        sign = !sign;
    }
    if j > 1 {
        sign = !sign;
    }
    let zz = z * z;
    if j == 1 || j == 2 {
        // [M-FMA] sin.go:168
        let poly = fma_arm64(
            fma_arm64(
                fma_arm64(
                    fma_arm64(fma_arm64(SIN[0], zz, SIN[1]), zz, SIN[2]),
                    zz,
                    SIN[3],
                ),
                zz,
                SIN[4],
            ),
            zz,
            SIN[5],
        );
        y = fma_arm64(z * zz, poly, z);
    } else {
        // [M-FMA] sin.go:170
        let poly = fma_arm64(
            fma_arm64(
                fma_arm64(
                    fma_arm64(fma_arm64(COS[0], zz, COS[1]), zz, COS[2]),
                    zz,
                    COS[3],
                ),
                zz,
                COS[4],
            ),
            zz,
            COS[5],
        );
        y = fma_arm64(zz * zz, poly, fms_arm64(1.0, 0.5, zz));
    }
    if sign {
        y = -y;
    }
    y
}

/// `math.Tan`: the tangent of the radian argument x. `Tan(±0) = ±0`, `Tan(±Inf) = NaN`.
#[allow(clippy::excessive_precision)]
// go-std: math.tan L1 [M-FMA] (tan.go:124-129; math.Tan: haveArchTan is false on amd64 and arm64)
pub fn tan(x: f64) -> f64 {
    const PI4A: f64 = f64::from_bits(0x3fe921fb40000000);
    const PI4B: f64 = f64::from_bits(0x3e64442d00000000);
    const PI4C: f64 = f64::from_bits(0x3ce8469898cc5170);
    const TAN_P: [f64; 3] = [
        -1.30936939181383777646e4,
        1.15351664838587416140e6,
        -1.79565251976484877988e7,
    ];
    const TAN_Q: [f64; 5] = [
        1.00000000000000000000e0,
        1.36812963470692954678e4,
        -1.32089234440210967447e6,
        2.50083801823357915839e7,
        -5.38695755929454629881e7,
    ];
    // special cases
    if x == 0.0 || x.is_nan() {
        return x; // return ±0 || NaN()
    }
    if x.is_infinite() {
        return nan();
    }
    // make argument positive but save the sign
    let mut sign = false;
    let mut x = x;
    if x < 0.0 {
        x = -x;
        sign = true;
    }
    let mut j: u64;
    let mut y: f64;
    let z: f64;
    if x >= REDUCE_THRESHOLD {
        (j, z) = trig_reduce(x);
    } else {
        j = super::conv::f64_to_u64(x * (4.0 / std::f64::consts::PI)); // [M-F2I]
        y = j as f64;
        /* map zeros and singularities to origin */
        if j & 1 == 1 {
            j += 1;
            y += 1.0;
        }
        // [M-FMA] tan.go:124
        z = fms_arm64(fms_arm64(fms_arm64(x, y, PI4A), y, PI4B), y, PI4C);
    }
    let zz = z * z;
    if zz > 1e-14 {
        // [M-FMA] tan.go:129: the Horner steps, `zz+Q[1]` as z*z+Q[1] fused, and
        // z + z*ratio.
        let num = zz * fma_arm64(fma_arm64(TAN_P[0], zz, TAN_P[1]), zz, TAN_P[2]);
        #[cfg(target_arch = "aarch64")]
        let q1 = z.mul_add(z, TAN_Q[1]);
        #[cfg(not(target_arch = "aarch64"))]
        let q1 = zz + TAN_Q[1];
        let den = fma_arm64(
            fma_arm64(fma_arm64(q1, zz, TAN_Q[2]), zz, TAN_Q[3]),
            zz,
            TAN_Q[4],
        );
        let _ = TAN_Q[0];
        y = fma_arm64(z, num / den, z);
    } else {
        y = z;
    }
    if j & 2 == 2 {
        y = -1.0 / y;
    }
    if sign {
        y = -y;
    }
    y
}

/// `math.Acos`: the arccosine, in radians, of x. `Acos(x) = NaN` if x < -1 or x > 1.
// go-std: math.acos L1
pub fn acos(x: f64) -> f64 {
    std::f64::consts::PI / 2.0 - asin(x)
}

/// `math.Atan`: the arctangent, in radians, of x. `Atan(±0) = ±0`, `Atan(±Inf) = ±Pi/2`.
// go-std: math.atan L1
pub fn atan(x: f64) -> f64 {
    if x == 0.0 {
        return x;
    }
    if x > 0.0 {
        return satan(x);
    }
    -satan(-x)
}

/// `math.Atan2`: the arc tangent of y/x, using the signs of the two to determine the
/// quadrant of the return value.
// go-std: math.atan2 L1
pub fn atan2(y: f64, x: f64) -> f64 {
    use std::f64::consts::PI;
    // special cases
    if y.is_nan() || x.is_nan() {
        return nan();
    }
    if y == 0.0 {
        if x >= 0.0 && !x.is_sign_negative() {
            return copysign(0.0, y);
        }
        return copysign(PI, y);
    }
    if x == 0.0 {
        return copysign(PI / 2.0, y);
    }
    if x.is_infinite() {
        if x == f64::INFINITY {
            if y.is_infinite() {
                return copysign(PI / 4.0, y);
            }
            return copysign(0.0, y);
        }
        if y.is_infinite() {
            return copysign(f64::from_bits(0x4002d97c7f3321d2), y); // 3*Pi/4
        }
        return copysign(PI, y);
    }
    if y.is_infinite() {
        return copysign(PI / 2.0, y);
    }
    // Call atan and determine the quadrant.
    let q = atan(y / x);
    if x < 0.0 {
        if q <= 0.0 {
            return q + PI;
        }
        return q - PI;
    }
    q
}
