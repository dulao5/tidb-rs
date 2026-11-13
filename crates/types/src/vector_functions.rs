//! go-file: pkg/types/vector_functions.go
//!
//! Distances and arithmetic of `VectorFloat32`. The sums `distance += x * y` are
//! multiply-adds that Go gc fuses into one FMA instruction on some targets only (mapping
//! rule `M-FMA`, tracker `data/go_fma_sites.tsv`, go1.25.12):
//!
//! | Go line | function | amd64 v1 (baseline) | arm64, ppc64le, riscv64, loong64 |
//! |---|---|---|---|
//! | 44 | L2SquaredDistance | not fused | `FMADDS` |
//! | 71 | InnerProduct | not fused | `FMADDS` |
//! | 100, 101, 102 | CosineDistance | not fused | `FMADDS` |
//! | 152 | L2Norm (float64) | not fused | `FMADDD` |
//!
//! **NaN results.** Which NaN comes out of an operation with NaN operands depends on the
//! hardware rule and on which register gc puts each operand in, and LLVM may order the
//! operands of commutative operations (and the two multiplicands of `fma`) differently
//! (tidb-rs PR #16: x86_64 CI, and Lucky's arm64 review):
//!
//! - x86 SSE (`ADDSS`/`MULSS`, amd64): with two NaN operands the result is the first source,
//!   the destination register, quieted; with one, that NaN quieted.
//! - ARM (`FADDS`, `FMADDS`, arm64, FPCR.DN = 0): the first signalling NaN in operand order
//!   (`FMADD`: addend, Rn, Rm; two operands: Rn, Rm), quieted; if none, the first quiet NaN
//!   in the same order; and a quiet-NaN addend with an infinite × zero product gives the
//!   default NaN `0x7FC00000` (`FPMulAdd`).
//! - An invalid operation without NaN operands (Inf − Inf, 0 × Inf) gives the hardware's
//!   default NaN: x86 `0xFFC00000`, ARM `0x7FC00000`; Rust gets it from the same hardware.
//!
//! gc's operand order, from `go build -gcflags=-S` of `pkg/types` (go1.25.12):
//!
//! | Go line | amd64 | arm64 |
//! |---|---|---|
//! | 44 `distance += diff * diff` | same operand | `FMADDS diff, acc, diff, acc` |
//! | 71 `distance += va[i] * vb[i]` | `MULSS vb, va` (va first), `ADDSS prod, acc` | `FMADDS va, acc, vb, acc`: Rn = vb, Rm = va |
//! | 100 `distance += va[i] * vb[i]` | as line 71 | `FMADDS vb, acc, va, acc`: Rn = va, Rm = vb |
//! | 101, 102, 152 | same operand | same operand |
//! | 137 `distance += diff` (L1) | `ADDSS diff, acc` (acc first) | `FADDS diff, acc, acc`: Rn = acc |
//!
//! The subtractions (`va[i] - vb[i]`) keep their operand order in LLVM as in gc, so the
//! hardware applies the rule itself. The distance loops run twice when needed: a fast loop
//! with plain operations, and only if its result is NaN, the `EXACT` loop whose helpers
//! ([`add32`], [`mul32`], [`madd32`], [`madd64`]) choose the NaN by the rules above. Data
//! that Go creates never contains NaN (parse and create reject it), so the fast loop is the
//! normal path. `CosineDistance` needs no `EXACT` loop: a NaN sum returns Go's canonical
//! `math.NaN()`.

use tidb_base::gofmt::Arg;
use tidb_base::gostd::math as gomath;
use tidb_base::{Error, errors};

use crate::vector::{VectorFloat32, init_vector_float32};

#[cfg(any(
    target_arch = "aarch64",
    all(target_arch = "powerpc64", target_endian = "little"),
    target_arch = "riscv64",
    target_arch = "loongarch64"
))]
macro_rules! fused {
    ($fused:expr, $unfused:expr) => {
        $fused
    };
}
#[cfg(not(any(
    target_arch = "aarch64",
    all(target_arch = "powerpc64", target_endian = "little"),
    target_arch = "riscv64",
    target_arch = "loongarch64"
)))]
macro_rules! fused {
    ($fused:expr, $unfused:expr) => {
        $unfused
    };
}

