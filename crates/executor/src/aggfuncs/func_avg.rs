//! go-file: pkg/executor/aggfuncs/func_avg.go
//!
//! AVG (part A of D12.4). The `Slide` methods (and thus the only difference between
//! `avgOriginal4Float64` and `avgOriginal4Float64HighPrecision`) come with the window
//! functions (part W). The partial phase's output is two columns, the count (`args[0]`) and
//! the sum (`args[1]`), read back by `avgPartial4Xxx`.
//!
//! The DISTINCT sets are `util/set`'s sets in the allocator of [`AggFunc::set_alloc`] (A1,
//! spec §5.3); the memory deltas Go adds up are not returned (spec §9 item 1).

use tidb_base::Error;
use tidb_chunk::ColAlloc;
use tidb_chunk::chunk::Chunk;
use tidb_chunk::row::Row;
use tidb_expression::expression::EvalState;
use tidb_kv::set::{
    Float64SetWithMemoryUsage, StringSetWithMemoryUsage, new_float64_set_with_memory_usage,
    new_string_set_with_memory_usage,
};
use tidb_types::{MyDecimal, decimal_add, decimal_div, decimal_sub, new_dec_from_int};

use super::aggfuncs::{
    AggFunc, AggFuncUpdateContext, BaseAggFunc, PartialResult, SlidingWindowAggFunc,
    partial_result_mut,
};
use super::func_sum::round_to_ret_tp;

// All the following avg function implementations return the decimal result,
// which store the partial results in "partialResult4AvgDecimal".
//
// "baseAvgDecimal" is wrapped by:
// - "avgOriginal4Decimal"
// - "avgPartial4Decimal"

/// baseAvgDecimal.
#[derive(Clone, Debug)]
pub struct BaseAvgDecimal {
    pub base: BaseAggFunc,
}

#[derive(Debug, Default)]
struct PartialResult4AvgDecimal {
    sum: MyDecimal,
    count: i64,
}

/// `sum / count` rounded to `retTp` (the tail of both DECIMAL `AppendFinalResult2Chunk`s).
// glue: the AppendFinalResult2Chunk body Go repeats in baseAvgDecimal and
// avgOriginal4DistinctDecimal
fn append_avg_decimal(
    base: &BaseAggFunc,
    ctx: &dyn AggFuncUpdateContext,
    p: &PartialResult4AvgDecimal,
    chk: &mut Chunk,
) -> Result<(), Error> {
    if p.count == 0 {
        chk.append_null(base.col());
        return Ok(());
    }
    let decimal_count = new_dec_from_int(p.count);
    let mut final_result = MyDecimal::default();
    decimal_div(
        &p.sum,
        &decimal_count,
        &mut final_result,
        ctx.get_div_precision_increment() as isize,
    )?;
    round_to_ret_tp(base, "avg", &mut final_result)?;
    chk.append_my_decimal(base.col(), &final_result);
    Ok(())
}

impl BaseAvgDecimal {
    // go: baseAvgDecimal.AllocPartialResult L2 (memDelta dropped, A1)
    pub fn alloc_partial_result(&self) -> PartialResult {
        Box::new(PartialResult4AvgDecimal::default())
    }

    // go: baseAvgDecimal.ResetPartialResult L1
    pub fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4AvgDecimal>(pr);
        p.sum = new_dec_from_int(0);
        p.count = 0;
    }

    // go: baseAvgDecimal.AppendFinalResult2Chunk L1 [M-ERRVAL, M-ALIAS]
    pub fn append_final_result2_chunk(
        &self,
        ctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4AvgDecimal>(pr);
        append_avg_decimal(&self.base, ctx, p, chk)
    }
}

