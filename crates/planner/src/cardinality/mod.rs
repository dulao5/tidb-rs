//! Go package `pkg/planner/cardinality` (block D10.6): cardinality estimation.
//!
//! D10.6 part 1 (VS1-29, specs/D10.6-cardinality): the pseudo estimation and what the VS1
//! planner reads — `Selectivity`, the pseudo rates, the column / int / index range row
//! counts on pseudo statistics, NDV and average row sizes. The estimation from real
//! statistics (TopN, CM sketch, histograms), cross-column estimation and the traces are
//! D10.6 part 2; ranges are built by `crate::ranger` (D10.4, VS1-25).

pub mod cross_estimation;
pub mod ndv;
pub mod pseudo;
pub mod row_count_column;
pub mod row_count_index;
pub mod row_size;
pub mod selectivity;
pub(crate) mod trace;

pub use ndv::*;
pub use pseudo::pseudo_avg_count_per_value;
pub use row_count_column::*;
pub use row_count_index::get_row_count_by_index_ranges;
pub use row_size::*;
pub use selectivity::{
    ColType, IndexType, PkType, StatsNode, calc_total_selectivity_for_mv_idx_path,
    get_usable_sets_by_greedy, is_last_bucket_end_value_underrepresented, selectivity,
};

/// `z + x * y` the way Go gc compiles row_size.go:89: fused on arm64, ppc64le, s390x,
/// riscv64, loong64, not on amd64 (GOAMD64=v1) (mapping rule `M-FMA`, tracker
/// `data/go_fma_sites.tsv`). Rust never contracts by itself.
// glue: M-FMA helper
#[inline(always)]
pub(crate) fn madd(z: f64, x: f64, y: f64) -> f64 {
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
