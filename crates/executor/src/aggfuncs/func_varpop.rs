//! go-file: pkg/executor/aggfuncs/func_varpop.go
//!
//! `VAR_POP` (part B1 of D12.4: population variance, also embedded by `VAR_SAMP`, `STDDEV_POP`,
//! and `STDDEV_SAMP`).
//!
//! Floating-point multiply-adds follow mapping rule M-FMA (`data/go_fma_sites.tsv` in tracker,
//! go1.25.12):
//!
//! | Go target | `:69` (`FNMSUBD`) | `:97` (`FNMSUBD`) | `:98` (`FMADDD`) | Rust target |
//! |---|---|---|---|---|
//! | amd64 GOAMD64=v1 (default, baseline) | no | no | no | `x86_64`: separate mul and add/sub |
//! | amd64 GOAMD64=v3 (informational) | no | no | `VFMADD231SD` | (baseline is v1) |
//! | arm64 | `FNMSUBD` | `FNMSUBD` | `FMADDD` | `aarch64`: `f64::mul_add` |
//! | ppc64le, riscv64, loong64 | fused | fused | fused | `f64::mul_add`; no vector set, untested |
//! | s390x | fused | fused | fused | big-endian, excluded by wire-format assertion |
//!
//! The DISTINCT set is `util/set`'s float64 set in the allocator of [`AggFunc::set_alloc`]
//! (A1, spec §5.3); the memory deltas Go adds up are not returned (spec §9 item 1).

use tidb_base::Error;
use tidb_chunk::ColAlloc;
use tidb_chunk::chunk::Chunk;
use tidb_chunk::row::Row;
use tidb_expression::expression::EvalState;
use tidb_kv::set::{Float64SetWithMemoryUsage, new_float64_set_with_memory_usage};

use super::aggfuncs::{
    AggFunc, AggFuncUpdateContext, BaseAggFunc, PartialResult, partial_result_mut,
};

/// `x*y - z` rounded once on targets where gc fuses it (`data/go_fma_sites.tsv`), twice
/// elsewhere (mapping rule M-FMA).
#[inline]
// glue: M-FMA helper (fused on targets in go_fma_sites.tsv, separate multiply and subtract elsewhere)
fn fnmsub_fma(x: f64, y: f64, z: f64) -> f64 {
    #[cfg(any(
        target_arch = "aarch64",
        all(target_arch = "powerpc64", target_endian = "little"),
        target_arch = "riscv64",
        target_arch = "loongarch64"
    ))]
    {
        x.mul_add(y, -z)
    }
    #[cfg(not(any(
        target_arch = "aarch64",
        all(target_arch = "powerpc64", target_endian = "little"),
        target_arch = "riscv64",
        target_arch = "loongarch64"
    )))]
    {
        x * y - z
    }
}

/// `z + x*y` rounded once on targets where gc fuses it (`data/go_fma_sites.tsv`), twice
/// elsewhere (mapping rule M-FMA).
#[inline]
// glue: M-FMA helper (fused on targets in go_fma_sites.tsv, separate multiply and add elsewhere)
fn fmadd_fma(x: f64, y: f64, z: f64) -> f64 {
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

/// baseVarPopAggFunc.
#[derive(Clone, Debug)]
pub struct BaseVarPopAggFunc {
    pub base: BaseAggFunc,
}

/// varPop4Float64.
#[derive(Clone, Debug)]
pub struct VarPop4Float64 {
    pub base: BaseVarPopAggFunc,
}

#[derive(Debug, Default)]
pub(crate) struct PartialResult4VarPopFloat64 {
    pub(crate) count: i64,
    pub(crate) sum: f64,
    pub(crate) variance: f64,
}

// go: calculateIntermediate L1 [M-FMA, M-INT]
fn calculate_intermediate(count: i64, sum: f64, input: f64, mut variance: f64) -> f64 {
    // [M-FMA] func_varpop.go:69 (arm64 FNMSUBD: float64(count)*input - sum)
    let t = fnmsub_fma(count as f64, input, sum);
    variance += (t * t) / (count.wrapping_mul(count - 1) as f64);
    variance
}

// go: calculateMerge L1 [M-FMA]
fn calculate_merge(
    src_count: i64,
    dst_count: i64,
    src_sum: f64,
    dst_sum: f64,
    src_variance: f64,
    mut dst_variance: f64,
) -> f64 {
    let src_count_float64 = src_count as f64;
    let dst_count_float64 = dst_count as f64;

    // [M-FMA] func_varpop.go:97 (arm64 FNMSUBD: (srcCountFloat64/dstCountFloat64)*dstSum - srcSum)
    let t = fnmsub_fma(src_count_float64 / dst_count_float64, dst_sum, src_sum);
    let coeff = (dst_count_float64 / src_count_float64) / (dst_count_float64 + src_count_float64);
    let coeff_t = coeff * t;
    // [M-FMA] func_varpop.go:98 (arm64 FMADDD: srcVariance + (coeff*t)*t)
    dst_variance += fmadd_fma(coeff_t, t, src_variance);
    dst_variance
}

impl AggFunc for VarPop4Float64 {
    // go: varPop4Float64.AllocPartialResult L2 (memDelta dropped, A1)
    fn alloc_partial_result(&self) -> PartialResult {
        Box::new(PartialResult4VarPopFloat64::default())
    }

    // go: varPop4Float64.ResetPartialResult L1
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4VarPopFloat64>(pr);
        p.count = 0;
        p.sum = 0.0;
        p.variance = 0.0;
    }

    // go: varPop4Float64.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4VarPopFloat64>(pr);
        if p.count == 0 {
            chk.append_null(self.base.base.col());
            return Ok(());
        }
        let variance = p.variance / p.count as f64;
        chk.append_float64(self.base.base.col(), variance);
        Ok(())
    }

    // go: varPop4Float64.UpdatePartialResult L2 (memDelta dropped, A1 [M-FMA])
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4VarPopFloat64>(pr);
        for &row in rows_in_group {
            let Some(input) = self.base.base.args[0].eval_real(sctx, st, row)? else {
                continue;
            };
            p.count += 1;
            p.sum += input;
            if p.count > 1 {
                p.variance = calculate_intermediate(p.count, p.sum, input, p.variance);
            }
        }
        Ok(())
    }

    // go: varPop4Float64.MergePartialResult L2 (memDelta dropped, A1 [M-FMA, M-INT])
    fn merge_partial_result(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        src: &mut PartialResult,
        dst: &mut PartialResult,
    ) -> Result<(), Error> {
        let p1 = partial_result_mut::<PartialResult4VarPopFloat64>(src);
        let (p1_count, p1_sum, p1_variance) = (p1.count, p1.sum, p1.variance);
        if p1_count == 0 {
            return Ok(());
        }
        let p2 = partial_result_mut::<PartialResult4VarPopFloat64>(dst);
        if p2.count == 0 {
            p2.count = p1_count;
            p2.sum = p1_sum;
            p2.variance = p1_variance;
            return Ok(());
        }
        if p2.count != 0 && p1_count != 0 {
            p2.variance =
                calculate_merge(p1_count, p2.count, p1_sum, p2.sum, p1_variance, p2.variance);
            p2.count = p2.count.wrapping_add(p1_count);
            p2.sum += p1_sum;
        }
        Ok(())
    }
}

