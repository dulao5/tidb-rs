//! go-file: pkg/executor/aggfuncs/func_sum.go
//!
//! SUM (part A of D12.4). The `Slide` methods (and thus the only difference between
//! `sum4Float64` and `sum4Float64HighPrecision`) come with the window functions (part W).
//! `baseSumAggFunc` has no methods; its implementations embed `BaseAggFunc` directly.
//!
//! The DISTINCT sets are `util/set`'s sets with Go's float key equality (spec §5.6) in the
//! allocator of [`AggFunc::set_alloc`] (A1, spec §5.3); the memory deltas Go adds up are
//! not returned (spec §9 item 1).

use tidb_base::Error;
use tidb_base::errors as goerrors;
use tidb_base::mysql::MaxDecimalScale;
use tidb_chunk::ColAlloc;
use tidb_chunk::chunk::Chunk;
use tidb_chunk::row::Row;
use tidb_expression::expression::EvalState;
use tidb_kv::set::{
    Float64SetWithMemoryUsage, StringSetWithMemoryUsage, new_float64_set_with_memory_usage,
    new_string_set_with_memory_usage,
};
use tidb_types::{MODE_HALF_UP, MyDecimal, decimal_add};

use super::aggfuncs::{
    AggFunc, AggFuncUpdateContext, BaseAggFunc, PartialResult, partial_result_mut,
};

#[derive(Debug, Default)]
struct PartialResult4SumFloat64 {
    val: f64,
    not_null_row_count: i64,
}

#[derive(Debug, Default)]
struct PartialResult4SumDecimal {
    val: MyDecimal,
    not_null_row_count: i64,
}

#[derive(Debug)]
struct PartialResult4SumDistinctFloat64 {
    val: f64,
    is_null: bool,
    val_set: Float64SetWithMemoryUsage<ColAlloc>,
}

#[derive(Debug)]
struct PartialResult4SumDistinctDecimal {
    val: MyDecimal,
    is_null: bool,
    val_set: StringSetWithMemoryUsage<ColAlloc>,
}

/// baseSum4Float64, embedded in `sum4Float64` and `sum4Float64HighPrecision`.
#[derive(Clone, Debug)]
pub struct BaseSum4Float64 {
    pub base: BaseAggFunc,
}

impl AggFunc for BaseSum4Float64 {
    // go: baseSum4Float64.AllocPartialResult L2 (memDelta dropped, A1)
    fn alloc_partial_result(&self) -> PartialResult {
        Box::new(PartialResult4SumFloat64::default())
    }

    // go: baseSum4Float64.ResetPartialResult L1
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4SumFloat64>(pr);
        p.val = 0.0;
        p.not_null_row_count = 0;
    }

    // go: baseSum4Float64.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4SumFloat64>(pr);
        if p.not_null_row_count == 0 {
            chk.append_null(self.base.col());
            return Ok(());
        }
        chk.append_float64(self.base.col(), p.val);
        Ok(())
    }

    // go: baseSum4Float64.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4SumFloat64>(pr);
        for &row in rows_in_group {
            let Some(input) = self.base.args[0].eval_real(sctx, st, row)? else {
                continue;
            };
            p.val += input;
            p.not_null_row_count += 1;
        }
        Ok(())
    }

    // go: baseSum4Float64.MergePartialResult L2 (memDelta dropped, A1; M-INT)
    fn merge_partial_result(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        src: &mut PartialResult,
        dst: &mut PartialResult,
    ) -> Result<(), Error> {
        let p1 = partial_result_mut::<PartialResult4SumFloat64>(src);
        let (val, count) = (p1.val, p1.not_null_row_count);
        if count == 0 {
            return Ok(());
        }
        let p2 = partial_result_mut::<PartialResult4SumFloat64>(dst);
        p2.val += val;
        p2.not_null_row_count = p2.not_null_row_count.wrapping_add(count);
        Ok(())
    }
}

/// `sum4Float64`: `baseSum4Float64` and a `Slide` (part W).
#[derive(Clone, Debug)]
pub struct Sum4Float64 {
    pub base: BaseSum4Float64,
}

/// `sum4Float64HighPrecision`: `baseSum4Float64` without a `Slide` (the window executor
/// recomputes each frame).
#[derive(Clone, Debug)]
pub struct Sum4Float64HighPrecision {
    pub base: BaseSum4Float64,
}

