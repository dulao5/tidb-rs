//! go-file: $GOROOT/src/math/bits.go
//! go-file: $GOROOT/src/math/pow10.go
//! go-file: $GOROOT/src/math/dim.go
//! go-file: $GOROOT/src/math/sin.go
//! go-file: $GOROOT/src/math/trig_reduce.go
//! go-file: $GOROOT/src/math/asin.go
//! go-file: $GOROOT/src/math/atan.go
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