/// varPop4DistinctFloat64.
#[derive(Clone, Debug)]
pub struct VarPop4DistinctFloat64 {
    pub base: BaseVarPopAggFunc,
    /// The allocator of the DISTINCT sets (A1, spec §5.3).
    alloc: ColAlloc,
}

impl VarPop4DistinctFloat64 {
    /// The function with the sets in the global allocator until [`AggFunc::set_alloc`].
    // glue: Go struct literal varPop4DistinctFloat64{base}
    pub fn new(base: BaseVarPopAggFunc) -> Self {
        VarPop4DistinctFloat64 {
            base,
            alloc: ColAlloc::Global,
        }
    }
}

#[derive(Debug)]
pub(crate) struct PartialResult4VarPopDistinctFloat64 {
    pub(crate) count: i64,
    pub(crate) sum: f64,
    pub(crate) variance: f64,
    val_set: Float64SetWithMemoryUsage<ColAlloc>,
}

impl AggFunc for VarPop4DistinctFloat64 {
    // go: varPop4DistinctFloat64.AllocPartialResult L2 (memDelta dropped, A1; the set's allocator, spec §5.3)
    fn alloc_partial_result(&self) -> PartialResult {
        let (val_set, _) = new_float64_set_with_memory_usage(self.alloc.clone());
        Box::new(PartialResult4VarPopDistinctFloat64 {
            count: 0,
            sum: 0.0,
            variance: 0.0,
            val_set,
        })
    }

    // go: varPop4DistinctFloat64.ResetPartialResult L2 (the set's allocator, spec §5.3)
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4VarPopDistinctFloat64>(pr);
        p.count = 0;
        p.sum = 0.0;
        p.variance = 0.0;
        p.val_set = new_float64_set_with_memory_usage(self.alloc.clone()).0;
    }

    // go: varPop4DistinctFloat64.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4VarPopDistinctFloat64>(pr);
        if p.count == 0 {
            chk.append_null(self.base.base.col());
            return Ok(());
        }
        let variance = p.variance / p.count as f64;
        chk.append_float64(self.base.base.col(), variance);
        Ok(())
    }

    // go: varPop4DistinctFloat64.UpdatePartialResult L2 (memDelta dropped, A1 [M-FMA])
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4VarPopDistinctFloat64>(pr);
        for &row in rows_in_group {
            let input = self.base.base.args[0].eval_real(sctx, st, row)?;
            let input = match input {
                Some(v) if !p.val_set.exist(v) => v,
                _ => continue,
            };
            p.val_set.insert(input);
            p.count += 1;
            p.sum += input;

            if p.count > 1 {
                p.variance = calculate_intermediate(p.count, p.sum, input, p.variance);
            }
        }
        Ok(())
    }

    // glue: A1 3.3 TrackedAlloc of the DISTINCT set (spec §5.3)
    fn set_alloc(&mut self, alloc: ColAlloc) {
        self.alloc = alloc;
    }
}