macro_rules! forward_base_sum4_float64 {
    ($t:ident) => {
        impl AggFunc for $t {
            // glue: baseSum4Float64.AllocPartialResult through the embedded struct (M-SELF)
            fn alloc_partial_result(&self) -> PartialResult {
                self.base.alloc_partial_result()
            }

            // glue: baseSum4Float64.ResetPartialResult through the embedded struct (M-SELF)
            fn reset_partial_result(&self, pr: &mut PartialResult) {
                self.base.reset_partial_result(pr)
            }

            // glue: baseSum4Float64.AppendFinalResult2Chunk through the embedded struct
            // (M-SELF)
            fn append_final_result2_chunk(
                &self,
                sctx: &dyn AggFuncUpdateContext,
                pr: &mut PartialResult,
                chk: &mut Chunk,
            ) -> Result<(), Error> {
                self.base.append_final_result2_chunk(sctx, pr, chk)
            }

            // glue: baseSum4Float64.UpdatePartialResult through the embedded struct (M-SELF)
            fn update_partial_result(
                &self,
                sctx: &dyn AggFuncUpdateContext,
                st: &EvalState,
                rows_in_group: &[Row<'_>],
                pr: &mut PartialResult,
            ) -> Result<(), Error> {
                self.base.update_partial_result(sctx, st, rows_in_group, pr)
            }

            // glue: baseSum4Float64.MergePartialResult through the embedded struct (M-SELF)
            fn merge_partial_result(
                &self,
                sctx: &dyn AggFuncUpdateContext,
                src: &mut PartialResult,
                dst: &mut PartialResult,
            ) -> Result<(), Error> {
                self.base.merge_partial_result(sctx, src, dst)
            }
        }
    };
}

forward_base_sum4_float64!(Sum4Float64);
forward_base_sum4_float64!(Sum4Float64HighPrecision);

/// The final DECIMAL of SUM, AVG, FIRST_ROW and MAX / MIN: Go's `retTp` nil check, the
/// scale of `retTp` (-1: `MaxDecimalScale`) and the in-place `Round` (M-ALIAS).
// glue: the AppendFinalResult2Chunk tail Go repeats in func_sum, func_avg, func_first_row
// and func_max_min
pub(crate) fn round_to_ret_tp(
    base: &BaseAggFunc,
    func: &str,
    val: &mut MyDecimal,
) -> Result<(), Error> {
    let Some(ret_tp) = &base.ret_tp else {
        return Err(goerrors::new(format!(
            "e.retTp of {func} should not be nil"
        )));
    };
    let mut frac = ret_tp.get_decimal();
    if frac == -1 {
        frac = MaxDecimalScale;
    }
    val.round_in_place(frac, MODE_HALF_UP)
}

/// sum4Decimal.
#[derive(Clone, Debug)]
pub struct Sum4Decimal {
    pub base: BaseAggFunc,
}

impl AggFunc for Sum4Decimal {
    // go: sum4Decimal.AllocPartialResult L2 (memDelta dropped, A1)
    fn alloc_partial_result(&self) -> PartialResult {
        Box::new(PartialResult4SumDecimal::default())
    }

    /// Only the count is cleared: the next `Update` overwrites the value (Go).
    // go: sum4Decimal.ResetPartialResult L1
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4SumDecimal>(pr);
        p.not_null_row_count = 0;
    }

    // go: sum4Decimal.AppendFinalResult2Chunk L1 [M-ALIAS]
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4SumDecimal>(pr);
        if p.not_null_row_count == 0 {
            chk.append_null(self.base.col());
            return Ok(());
        }
        round_to_ret_tp(&self.base, "sum", &mut p.val)?;
        chk.append_my_decimal(self.base.col(), &p.val);
        Ok(())
    }

    // go: sum4Decimal.UpdatePartialResult L2 (memDelta dropped, A1 [M-ERRVAL])
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4SumDecimal>(pr);
        for &row in rows_in_group {
            let Some(input) = self.base.args[0].eval_decimal(sctx, st, row)? else {
                continue;
            };
            if p.not_null_row_count == 0 {
                p.val = input;
                p.not_null_row_count = 1;
                continue;
            }

            let mut new_sum = MyDecimal::default();
            decimal_add(&p.val, &input, &mut new_sum)?;
            p.val = new_sum;
            p.not_null_row_count += 1;
        }
        Ok(())
    }

    /// `dst`'s value is added even when its count is 0 (Go; a reset `dst` keeps its value).
    // go: sum4Decimal.MergePartialResult L2 (memDelta dropped, A1; M-INT [M-ERRVAL])
    fn merge_partial_result(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        src: &mut PartialResult,
        dst: &mut PartialResult,
    ) -> Result<(), Error> {
        let p1 = partial_result_mut::<PartialResult4SumDecimal>(src);
        let (val, count) = (p1.val, p1.not_null_row_count);
        if count == 0 {
            return Ok(());
        }
        let p2 = partial_result_mut::<PartialResult4SumDecimal>(dst);
        let mut new_sum = MyDecimal::default();
        decimal_add(&val, &p2.val, &mut new_sum)?;
        p2.val = new_sum;
        p2.not_null_row_count = p2.not_null_row_count.wrapping_add(count);
        Ok(())
    }
}