macro_rules! forward_base_avg_decimal {
    () => {
        // glue: baseAvgDecimal.AllocPartialResult through the embedded struct (M-SELF)
        fn alloc_partial_result(&self) -> PartialResult {
            self.base.alloc_partial_result()
        }

        // glue: baseAvgDecimal.ResetPartialResult through the embedded struct (M-SELF)
        fn reset_partial_result(&self, pr: &mut PartialResult) {
            self.base.reset_partial_result(pr)
        }

        // glue: baseAvgDecimal.AppendFinalResult2Chunk through the embedded struct (M-SELF)
        fn append_final_result2_chunk(
            &self,
            sctx: &dyn AggFuncUpdateContext,
            pr: &mut PartialResult,
            chk: &mut Chunk,
        ) -> Result<(), Error> {
            self.base.append_final_result2_chunk(sctx, pr, chk)
        }
    };
}

/// avgOriginal4Decimal.
#[derive(Clone, Debug)]
pub struct AvgOriginal4Decimal {
    pub base: BaseAvgDecimal,
}

impl AggFunc for AvgOriginal4Decimal {
    forward_base_avg_decimal!();

    // go: avgOriginal4Decimal.UpdatePartialResult L2 (memDelta dropped, A1 [M-ERRVAL])
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4AvgDecimal>(pr);
        for &row in rows_in_group {
            let Some(input) = self.base.base.args[0].eval_decimal(sctx, st, row)? else {
                continue;
            };

            let mut new_sum = MyDecimal::default();
            decimal_add(&p.sum, &input, &mut new_sum)?;
            p.sum = new_sum;
            p.count += 1;
        }
        Ok(())
    }

    // glue: Go type assertion agg.(SlidingWindowAggFunc) (spec §5.1)
    fn as_sliding_window(&self) -> Option<&dyn SlidingWindowAggFunc> {
        Some(self)
    }
}

impl SlidingWindowAggFunc for AvgOriginal4Decimal {
    // go: avgOriginal4Decimal.Slide L1 [M-ERRVAL]
    fn slide<'a>(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        get_row: &dyn Fn(u64) -> Row<'a>,
        last_start: u64,
        last_end: u64,
        shift_start: u64,
        shift_end: u64,
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4AvgDecimal>(pr);
        for i in 0..shift_end {
            let Some(input) =
                self.base.base.args[0].eval_decimal(sctx, st, get_row(last_end + i))?
            else {
                continue;
            };
            let mut new_sum = MyDecimal::default();
            decimal_add(&p.sum, &input, &mut new_sum)?;
            p.sum = new_sum;
            p.count += 1;
        }
        for i in 0..shift_start {
            let Some(input) =
                self.base.base.args[0].eval_decimal(sctx, st, get_row(last_start + i))?
            else {
                continue;
            };
            let mut new_sum = MyDecimal::default();
            decimal_sub(&p.sum, &input, &mut new_sum)?;
            p.sum = new_sum;
            p.count -= 1;
        }
        Ok(())
    }
}

/// avgPartial4Decimal.
#[derive(Clone, Debug)]
pub struct AvgPartial4Decimal {
    pub base: BaseAvgDecimal,
}

impl AggFunc for AvgPartial4Decimal {
    forward_base_avg_decimal!();

    // go: avgPartial4Decimal.UpdatePartialResult L2 (memDelta dropped, A1; M-INT)
    // [M-ERRVAL]
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4AvgDecimal>(pr);
        for &row in rows_in_group {
            let Some(input_sum) = self.base.base.args[1].eval_decimal(sctx, st, row)? else {
                continue;
            };

            let Some(input_count) = self.base.base.args[0].eval_int(sctx, st, row)? else {
                continue;
            };

            let mut new_sum = MyDecimal::default();
            decimal_add(&p.sum, &input_sum, &mut new_sum)?;
            p.sum = new_sum;
            p.count = p.count.wrapping_add(input_count);
        }
        Ok(())
    }

    // go: avgPartial4Decimal.MergePartialResult L2 (memDelta dropped, A1; M-INT)
    // [M-ERRVAL]
    fn merge_partial_result(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        src: &mut PartialResult,
        dst: &mut PartialResult,
    ) -> Result<(), Error> {
        let p1 = partial_result_mut::<PartialResult4AvgDecimal>(src);
        let (sum, count) = (p1.sum, p1.count);
        if count == 0 {
            return Ok(());
        }
        let p2 = partial_result_mut::<PartialResult4AvgDecimal>(dst);
        let mut new_sum = MyDecimal::default();
        decimal_add(&sum, &p2.sum, &mut new_sum)?;
        p2.sum = new_sum;
        p2.count = p2.count.wrapping_add(count);
        Ok(())
    }
}

