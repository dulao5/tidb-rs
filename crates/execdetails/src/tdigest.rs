//! go-dep-file: github.com/influxdata/tdigest@v0.0.1 tdigest.go, centroid.go
//!
//! The t-digest that `execdetails.Percentile` switches to after
//! `MaxDetailsNumsForOneQuery` values (L1 port of the dependency, only what TiDB calls:
//! `New`, `Add`, `AddCentroidList`, `Centroids`, `Quantile`).
//!
//! Floating point follows Go bit for bit:
//! - `math.Sin`, `math.Asin`, `math.Min`, `math.Max` are Go's ([`tidb_base::gostd::math`]),
//!   not Rust's libm;
//! - `sort.Sort` is Go's pdqsort ([`tidb_base::gostd::sort::sort`]): centroids with equal
//!   means keep Go's order, which decides how they merge;
//! - multiply-adds follow mapping rule M-FMA. The positions gc fuses on arm64 were read from
//!   `go tool objdump` of a go1.25.12 linux/arm64 build (the dependency is outside the
//!   tracker's `data/go_fma_sites.tsv`); amd64 (GOAMD64=v1) fuses nothing. gc also rewrites
//!   `x/2.0` to `x*0.5` and `2.0*x` to `x+x` (both exact) and recomputes `index` (`q *
//!   processedWeight`) inside each fused expression, so the fused forms below spell out the
//!   operands the instructions use.

use tidb_base::gostd::math;
use tidb_base::gostd::sort;

/// `Centroid`: average position of all points in a shape.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Centroid {
    pub mean: f64,
    pub weight: f64,
}

impl Centroid {
    /// `Add` averages the two centroids together and updates this centroid. It returns false
    /// (Go: `ErrWeightLessThanZero`) and changes nothing when `r.weight < 0`.
    // go-dep: github.com/influxdata/tdigest@v0.0.1.Centroid.Add L1 (the error is a bool; callers ignore it)
    pub fn add(&mut self, r: Centroid) -> bool {
        if r.weight < 0.0 {
            return false;
        }
        if self.weight != 0.0 {
            self.weight += r.weight;
            self.mean += r.weight * (r.mean - self.mean) / self.weight;
        } else {
            self.weight = r.weight;
            self.mean = r.mean;
        }
        true
    }
}

/// `TDigest`: a data structure for accurate on-line accumulation of rank-based statistics
/// such as quantiles and trimmed means.
#[derive(Clone, Debug)]
pub struct TDigest {
    pub compression: f64,
    max_processed: usize,
    max_unprocessed: usize,
    processed: Vec<Centroid>,
    unprocessed: Vec<Centroid>,
    cumulative: Vec<f64>,
    processed_weight: f64,
    unprocessed_weight: f64,
    min: f64,
    max: f64,
}

/// `x*y + z`, fused on arm64 (`FMADDD`), mapping rule M-FMA.
#[inline]
// glue: M-FMA helper (fused on aarch64, separate multiply and add elsewhere)
fn fma(x: f64, y: f64, z: f64) -> f64 {
    #[cfg(target_arch = "aarch64")]
    {
        x.mul_add(y, z)
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        x * y + z
    }
}

impl Default for TDigest {
    // glue: TDigest::default is New()
    fn default() -> Self {
        Self::new()
    }
}

impl TDigest {
    /// `New` initializes a new distribution with a default compression.
    // go-dep: github.com/influxdata/tdigest@v0.0.1.New L1
    pub fn new() -> TDigest {
        Self::new_with_compression(1000.0)
    }

    /// `NewWithCompression` initializes a new distribution with custom compression.
    // go-dep: github.com/influxdata/tdigest@v0.0.1.NewWithCompression L1
    pub fn new_with_compression(c: f64) -> TDigest {
        let max_processed = processed_size(0, c);
        let max_unprocessed = unprocessed_size(0, c);
        let mut t = TDigest {
            compression: c,
            max_processed,
            max_unprocessed,
            processed: Vec::with_capacity(max_processed),
            unprocessed: Vec::with_capacity(max_unprocessed + 1),
            cumulative: Vec::new(),
            processed_weight: 0.0,
            unprocessed_weight: 0.0,
            min: 0.0,
            max: 0.0,
        };
        t.reset();
        t
    }

    /// `Reset` resets the distribution to its initial state.
    // go-dep: github.com/influxdata/tdigest@v0.0.1.TDigest.Reset L1
    pub fn reset(&mut self) {
        self.processed.clear();
        self.unprocessed.clear();
        self.cumulative.clear();
        self.processed_weight = 0.0;
        self.unprocessed_weight = 0.0;
        self.min = f64::MAX;
        self.max = -f64::MAX;
    }