/// sum4DistinctFloat64.
#[derive(Clone, Debug)]
pub struct Sum4DistinctFloat64 {
    pub base: BaseAggFunc,
    /// The allocator of the DISTINCT sets (A1, spec §5.3).
    alloc: ColAlloc,
}

impl Sum4DistinctFloat64 {
    /// The function with the sets in the global allocator until [`AggFunc::set_alloc`].
    // glue: Go struct literal sum4DistinctFloat64{base}
    pub fn new(base: BaseAggFunc) -> Self {
        Sum4DistinctFloat64 {
            base,
            alloc: ColAlloc::Global,
        }
    }
}

impl AggFunc for Sum4DistinctFloat64 {
    // go: sum4DistinctFloat64.AllocPartialResult L2 (memDelta dropped, A1; the set's allocator, spec §5.3)
    fn alloc_partial_result(&self) -> PartialResult {
        let (val_set, _) = new_float64_set_with_memory_usage(self.alloc.clone());
        Box::new(PartialResult4SumDistinctFloat64 {
            val: 0.0,
            is_null: true,
            val_set,
        })
    }

    // go: sum4DistinctFloat64.ResetPartialResult L2 (the set's allocator, spec §5.3)
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4SumDistinctFloat64>(pr);
        p.is_null = true;
        p.val_set = new_float64_set_with_memory_usage(self.alloc.clone()).0;
    }

    // go: sum4DistinctFloat64.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4SumDistinctFloat64>(pr);
        for &row in rows_in_group {
            let input = self.base.args[0].eval_real(sctx, st, row)?;
            let input = match input {
                Some(v) if !p.val_set.exist(v) => v,
                _ => continue,
            };
            p.val_set.insert(input);
            if p.is_null {
                p.val = input;
                p.is_null = false;
                continue;
            }
            p.val += input;
        }
        Ok(())
    }

    // go: sum4DistinctFloat64.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4SumDistinctFloat64>(pr);
        if p.is_null {
            chk.append_null(self.base.col());
            return Ok(());
        }
        chk.append_float64(self.base.col(), p.val);
        Ok(())
    }

    // glue: A1 3.3 TrackedAlloc of the DISTINCT set (spec §5.3)
    fn set_alloc(&mut self, alloc: ColAlloc) {
        self.alloc = alloc;
    }
}

/// sum4DistinctDecimal.
#[derive(Clone, Debug)]
pub struct Sum4DistinctDecimal {
    pub base: BaseAggFunc,
    /// The allocator of the DISTINCT sets (A1, spec §5.3).
    alloc: ColAlloc,
}

impl Sum4DistinctDecimal {
    /// The function with the sets in the global allocator until [`AggFunc::set_alloc`].
    // glue: Go struct literal sum4DistinctDecimal{base}
    pub fn new(base: BaseAggFunc) -> Self {
        Sum4DistinctDecimal {
            base,
            alloc: ColAlloc::Global,
        }
    }
}

impl AggFunc for Sum4DistinctDecimal {
    // go: sum4DistinctDecimal.AllocPartialResult L2 (memDelta dropped, A1; the set's allocator, spec §5.3)
    fn alloc_partial_result(&self) -> PartialResult {
        let (val_set, _) = new_string_set_with_memory_usage(self.alloc.clone());
        Box::new(PartialResult4SumDistinctDecimal {
            val: MyDecimal::default(),
            is_null: true,
            val_set,
        })
    }

    // go: sum4DistinctDecimal.ResetPartialResult L2 (the set's allocator, spec §5.3)
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4SumDistinctDecimal>(pr);
        p.is_null = true;
        p.val_set = new_string_set_with_memory_usage(self.alloc.clone()).0;
    }

    // go: sum4DistinctDecimal.UpdatePartialResult L2 (memDelta dropped, A1 [M-ERRVAL])
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4SumDistinctDecimal>(pr);
        for &row in rows_in_group {
            let Some(input) = self.base.args[0].eval_decimal(sctx, st, row)? else {
                continue;
            };
            let (hash, err) = input.to_hash_key();
            err?;
            let dec_str = hash;
            if p.val_set.exist(&dec_str) {
                continue;
            }
            p.val_set.insert(dec_str);
            if p.is_null {
                p.val = input;
                p.is_null = false;
                continue;
            }
            let mut new_sum = MyDecimal::default();
            decimal_add(&p.val, &input, &mut new_sum)?;
            p.val = new_sum;
        }
        Ok(())
    }

    // go: sum4DistinctDecimal.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4SumDistinctDecimal>(pr);
        if p.is_null {
            chk.append_null(self.base.col());
            return Ok(());
        }
        chk.append_my_decimal(self.base.col(), &p.val);
        Ok(())
    }

    // glue: A1 3.3 TrackedAlloc of the DISTINCT set (spec §5.3)
    fn set_alloc(&mut self, alloc: ColAlloc) {
        self.alloc = alloc;
    }
}