#[derive(Debug)]
struct PartialResult4AvgDistinctDecimal {
    base: PartialResult4AvgDecimal,
    val_set: StringSetWithMemoryUsage<ColAlloc>,
}

/// avgOriginal4DistinctDecimal.
#[derive(Clone, Debug)]
pub struct AvgOriginal4DistinctDecimal {
    pub base: BaseAggFunc,
    /// The allocator of the DISTINCT sets (A1, spec §5.3).
    alloc: ColAlloc,
}

impl AvgOriginal4DistinctDecimal {
    /// The function with the sets in the global allocator until [`AggFunc::set_alloc`].
    // glue: Go struct literal avgOriginal4DistinctDecimal{base}
    pub fn new(base: BaseAggFunc) -> Self {
        AvgOriginal4DistinctDecimal {
            base,
            alloc: ColAlloc::Global,
        }
    }
}

impl AggFunc for AvgOriginal4DistinctDecimal {
    // go: avgOriginal4DistinctDecimal.AllocPartialResult L2 (memDelta dropped, A1; the set's allocator, spec §5.3)
    fn alloc_partial_result(&self) -> PartialResult {
        let (val_set, _) = new_string_set_with_memory_usage(self.alloc.clone());
        Box::new(PartialResult4AvgDistinctDecimal {
            base: PartialResult4AvgDecimal::default(),
            val_set,
        })
    }

    // go: avgOriginal4DistinctDecimal.ResetPartialResult L2 (the set's allocator, spec §5.3)
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4AvgDistinctDecimal>(pr);
        p.base.sum = new_dec_from_int(0);
        p.base.count = 0;
        p.val_set = new_string_set_with_memory_usage(self.alloc.clone()).0;
    }

    // go: avgOriginal4DistinctDecimal.UpdatePartialResult L2 (memDelta dropped, A1)
    // [M-ERRVAL]
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4AvgDistinctDecimal>(pr);
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
            p.val_set.insert(&dec_str);
            let mut new_sum = MyDecimal::default();
            decimal_add(&p.base.sum, &input, &mut new_sum)?;
            p.base.sum = new_sum;
            p.base.count += 1;
        }
        Ok(())
    }

    // go: avgOriginal4DistinctDecimal.AppendFinalResult2Chunk L1 [M-ERRVAL, M-ALIAS]
    fn append_final_result2_chunk(
        &self,
        ctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4AvgDistinctDecimal>(pr);
        append_avg_decimal(&self.base, ctx, &p.base, chk)
    }

    // glue: A1 3.3 TrackedAlloc of the DISTINCT set (spec §5.3)
    fn set_alloc(&mut self, alloc: ColAlloc) {
        self.alloc = alloc;
    }
}

// All the following avg function implementations return the float64 result,
// which store the partial results in "partialResult4AvgFloat64".
//
// "baseAvgFloat64" is wrapped by:
// - "avgOriginal4Float64"
// - "avgPartial4Float64"

/// baseAvgFloat64.
#[derive(Clone, Debug)]
pub struct BaseAvgFloat64 {
    pub base: BaseAggFunc,
}

#[derive(Debug, Default)]
struct PartialResult4AvgFloat64 {
    sum: f64,
    count: i64,
}