/// `z + x*y` as gc compiles it at the fused sites of this file (see the module table):
/// one `fmadd` where gc fuses, a separate multiply and add elsewhere (Rust never contracts
/// by itself). `ARM_YX`: on arm64 gc puts `y` in Rn and `x` in Rm (line 71); otherwise `x`
/// in Rn. With `EXACT`, NaN operands give gc's NaN (module doc).
// glue: the multiply-add of the M-FMA sites (module table)
#[inline(always)]
fn madd32<const EXACT: bool, const ARM_YX: bool>(z: f32, x: f32, y: f32) -> f32 {
    fused!(
        {
            #[cfg(target_arch = "aarch64")]
            if EXACT {
                let (n, m) = if ARM_YX { (y, x) } else { (x, y) };
                if let Some(r) = arm_fmadd_nan32(z, n, m) {
                    return r;
                }
            }
            x.mul_add(y, z)
        },
        add32::<EXACT>(z, mul32::<EXACT>(x, y))
    )
}

/// [`madd32`] for the float64 sum of `L2Norm` (line 152, the same operand twice).
// glue: the multiply-add of the M-FMA site of L2Norm (module table)
#[inline(always)]
fn madd64<const EXACT: bool>(z: f64, x: f64, y: f64) -> f64 {
    fused!(
        {
            #[cfg(target_arch = "aarch64")]
            if EXACT && let Some(r) = arm_fmadd_nan64(z, x, y) {
                return r;
            }
            x.mul_add(y, z)
        },
        add64::<EXACT>(z, x * y)
    )
}

/// `a + b` where Go accumulates `a += b`: gc's first operand is `a` on both targets (amd64
/// destination, arm64 Rn). With `EXACT`, NaN operands give gc's NaN.
// glue: NaN propagation of gc's operand order
#[inline(always)]
fn add32<const EXACT: bool>(a: f32, b: f32) -> f32 {
    if EXACT {
        #[cfg(target_arch = "x86_64")]
        if a.is_nan() && b.is_nan() {
            return quiet32(a);
        }
        #[cfg(target_arch = "aarch64")]
        if let Some(r) = arm_nan2_32(a, b) {
            return r;
        }
    }
    a + b
}

/// `x * y` as gc emits it on amd64 (`MULSS y, x`: `x` is the destination); see [`add32`].
// glue: NaN propagation of gc's operand order (x86_64)
#[inline(always)]
#[allow(dead_code)]
fn mul32<const EXACT: bool>(x: f32, y: f32) -> f32 {
    #[cfg(target_arch = "x86_64")]
    if EXACT && x.is_nan() && y.is_nan() {
        return quiet32(x);
    }
    x * y
}

/// [`add32`] for float64 (`ADDSD`).
// glue: NaN propagation of gc's operand order (x86_64)
#[inline(always)]
#[allow(dead_code)]
fn add64<const EXACT: bool>(a: f64, b: f64) -> f64 {
    #[cfg(target_arch = "x86_64")]
    if EXACT && a.is_nan() && b.is_nan() {
        return quiet64(a);
    }
    a + b
}

/// A NaN quieted as the hardware does (the quiet bit set, payload and sign kept).
// glue: NaN quieting of the hardware rules (module doc)
#[inline(always)]
#[allow(dead_code)]
fn quiet32(v: f32) -> f32 {
    f32::from_bits(v.to_bits() | 0x0040_0000)
}

/// [`quiet32`] for float64.
// glue: NaN quieting of the hardware rules (module doc)
#[inline(always)]
#[allow(dead_code)]
fn quiet64(v: f64) -> f64 {
    f64::from_bits(v.to_bits() | 0x0008_0000_0000_0000)
}