    /// `Add` adds a value x with a weight w to the distribution.
    // go-dep: github.com/influxdata/tdigest@v0.0.1.TDigest.Add L1
    pub fn add(&mut self, x: f64, w: f64) {
        if x.is_nan() {
            return;
        }
        self.add_centroid(Centroid { mean: x, weight: w });
    }

    /// `AddCentroidList` can quickly add multiple centroids.
    // go-dep: github.com/influxdata/tdigest@v0.0.1.TDigest.AddCentroidList L1
    pub fn add_centroid_list(&mut self, c: &[Centroid]) {
        let l = c.len();
        let mut i = 0;
        while i < l {
            let diff = l - i;
            let room = self.max_unprocessed as isize - self.unprocessed.len() as isize;
            let mut mid = i + diff;
            if room < diff as isize {
                mid = (i as isize + room) as usize;
            }
            while i < mid {
                self.add_centroid(c[i]);
                i += 1;
            }
            i += 1;
        }
    }

    /// `AddCentroid` adds a single centroid.
    // go-dep: github.com/influxdata/tdigest@v0.0.1.TDigest.AddCentroid L1
    pub fn add_centroid(&mut self, c: Centroid) {
        self.unprocessed.push(c);
        self.unprocessed_weight += c.weight;

        if self.processed.len() > self.max_processed
            || self.unprocessed.len() > self.max_unprocessed
        {
            self.process();
        }
    }

    // go-dep: github.com/influxdata/tdigest@v0.0.1.TDigest.process L1
    fn process(&mut self) {
        if !self.unprocessed.is_empty() || self.processed.len() > self.max_processed {
            // Append all processed centroids to the unprocessed list and sort
            self.unprocessed.extend_from_slice(&self.processed);
            sort::sort(&mut self.unprocessed, |a, b| a.mean < b.mean);

            // Reset processed list with first centroid
            self.processed.clear();
            self.processed.push(self.unprocessed[0]);

            self.processed_weight += self.unprocessed_weight;
            self.unprocessed_weight = 0.0;
            let mut so_far = self.unprocessed[0].weight;
            let mut limit = self.processed_weight * self.integrated_q(1.0);
            for &centroid in &self.unprocessed[1..] {
                let projected = so_far + centroid.weight;
                if projected <= limit {
                    so_far = projected;
                    let last = self.processed.len() - 1;
                    self.processed[last].add(centroid);
                } else {
                    let k1 = self.integrated_location(so_far / self.processed_weight);
                    limit = self.processed_weight * self.integrated_q(k1 + 1.0);
                    so_far += centroid.weight;
                    self.processed.push(centroid);
                }
            }
            self.min = math::min(self.min, self.processed[0].mean);
            self.max = math::max(self.max, self.processed[self.processed.len() - 1].mean);
            self.update_cumulative();
            self.unprocessed.clear();
        }
    }

    /// `Centroids` returns a copy of processed centroids. Useful when aggregating multiple
    /// t-digests.
    // go-dep: github.com/influxdata/tdigest@v0.0.1.TDigest.Centroids L1
    pub fn centroids(&mut self) -> Vec<Centroid> {
        self.process();
        self.processed.clone()
    }

    // go-dep: github.com/influxdata/tdigest@v0.0.1.TDigest.updateCumulative L1 [M-FMA]
    fn update_cumulative(&mut self) {
        let n = self.processed.len() + 1;
        self.cumulative.clear();
        self.cumulative.resize(n, 0.0);

        let mut prev = 0.0;
        for (i, centroid) in self.processed.iter().enumerate() {
            let cur = centroid.weight;
            // [M-FMA] tdigest.go:154 (prev + cur/2.0: gc writes cur*0.5 and fuses)
            self.cumulative[i] = fma(cur, 0.5, prev);
            prev += cur;
        }
        self.cumulative[self.processed.len()] = prev;
    }