impl BaseAvgFloat64 {
    // go: baseAvgFloat64.AllocPartialResult L2 (memDelta dropped, A1)
    pub fn alloc_partial_result(&self) -> PartialResult {
        Box::new(PartialResult4AvgFloat64::default())
    }

    // go: baseAvgFloat64.ResetPartialResult L1
    pub fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4AvgFloat64>(pr);
        p.sum = 0.0;
        p.count = 0;
    }

    // go: baseAvgFloat64.AppendFinalResult2Chunk L1
    pub fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4AvgFloat64>(pr);
        if p.count == 0 {
            chk.append_null(self.base.col());
        } else {
            chk.append_float64(self.base.col(), p.sum / p.count as f64);
        }
        Ok(())
    }
}

macro_rules! forward_base_avg_float64 {
    ($($base:ident).+) => {
        // glue: baseAvgFloat64.AllocPartialResult through the embedded struct (M-SELF)
        fn alloc_partial_result(&self) -> PartialResult {
            self.$($base).+.alloc_partial_result()
        }

        // glue: baseAvgFloat64.ResetPartialResult through the embedded struct (M-SELF)
        fn reset_partial_result(&self, pr: &mut PartialResult) {
            self.$($base).+.reset_partial_result(pr)
        }

        // glue: baseAvgFloat64.AppendFinalResult2Chunk through the embedded struct (M-SELF)
        fn append_final_result2_chunk(
            &self,
            sctx: &dyn AggFuncUpdateContext,
            pr: &mut PartialResult,
            chk: &mut Chunk,
        ) -> Result<(), Error> {
            self.$($base).+.append_final_result2_chunk(sctx, pr, chk)
        }
    };
}

/// avgOriginal4Float64HighPrecision.
#[derive(Clone, Debug)]
pub struct AvgOriginal4Float64HighPrecision {
    pub base: BaseAvgFloat64,
}

impl AggFunc for AvgOriginal4Float64HighPrecision {
    forward_base_avg_float64!(base);

    // go: avgOriginal4Float64HighPrecision.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4AvgFloat64>(pr);
        for &row in rows_in_group {
            let Some(input) = self.base.base.args[0].eval_real(sctx, st, row)? else {
                continue;
            };

            p.sum += input;
            p.count += 1;
        }
        Ok(())
    }
}

/// `avgOriginal4Float64`: `avgOriginal4Float64HighPrecision` and a `Slide` (part W).
#[derive(Clone, Debug)]
pub struct AvgOriginal4Float64 {
    pub base: AvgOriginal4Float64HighPrecision,
}

impl AggFunc for AvgOriginal4Float64 {
    forward_base_avg_float64!(base.base);

    // glue: avgOriginal4Float64HighPrecision.UpdatePartialResult through the embedded
    // struct (M-SELF)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        self.base.update_partial_result(sctx, st, rows_in_group, pr)
    }

    // glue: Go type assertion agg.(SlidingWindowAggFunc) (spec §5.1)
    fn as_sliding_window(&self) -> Option<&dyn SlidingWindowAggFunc> {
        Some(self)
    }
}

impl SlidingWindowAggFunc for AvgOriginal4Float64 {
    // go: avgOriginal4Float64.Slide L1
    fn slide<'a>(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        get_row: &dyn Fn(u64) -> Row<'a>,
        last_start: u64,
        last_end: u64,
        shift_start: u64,
        shift_end: u64,
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4AvgFloat64>(pr);
        for i in 0..shift_end {
            let Some(input) =
                self.base.base.base.args[0].eval_real(sctx, st, get_row(last_end + i))?
            else {
                continue;
            };
            p.sum += input;
            p.count += 1;
        }
        for i in 0..shift_start {
            let Some(input) =
                self.base.base.base.args[0].eval_real(sctx, st, get_row(last_start + i))?
            else {
                continue;
            };
            p.sum -= input;
            p.count -= 1;
        }
        Ok(())
    }
}

/// avgPartial4Float64.
#[derive(Clone, Debug)]
pub struct AvgPartial4Float64 {
    pub base: BaseAvgFloat64,
}