/// ARM two-operand NaN rule (`FPProcessNaNs`): the first signalling NaN of (n, m),
/// quieted, else the first quiet NaN; `None` without NaN operands.
// glue: ARM NaN propagation (FPProcessNaNs)
#[cfg(target_arch = "aarch64")]
#[inline(always)]
fn arm_nan2_32(n: f32, m: f32) -> Option<f32> {
    let snan = |v: f32| v.is_nan() && v.to_bits() & 0x0040_0000 == 0;
    if snan(n) {
        Some(quiet32(n))
    } else if snan(m) {
        Some(quiet32(m))
    } else if n.is_nan() {
        Some(n)
    } else if m.is_nan() {
        Some(m)
    } else {
        None
    }
}

/// ARM `FMADD` NaN rule (`FPMulAdd`: `FPProcessNaNs3` over addend, Rn, Rm; then a
/// quiet-NaN addend with an infinite × zero product gives the default NaN).
// glue: ARM NaN propagation (FPMulAdd)
#[cfg(target_arch = "aarch64")]
#[inline(always)]
fn arm_fmadd_nan32(a: f32, n: f32, m: f32) -> Option<f32> {
    let snan = |v: f32| v.is_nan() && v.to_bits() & 0x0040_0000 == 0;
    let r = if snan(a) {
        quiet32(a)
    } else if snan(n) {
        quiet32(n)
    } else if snan(m) {
        quiet32(m)
    } else if a.is_nan() {
        a
    } else if n.is_nan() {
        n
    } else if m.is_nan() {
        m
    } else {
        return None;
    };
    if a.is_nan() && !snan(a) && ((n.is_infinite() && m == 0.0) || (n == 0.0 && m.is_infinite())) {
        return Some(f32::from_bits(0x7fc0_0000));
    }
    Some(r)
}

/// [`arm_fmadd_nan32`] for float64 (`FMADDD`).
// glue: ARM NaN propagation (FPMulAdd)
#[cfg(target_arch = "aarch64")]
#[inline(always)]
fn arm_fmadd_nan64(a: f64, n: f64, m: f64) -> Option<f64> {
    let snan = |v: f64| v.is_nan() && v.to_bits() & 0x0008_0000_0000_0000 == 0;
    let r = if snan(a) {
        quiet64(a)
    } else if snan(n) {
        quiet64(n)
    } else if snan(m) {
        quiet64(m)
    } else if a.is_nan() {
        a
    } else if n.is_nan() {
        n
    } else if m.is_nan() {
        m
    } else {
        return None;
    };
    if a.is_nan() && !snan(a) && ((n.is_infinite() && m == 0.0) || (n == 0.0 && m.is_infinite())) {
        return Some(f64::from_bits(0x7ff8_0000_0000_0000));
    }
    Some(r)
}

// glue: the error of VectorFloat32.checkIdenticalDims, out of line (cold path)
#[cold]
#[inline(never)]
fn different_dims(a: usize, b: usize) -> Error {
    errors::errorf(
        "vectors have different dimensions: %d and %d",
        &[Arg::from(a), Arg::from(b)],
    )
}

