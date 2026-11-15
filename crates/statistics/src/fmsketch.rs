//! go-file: pkg/statistics/fmsketch.go
//!
//! The Flajolet-Martin sketch is D11.1 part 2 (specs/D11.1-statistics §1.3): it is only built
//! by ANALYZE (delegated to Go TiDB) and merged for global stats. As with `CMSketch`, the type
//! has no values in part 1, so no statistics hold one.

/// FMSketch (Flajolet-Martin Sketch) is used to count the approximate number of distinct elements.
// glue: boundary of the type FMSketch, blocked-by:D11.1 (part 2: FM sketch; no statistics hold one in part 1)
#[derive(Debug)]
pub enum FMSketch {}

impl FMSketch {
    /// Copy makes a copy for current FMSketch.
    // glue: stub of FMSketch.Copy, blocked-by:D11.1 (part 2; no value of FMSketch exists)
    pub fn copy(&self) -> FMSketch {
        match *self {}
    }

    /// MemoryUsage returns the total memory usage of a FMSketch.
    // glue: stub of FMSketch.MemoryUsage, blocked-by:D11.1 (part 2; no value of FMSketch exists)
    pub fn memory_usage(&self) -> i64 {
        match *self {}
    }
}
