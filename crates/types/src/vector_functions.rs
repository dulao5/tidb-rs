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
//! [`madd32`] / [`madd64`] follow Go per target: `mul_add` (one `fmadd` instruction) where
//! gc fuses, a separate multiply and add elsewhere (Rust never contracts by itself).

use tidb_base::gofmt::Arg;
use tidb_base::gostd::math as gomath;
use tidb_base::{Error, errors};

use crate::vector::{VectorFloat32, init_vector_float32};

/// `z + x*y` as gc compiles it at the fused sites of this file (see the module table).
// glue: the multiply-add of the M-FMA sites (module table)
#[inline(always)]
fn madd32(z: f32, x: f32, y: f32) -> f32 {
    #[cfg(any(
        target_arch = "aarch64",
        all(target_arch = "powerpc64", target_endian = "little"),
        target_arch = "riscv64",
        target_arch = "loongarch64"
    ))]
    {
        x.mul_add(y, z)
    }
    #[cfg(not(any(
        target_arch = "aarch64",
        all(target_arch = "powerpc64", target_endian = "little"),
        target_arch = "riscv64",
        target_arch = "loongarch64"
    )))]
    {
        z + x * y
    }
}

/// [`madd32`] for the float64 sum of `L2Norm`.
// glue: the multiply-add of the M-FMA site of L2Norm (module table)
#[inline(always)]
fn madd64(z: f64, x: f64, y: f64) -> f64 {
    #[cfg(any(
        target_arch = "aarch64",
        all(target_arch = "powerpc64", target_endian = "little"),
        target_arch = "riscv64",
        target_arch = "loongarch64"
    ))]
    {
        x.mul_add(y, z)
    }
    #[cfg(not(any(
        target_arch = "aarch64",
        all(target_arch = "powerpc64", target_endian = "little"),
        target_arch = "riscv64",
        target_arch = "loongarch64"
    )))]
    {
        z + x * y
    }
}

impl VectorFloat32<'_> {
    // go: VectorFloat32.checkIdenticalDims L1 [M-ERR]
    fn check_identical_dims(&self, b: &VectorFloat32<'_>) -> Result<(), Error> {
        if self.len() != b.len() {
            return Err(errors::errorf(
                "vectors have different dimensions: %d and %d",
                &[Arg::from(self.len()), Arg::from(b.len())],
            ));
        }
        Ok(())
    }

    /// L2SquaredDistance returns the squared L2 distance between two vectors.
    /// This saves a sqrt calculation.
    // go: VectorFloat32.L2SquaredDistance L1 [M-ERR, M-FMA]
    pub fn l2_squared_distance(&self, b: &VectorFloat32<'_>) -> Result<f64, Error> {
        self.check_identical_dims(b)?;

        let mut distance: f32 = 0.0;
        let va = self.elements();
        let vb = b.elements();

        for (x, y) in va.iter().zip(vb.iter()) {
            // Hope this can be vectorized.
            let diff = x - y;
            distance = madd32(distance, diff, diff); // [M-FMA] vector_functions.go:44
        }

        Ok(distance as f64)
    }

    /// L2Distance returns the L2 distance between two vectors.
    // go: VectorFloat32.L2Distance L1 [M-ERR]
    pub fn l2_distance(&self, b: &VectorFloat32<'_>) -> Result<f64, Error> {
        let d = self.l2_squared_distance(b)?;
        Ok(d.sqrt())
    }

    /// InnerProduct returns the inner product of two vectors.
    // go: VectorFloat32.InnerProduct L1 [M-ERR, M-FMA]
    pub fn inner_product(&self, b: &VectorFloat32<'_>) -> Result<f64, Error> {
        self.check_identical_dims(b)?;

        let mut distance: f32 = 0.0;
        let va = self.elements();
        let vb = b.elements();

        for (x, y) in va.iter().zip(vb.iter()) {
            // Hope this can be vectorized.
            distance = madd32(distance, x, y); // [M-FMA] vector_functions.go:71
        }

        Ok(distance as f64)
    }

    /// NegativeInnerProduct returns the negative inner product of two vectors.
    // go: VectorFloat32.NegativeInnerProduct L1 [M-ERR]
    pub fn negative_inner_product(&self, b: &VectorFloat32<'_>) -> Result<f64, Error> {
        let d = self.inner_product(b)?;
        // `d * -1`: gc rewrites a multiplication by the constant -1 into a negation (the
        // sign bit flips, also for NaN; a multiplication would keep a NaN's sign).
        Ok(-d)
    }

    /// CosineDistance returns the cosine distance between two vectors.
    // go: VectorFloat32.CosineDistance L1 [M-ERR, M-FMA]
    pub fn cosine_distance(&self, b: &VectorFloat32<'_>) -> Result<f64, Error> {
        self.check_identical_dims(b)?;

        let mut distance: f32 = 0.0;
        let mut norma: f32 = 0.0;
        let mut normb: f32 = 0.0;
        let va = self.elements();
        let vb = b.elements();

        for (x, y) in va.iter().zip(vb.iter()) {
            // Hope this can be vectorized.
            distance = madd32(distance, x, y); // [M-FMA] vector_functions.go:100
            norma = madd32(norma, x, x); // [M-FMA] vector_functions.go:101
            normb = madd32(normb, y, y); // [M-FMA] vector_functions.go:102
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
    pub fn l1_distance(&self, b: &VectorFloat32<'_>) -> Result<f64, Error> {
        self.check_identical_dims(b)?;

        let mut distance: f32 = 0.0;
        let va = self.elements();
        let vb = b.elements();

        for (x, y) in va.iter().zip(vb.iter()) {
            // Hope this can be vectorized.
            let mut diff = x - y;
            if diff < 0.0 {
                diff = -diff;
            }
            distance += diff;
        }

        Ok(distance as f64)
    }

    /// L2Norm returns the L2 norm of the vector.
    // go: VectorFloat32.L2Norm L1 [M-FMA]
    pub fn l2_norm(&self) -> f64 {
        // Note: We align the impl with pgvector: Only l2_norm use double
        // precision during calculation.
        let mut norm: f64 = 0.0;

        for x in self.elements().iter() {
            // Hope this can be vectorized.
            norm = madd64(norm, x as f64, x as f64); // [M-FMA] vector_functions.go:152
        }
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