impl VectorFloat32<'_> {
    // go: VectorFloat32.checkIdenticalDims L1 [M-ERR]
    #[inline]
    fn check_identical_dims(&self, b: &VectorFloat32<'_>) -> Result<(), Error> {
        if self.len() != b.len() {
            return Err(different_dims(self.len(), b.len()));
        }
        Ok(())
    }

    /// L2SquaredDistance returns the squared L2 distance between two vectors.
    /// This saves a sqrt calculation.
    // go: VectorFloat32.L2SquaredDistance L1 [M-ERR, M-FMA]
    #[inline]
    pub fn l2_squared_distance(&self, b: &VectorFloat32<'_>) -> Result<f64, Error> {
        self.check_identical_dims(b)?;

        let va = self.elements();
        let vb = b.elements();
        let run = |exact: bool| {
            let mut distance: f32 = 0.0;
            for (x, y) in va.iter().zip(vb.iter()) {
                // Hope this can be vectorized.
                let diff = x - y;
                distance = if exact {
                    madd32::<true, false>(distance, diff, diff) // [M-FMA] vector_functions.go:44
                } else {
                    madd32::<false, false>(distance, diff, diff) // [M-FMA] vector_functions.go:44
                };
            }
            distance
        };
        let d = run(false);
        let distance = if d.is_nan() { run(true) } else { d };

        Ok(distance as f64)
    }

    /// L2Distance returns the L2 distance between two vectors.
    // go: VectorFloat32.L2Distance L1 [M-ERR]
    #[inline]
    pub fn l2_distance(&self, b: &VectorFloat32<'_>) -> Result<f64, Error> {
        let d = self.l2_squared_distance(b)?;
        Ok(d.sqrt())
    }

    /// InnerProduct returns the inner product of two vectors.
    // go: VectorFloat32.InnerProduct L1 [M-ERR, M-FMA]
    #[inline]
    pub fn inner_product(&self, b: &VectorFloat32<'_>) -> Result<f64, Error> {
        self.check_identical_dims(b)?;

        let va = self.elements();
        let vb = b.elements();
        let run = |exact: bool| {
            let mut distance: f32 = 0.0;
            for (x, y) in va.iter().zip(vb.iter()) {
                // Hope this can be vectorized.
                distance = if exact {
                    madd32::<true, true>(distance, x, y) // [M-FMA] vector_functions.go:71
                } else {
                    madd32::<false, true>(distance, x, y) // [M-FMA] vector_functions.go:71
                };
            }
            distance
        };
        let d = run(false);
        let distance = if d.is_nan() { run(true) } else { d };

        Ok(distance as f64)
    }

    /// NegativeInnerProduct returns the negative inner product of two vectors.
    // go: VectorFloat32.NegativeInnerProduct L1 [M-ERR]
    #[inline]
    pub fn negative_inner_product(&self, b: &VectorFloat32<'_>) -> Result<f64, Error> {
        let d = self.inner_product(b)?;
        // `d * -1`: gc rewrites a multiplication by the constant -1 into a negation (the
        // sign bit flips, also for NaN; a multiplication would keep a NaN's sign).
        Ok(-d)
    }

    /// CosineDistance returns the cosine distance between two vectors.
    // go: VectorFloat32.CosineDistance L1 [M-ERR, M-FMA]
    #[inline]
    pub fn cosine_distance(&self, b: &VectorFloat32<'_>) -> Result<f64, Error> {
        self.check_identical_dims(b)?;

        let mut distance: f32 = 0.0;
        let mut norma: f32 = 0.0;
        let mut normb: f32 = 0.0;
        let va = self.elements();
        let vb = b.elements();

        for (x, y) in va.iter().zip(vb.iter()) {
            // Hope this can be vectorized.
            distance = madd32::<false, false>(distance, x, y); // [M-FMA] vector_functions.go:100
            norma = madd32::<false, false>(norma, x, x); // [M-FMA] vector_functions.go:101
            normb = madd32::<false, false>(normb, y, y); // [M-FMA] vector_functions.go:102
        }

        let mut similarity = distance as f64 / (norma as f64 * normb as f64).sqrt();

        if similarity.is_nan() {
            // Divide by zero
            return Ok(gomath::nan());
        }

        // Go: `if similarity > 1.0 { similarity = 1.0 } else if similarity < -1.0 { ... }`
        // (NaN returned above, so clamp is the same).
        similarity = similarity.clamp(-1.0, 1.0);

        Ok(1.0 - similarity)
    }

    /// L1Distance returns the L1 distance between two vectors.
    // go: VectorFloat32.L1Distance L1 [M-ERR]
    #[inline]
    pub fn l1_distance(&self, b: &VectorFloat32<'_>) -> Result<f64, Error> {
        self.check_identical_dims(b)?;

        let va = self.elements();
        let vb = b.elements();
        let run = |exact: bool| {
            let mut distance: f32 = 0.0;
            for (x, y) in va.iter().zip(vb.iter()) {
                // Hope this can be vectorized.
                let mut diff = x - y;
                if diff < 0.0 {
                    diff = -diff;
                }
                distance = if exact {
                    add32::<true>(distance, diff)
                } else {
                    add32::<false>(distance, diff)
                };
            }
            distance
        };
        let d = run(false);
        let distance = if d.is_nan() { run(true) } else { d };

        Ok(distance as f64)
    }

    /// L2Norm returns the L2 norm of the vector.
    // go: VectorFloat32.L2Norm L1 [M-FMA]
    pub fn l2_norm(&self) -> f64 {
        // Note: We align the impl with pgvector: Only l2_norm use double
        // precision during calculation.
        let run = |exact: bool| {
            let mut norm: f64 = 0.0;
            for x in self.elements().iter() {
                // Hope this can be vectorized.
                norm = if exact {
                    madd64::<true>(norm, x as f64, x as f64) // [M-FMA] vector_functions.go:152
                } else {
                    madd64::<false>(norm, x as f64, x as f64) // [M-FMA] vector_functions.go:152
                };
            }
            norm
        };
        let n = run(false);
        let norm = if n.is_nan() { run(true) } else { n };
        norm.sqrt()
    }

    /// The element-wise operation of Add, Sub and Mul, with their result checks.
    // go-part: VectorFloat32.Add
    fn elementwise(
        &self,
        b: &VectorFloat32<'_>,
        op: impl Fn(f32, f32) -> f32,
    ) -> Result<VectorFloat32<'static>, Error> {
        self.check_identical_dims(b)?;

        let mut result = init_vector_float32(self.len());

        let va = self.elements();
        let vb = b.elements();
        let mut vr = result.elements_mut();

        for (i, (x, y)) in va.iter().zip(vb.iter()).enumerate() {
            // Hope this can be vectorized.
            vr.set(i, op(x, y));
        }
        for i in 0..vr.len() {
            let r = vr.get(i) as f64;
            if r.is_infinite() {
                return Err(errors::errorf("value out of range: overflow", &[]));
            }
            if r.is_nan() {
                return Err(errors::errorf("value out of range: NaN", &[]));
            }
        }

        Ok(result)
    }

    /// Add adds two vectors. The vectors must have the same dimension.
    // go: VectorFloat32.Add L1 [M-ERR]
    pub fn add(&self, b: &VectorFloat32<'_>) -> Result<VectorFloat32<'static>, Error> {
        self.elementwise(b, |x, y| x + y)
    }

    /// Sub subtracts two vectors. The vectors must have the same dimension.
    // go: VectorFloat32.Sub L1 [M-ERR]
    pub fn sub(&self, b: &VectorFloat32<'_>) -> Result<VectorFloat32<'static>, Error> {
        self.elementwise(b, |x, y| x - y)
    }

    /// Mul multiplies two vectors. The vectors must have the same dimension.
    // go: VectorFloat32.Mul L1 [M-ERR]
    pub fn mul(&self, b: &VectorFloat32<'_>) -> Result<VectorFloat32<'static>, Error> {
        // TODO: Check for underflow.
        // See https://github.com/pgvector/pgvector/blob/81d13bd40f03890bb5b6360259628cd473c2e467/src/vector.c#L873
        self.elementwise(b, |x, y| x * y)
    }

    /// Compare returns an integer comparing two vectors. The result will be 0 if a==b, -1 if a < b, and +1 if a > b.
    // go: VectorFloat32.Compare L1
    pub fn compare(&self, b: &VectorFloat32<'_>) -> isize {
        let la = self.len();
        let lb = b.len();
        let common_len = la.min(lb);

        let va = self.elements();
        let vb = b.elements();

        for i in 0..common_len {
            if va.get(i) < vb.get(i) {
                return -1;
            } else if va.get(i) > vb.get(i) {
                return 1;
            }
        }
        if la < lb {
            return -1;
        } else if la > lb {
            return 1;
        }
        0
    }
}