impl AggFunc for AvgPartial4Float64 {
    forward_base_avg_float64!(base);

    // go: avgPartial4Float64.UpdatePartialResult L2 (memDelta dropped, A1; M-INT)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4AvgFloat64>(pr);
        for &row in rows_in_group {
            let Some(input_sum) = self.base.base.args[1].eval_real(sctx, st, row)? else {
                continue;
            };

            let Some(input_count) = self.base.base.args[0].eval_int(sctx, st, row)? else {
                continue;
            };
            p.sum += input_sum;
            p.count = p.count.wrapping_add(input_count);
        }
        Ok(())
    }

    // go: avgPartial4Float64.MergePartialResult L2 (memDelta dropped, A1; M-INT)
    fn merge_partial_result(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        src: &mut PartialResult,
        dst: &mut PartialResult,
    ) -> Result<(), Error> {
        let p1 = partial_result_mut::<PartialResult4AvgFloat64>(src);
        let (sum, count) = (p1.sum, p1.count);
        let p2 = partial_result_mut::<PartialResult4AvgFloat64>(dst);
        p2.sum += sum;
        p2.count = p2.count.wrapping_add(count);
        Ok(())
    }
}

#[derive(Debug)]
struct PartialResult4AvgDistinctFloat64 {
    base: PartialResult4AvgFloat64,
    val_set: Float64SetWithMemoryUsage<ColAlloc>,
}

/// avgOriginal4DistinctFloat64.
#[derive(Clone, Debug)]
pub struct AvgOriginal4DistinctFloat64 {
    pub base: BaseAggFunc,
    /// The allocator of the DISTINCT sets (A1, spec §5.3).
    alloc: ColAlloc,
}

impl AvgOriginal4DistinctFloat64 {
    /// The function with the sets in the global allocator until [`AggFunc::set_alloc`].
    // glue: Go struct literal avgOriginal4DistinctFloat64{base}
    pub fn new(base: BaseAggFunc) -> Self {
        AvgOriginal4DistinctFloat64 {
            base,
            alloc: ColAlloc::Global,
        }
    }
}

impl AggFunc for AvgOriginal4DistinctFloat64 {
    // go: avgOriginal4DistinctFloat64.AllocPartialResult L2 (memDelta dropped, A1; the set's allocator, spec §5.3)
    fn alloc_partial_result(&self) -> PartialResult {
        let (val_set, _) = new_float64_set_with_memory_usage(self.alloc.clone());
        Box::new(PartialResult4AvgDistinctFloat64 {
            base: PartialResult4AvgFloat64::default(),
            val_set,
        })
    }

    // go: avgOriginal4DistinctFloat64.ResetPartialResult L2 (the set's allocator, spec §5.3)
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4AvgDistinctFloat64>(pr);
        p.base.sum = 0.0;
        p.base.count = 0;
        p.val_set = new_float64_set_with_memory_usage(self.alloc.clone()).0;
    }

    // go: avgOriginal4DistinctFloat64.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4AvgDistinctFloat64>(pr);
        for &row in rows_in_group {
            let input = self.base.args[0].eval_real(sctx, st, row)?;
            let input = match input {
                Some(v) if !p.val_set.exist(v) => v,
                _ => continue,
            };

            p.base.sum += input;
            p.base.count += 1;
            p.val_set.insert(input);
        }
        Ok(())
    }

    // go: avgOriginal4DistinctFloat64.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4AvgDistinctFloat64>(pr);
        if p.base.count == 0 {
            chk.append_null(self.base.col());
            return Ok(());
        }
        chk.append_float64(self.base.col(), p.base.sum / p.base.count as f64);
        Ok(())
    }

    // glue: A1 3.3 TrackedAlloc of the DISTINCT set (spec §5.3)
    fn set_alloc(&mut self, alloc: ColAlloc) {
        self.alloc = alloc;
    }
}