    /// `Quantile` returns the (approximate) quantile of the distribution. Accepted values for
    /// q are between 0.0 and 1.0. Returns NaN if Count is zero or bad inputs.
    // go-dep: github.com/influxdata/tdigest@v0.0.1.TDigest.Quantile L1 [M-FMA]
    pub fn quantile(&mut self, q: f64) -> f64 {
        self.process();
        if q < 0.0 || q > 1.0 || self.processed.is_empty() {
            return math::nan();
        }
        if self.processed.len() == 1 {
            return self.processed[0].mean;
        }
        let pw = self.processed_weight;
        let index = q * pw;
        if index <= self.processed[0].weight / 2.0 {
            // [M-FMA] tdigest.go:173 (2.0*index is index+index with one side fused:
            // fma(q, pw, index); then (…/w)*(mean-min) + min is fused)
            #[cfg(target_arch = "aarch64")]
            let twice = q.mul_add(pw, index);
            #[cfg(not(target_arch = "aarch64"))]
            let twice = 2.0 * index;
            return fma(
                twice / self.processed[0].weight,
                self.processed[0].mean - self.min,
                self.min,
            );
        }

        let lower = search(self.cumulative.len(), |i| self.cumulative[i] >= index);

        if lower + 1 != self.cumulative.len() {
            // [M-FMA] tdigest.go:181 (q*pw - cumulative[lower-1], FNMSUBD)
            // [M-FMA] tdigest.go:182 (cumulative[lower] - q*pw, FMSUBD)
            #[cfg(target_arch = "aarch64")]
            let (z1, z2) = (
                q.mul_add(pw, -self.cumulative[lower - 1]),
                (-q).mul_add(pw, self.cumulative[lower]),
            );
            #[cfg(not(target_arch = "aarch64"))]
            let (z1, z2) = (
                index - self.cumulative[lower - 1],
                self.cumulative[lower] - index,
            );
            return weighted_average(
                self.processed[lower - 1].mean,
                z2,
                self.processed[lower].mean,
                z1,
            );
        }

        let w = self.processed[lower - 1].weight;
        // [M-FMA] tdigest.go:186 (index - pw is FNMSUBD q, pw, pw; then - w*0.5 is FMSUBD)
        // [M-FMA] tdigest.go:187 (w*0.5 - z1, FNMSUBD)
        #[cfg(target_arch = "aarch64")]
        let (z1, z2) = {
            let z1 = (-w).mul_add(0.5, q.mul_add(pw, -pw));
            (z1, w.mul_add(0.5, -z1))
        };
        #[cfg(not(target_arch = "aarch64"))]
        let (z1, z2) = {
            let z1 = index - pw - w / 2.0;
            (z1, (w / 2.0) - z1)
        };
        weighted_average(
            self.processed[self.processed.len() - 1].mean,
            z1,
            self.max,
            z2,
        )
    }

    // go-dep: github.com/influxdata/tdigest@v0.0.1.TDigest.integratedQ L1
    fn integrated_q(&self, k: f64) -> f64 {
        use std::f64::consts::PI;
        (math::sin(math::min(k, self.compression) * PI / self.compression - PI / 2.0) + 1.0) / 2.0
    }

    // go-dep: github.com/influxdata/tdigest@v0.0.1.TDigest.integratedLocation L1
    fn integrated_location(&self, q: f64) -> f64 {
        use std::f64::consts::PI;
        self.compression * (math::asin(2.0 * q - 1.0) + PI / 2.0) / PI
    }
}

/// `sort.Search(n, f)`: the smallest index i in [0, n) at which f(i) is true, assuming that
/// on the range [0, n), f(i) == true implies f(i+1) == true; n if there is no such index.
// go-std: sort.Search L1
fn search(n: usize, mut f: impl FnMut(usize) -> bool) -> usize {
    let (mut i, mut j) = (0usize, n);
    while i < j {
        let h = (i + j) >> 1;
        if !f(h) {
            i = h + 1;
        } else {
            j = h;
        }
    }
    i
}

// go-dep: github.com/influxdata/tdigest@v0.0.1.weightedAverage L1
fn weighted_average(x1: f64, w1: f64, x2: f64, w2: f64) -> f64 {
    if x1 <= x2 {
        return weighted_average_sorted(x1, w1, x2, w2);
    }
    weighted_average_sorted(x2, w2, x1, w1)
}

// go-dep: github.com/influxdata/tdigest@v0.0.1.weightedAverageSorted L1 [M-FMA]
fn weighted_average_sorted(x1: f64, w1: f64, x2: f64, w2: f64) -> f64 {
    let x = fma(x2, w2, x1 * w1) / (w1 + w2); // [M-FMA] tdigest.go:260
    math::max(x1, math::min(x, x2))
}

// go-dep: github.com/influxdata/tdigest@v0.0.1.processedSize L1
fn processed_size(size: usize, compression: f64) -> usize {
    if size == 0 {
        return (2.0 * compression.ceil()) as usize;
    }
    size
}

// go-dep: github.com/influxdata/tdigest@v0.0.1.unprocessedSize L1
fn unprocessed_size(size: usize, compression: f64) -> usize {
    if size == 0 {
        return (8.0 * compression.ceil()) as usize;
    }
    size
}
