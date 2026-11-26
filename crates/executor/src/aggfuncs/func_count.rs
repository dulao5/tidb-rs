//! go-file: pkg/executor/aggfuncs/func_count.go
//!
//! COUNT without DISTINCT (part A of D12.4). `countOriginal4Xxx` count the non-NULL rows
//! of the first argument evaluated as its type; `countPartial` sums the counts of a partial
//! phase. Their `Slide` methods come with the window functions (part W), the
//! `countOriginalWithDistinct*` of `func_count_distinct.go` with part B1.

use tidb_base::Error;
use tidb_chunk::chunk::Chunk;
use tidb_chunk::row::Row;
use tidb_expression::expression::EvalState;

use super::aggfuncs::{
    AggFunc, AggFuncUpdateContext, BaseAggFunc, PartialResult, SlidingWindowAggFunc,
    partial_result_mut,
};

/// baseCount, embedded in the COUNT implementations.
#[derive(Clone, Debug)]
pub struct BaseCount {
    pub base: BaseAggFunc,
}

type PartialResult4Count = i64;

impl BaseCount {
    // go: baseCount.AllocPartialResult L2 (memDelta dropped, A1)
    pub fn alloc_partial_result(&self) -> PartialResult {
        Box::new(PartialResult4Count::default())
    }

    // go: baseCount.ResetPartialResult L1
    pub fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4Count>(pr);
        *p = 0;
    }

    // go: baseCount.AppendFinalResult2Chunk L1
    pub fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4Count>(pr);
        chk.append_int64(self.base.col(), *p);
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct CountOriginal4Int {
    pub base: BaseCount,
}

impl AggFunc for CountOriginal4Int {
    // glue: baseCount.AllocPartialResult through the embedded struct (M-SELF)
    fn alloc_partial_result(&self) -> PartialResult {
        self.base.alloc_partial_result()
    }

    // glue: baseCount.ResetPartialResult through the embedded struct (M-SELF)
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        self.base.reset_partial_result(pr)
    }

    // glue: baseCount.AppendFinalResult2Chunk through the embedded struct (M-SELF)
    fn append_final_result2_chunk(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        self.base.append_final_result2_chunk(sctx, pr, chk)
    }

    // go: countOriginal4Int.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4Count>(pr);

        for &row in rows_in_group {
            let input = self.base.base.args[0].eval_int(sctx, st, row)?;
            if input.is_none() {
                continue;
            }

            *p += 1;
        }

        Ok(())
    }

    // glue: Go type assertion agg.(SlidingWindowAggFunc) (spec §5.1)
    fn as_sliding_window(&self) -> Option<&dyn SlidingWindowAggFunc> {
        Some(self)
    }
}

impl SlidingWindowAggFunc for CountOriginal4Int {
    // go: countOriginal4Int.Slide L1
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
        let p = partial_result_mut::<PartialResult4Count>(pr);
        for i in 0..shift_start {
            let input = self.base.base.args[0].eval_int(sctx, st, get_row(last_start + i))?;
            if input.is_none() {
                continue;
            }
            *p -= 1;
        }
        for i in 0..shift_end {
            let input = self.base.base.args[0].eval_int(sctx, st, get_row(last_end + i))?;
            if input.is_none() {
                continue;
            }
            *p += 1;
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct CountOriginal4Real {
    pub base: BaseCount,
}

impl AggFunc for CountOriginal4Real {
    // glue: baseCount.AllocPartialResult through the embedded struct (M-SELF)
    fn alloc_partial_result(&self) -> PartialResult {
        self.base.alloc_partial_result()
    }

    // glue: baseCount.ResetPartialResult through the embedded struct (M-SELF)
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        self.base.reset_partial_result(pr)
    }

    // glue: baseCount.AppendFinalResult2Chunk through the embedded struct (M-SELF)
    fn append_final_result2_chunk(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        self.base.append_final_result2_chunk(sctx, pr, chk)
    }

    // go: countOriginal4Real.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4Count>(pr);

        for &row in rows_in_group {
            let input = self.base.base.args[0].eval_real(sctx, st, row)?;
            if input.is_none() {
                continue;
            }

            *p += 1;
        }

        Ok(())
    }

    // glue: Go type assertion agg.(SlidingWindowAggFunc) (spec §5.1)
    fn as_sliding_window(&self) -> Option<&dyn SlidingWindowAggFunc> {
        Some(self)
    }
}

impl SlidingWindowAggFunc for CountOriginal4Real {
    // go: countOriginal4Real.Slide L1
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
        let p = partial_result_mut::<PartialResult4Count>(pr);
        for i in 0..shift_start {
            let input = self.base.base.args[0].eval_real(sctx, st, get_row(last_start + i))?;
            if input.is_none() {
                continue;
            }
            *p -= 1;
        }
        for i in 0..shift_end {
            let input = self.base.base.args[0].eval_real(sctx, st, get_row(last_end + i))?;
            if input.is_none() {
                continue;
            }
            *p += 1;
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct CountOriginal4Decimal {
    pub base: BaseCount,
}

impl AggFunc for CountOriginal4Decimal {
    // glue: baseCount.AllocPartialResult through the embedded struct (M-SELF)
    fn alloc_partial_result(&self) -> PartialResult {
        self.base.alloc_partial_result()
    }

    // glue: baseCount.ResetPartialResult through the embedded struct (M-SELF)
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        self.base.reset_partial_result(pr)
    }

    // glue: baseCount.AppendFinalResult2Chunk through the embedded struct (M-SELF)
    fn append_final_result2_chunk(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        self.base.append_final_result2_chunk(sctx, pr, chk)
    }

    // go: countOriginal4Decimal.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4Count>(pr);

        for &row in rows_in_group {
            let input = self.base.base.args[0].eval_decimal(sctx, st, row)?;
            if input.is_none() {
                continue;
            }

            *p += 1;
        }

        Ok(())
    }

    // glue: Go type assertion agg.(SlidingWindowAggFunc) (spec §5.1)
    fn as_sliding_window(&self) -> Option<&dyn SlidingWindowAggFunc> {
        Some(self)
    }
}

impl SlidingWindowAggFunc for CountOriginal4Decimal {
    // go: countOriginal4Decimal.Slide L1
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
        let p = partial_result_mut::<PartialResult4Count>(pr);
        for i in 0..shift_start {
            let input = self.base.base.args[0].eval_decimal(sctx, st, get_row(last_start + i))?;
            if input.is_none() {
                continue;
            }
            *p -= 1;
        }
        for i in 0..shift_end {
            let input = self.base.base.args[0].eval_decimal(sctx, st, get_row(last_end + i))?;
            if input.is_none() {
                continue;
            }
            *p += 1;
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct CountOriginal4Time {
    pub base: BaseCount,
}

impl AggFunc for CountOriginal4Time {
    // glue: baseCount.AllocPartialResult through the embedded struct (M-SELF)
    fn alloc_partial_result(&self) -> PartialResult {
        self.base.alloc_partial_result()
    }

    // glue: baseCount.ResetPartialResult through the embedded struct (M-SELF)
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        self.base.reset_partial_result(pr)
    }

    // glue: baseCount.AppendFinalResult2Chunk through the embedded struct (M-SELF)
    fn append_final_result2_chunk(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        self.base.append_final_result2_chunk(sctx, pr, chk)
    }

    // go: countOriginal4Time.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4Count>(pr);

        for &row in rows_in_group {
            let input = self.base.base.args[0].eval_time(sctx, st, row)?;
            if input.is_none() {
                continue;
            }

            *p += 1;
        }

        Ok(())
    }

    // glue: Go type assertion agg.(SlidingWindowAggFunc) (spec §5.1)
    fn as_sliding_window(&self) -> Option<&dyn SlidingWindowAggFunc> {
        Some(self)
    }
}

impl SlidingWindowAggFunc for CountOriginal4Time {
    // go: countOriginal4Time.Slide L1
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
        let p = partial_result_mut::<PartialResult4Count>(pr);
        for i in 0..shift_start {
            let input = self.base.base.args[0].eval_time(sctx, st, get_row(last_start + i))?;
            if input.is_none() {
                continue;
            }
            *p -= 1;
        }
        for i in 0..shift_end {
            let input = self.base.base.args[0].eval_time(sctx, st, get_row(last_end + i))?;
            if input.is_none() {
                continue;
            }
            *p += 1;
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct CountOriginal4Duration {
    pub base: BaseCount,
}

impl AggFunc for CountOriginal4Duration {
    // glue: baseCount.AllocPartialResult through the embedded struct (M-SELF)
    fn alloc_partial_result(&self) -> PartialResult {
        self.base.alloc_partial_result()
    }

    // glue: baseCount.ResetPartialResult through the embedded struct (M-SELF)
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        self.base.reset_partial_result(pr)
    }

    // glue: baseCount.AppendFinalResult2Chunk through the embedded struct (M-SELF)
    fn append_final_result2_chunk(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        self.base.append_final_result2_chunk(sctx, pr, chk)
    }

    // go: countOriginal4Duration.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4Count>(pr);

        for &row in rows_in_group {
            let input = self.base.base.args[0].eval_duration(sctx, st, row)?;
            if input.is_none() {
                continue;
            }

            *p += 1;
        }

        Ok(())
    }

    // glue: Go type assertion agg.(SlidingWindowAggFunc) (spec §5.1)
    fn as_sliding_window(&self) -> Option<&dyn SlidingWindowAggFunc> {
        Some(self)
    }
}

impl SlidingWindowAggFunc for CountOriginal4Duration {
    // go: countOriginal4Duration.Slide L1
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
        let p = partial_result_mut::<PartialResult4Count>(pr);
        for i in 0..shift_start {
            let input = self.base.base.args[0].eval_duration(sctx, st, get_row(last_start + i))?;
            if input.is_none() {
                continue;
            }
            *p -= 1;
        }
        for i in 0..shift_end {
            let input = self.base.base.args[0].eval_duration(sctx, st, get_row(last_end + i))?;
            if input.is_none() {
                continue;
            }
            *p += 1;
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct CountOriginal4JSON {
    pub base: BaseCount,
}

impl AggFunc for CountOriginal4JSON {
    // glue: baseCount.AllocPartialResult through the embedded struct (M-SELF)
    fn alloc_partial_result(&self) -> PartialResult {
        self.base.alloc_partial_result()
    }

    // glue: baseCount.ResetPartialResult through the embedded struct (M-SELF)
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        self.base.reset_partial_result(pr)
    }

    // glue: baseCount.AppendFinalResult2Chunk through the embedded struct (M-SELF)
    fn append_final_result2_chunk(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        self.base.append_final_result2_chunk(sctx, pr, chk)
    }

    // go: countOriginal4JSON.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4Count>(pr);

        for &row in rows_in_group {
            let input = self.base.base.args[0].eval_json(sctx, st, row)?;
            if input.is_none() {
                continue;
            }

            *p += 1;
        }

        Ok(())
    }

    // glue: Go type assertion agg.(SlidingWindowAggFunc) (spec §5.1)
    fn as_sliding_window(&self) -> Option<&dyn SlidingWindowAggFunc> {
        Some(self)
    }
}

impl SlidingWindowAggFunc for CountOriginal4JSON {
    // go: countOriginal4JSON.Slide L1
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
        let p = partial_result_mut::<PartialResult4Count>(pr);
        for i in 0..shift_start {
            let input = self.base.base.args[0].eval_json(sctx, st, get_row(last_start + i))?;
            if input.is_none() {
                continue;
            }
            *p -= 1;
        }
        for i in 0..shift_end {
            let input = self.base.base.args[0].eval_json(sctx, st, get_row(last_end + i))?;
            if input.is_none() {
                continue;
            }
            *p += 1;
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct CountOriginal4VectorFloat32 {
    pub base: BaseCount,
}

impl AggFunc for CountOriginal4VectorFloat32 {
    // glue: baseCount.AllocPartialResult through the embedded struct (M-SELF)
    fn alloc_partial_result(&self) -> PartialResult {
        self.base.alloc_partial_result()
    }

    // glue: baseCount.ResetPartialResult through the embedded struct (M-SELF)
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        self.base.reset_partial_result(pr)
    }

    // glue: baseCount.AppendFinalResult2Chunk through the embedded struct (M-SELF)
    fn append_final_result2_chunk(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        self.base.append_final_result2_chunk(sctx, pr, chk)
    }

    // go: countOriginal4VectorFloat32.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4Count>(pr);

        for &row in rows_in_group {
            let input = self.base.base.args[0].eval_vector_float32(sctx, st, row)?;
            if input.is_none() {
                continue;
            }

            *p += 1;
        }

        Ok(())
    }

    // glue: Go type assertion agg.(SlidingWindowAggFunc) (spec §5.1)
    fn as_sliding_window(&self) -> Option<&dyn SlidingWindowAggFunc> {
        Some(self)
    }
}

impl SlidingWindowAggFunc for CountOriginal4VectorFloat32 {
    // go: countOriginal4VectorFloat32.Slide L1
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
        let p = partial_result_mut::<PartialResult4Count>(pr);
        for i in 0..shift_start {
            let input =
                self.base.base.args[0].eval_vector_float32(sctx, st, get_row(last_start + i))?;
            if input.is_none() {
                continue;
            }
            *p -= 1;
        }
        for i in 0..shift_end {
            let input =
                self.base.base.args[0].eval_vector_float32(sctx, st, get_row(last_end + i))?;
            if input.is_none() {
                continue;
            }
            *p += 1;
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct CountOriginal4String {
    pub base: BaseCount,
}

impl AggFunc for CountOriginal4String {
    // glue: baseCount.AllocPartialResult through the embedded struct (M-SELF)
    fn alloc_partial_result(&self) -> PartialResult {
        self.base.alloc_partial_result()
    }

    // glue: baseCount.ResetPartialResult through the embedded struct (M-SELF)
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        self.base.reset_partial_result(pr)
    }

    // glue: baseCount.AppendFinalResult2Chunk through the embedded struct (M-SELF)
    fn append_final_result2_chunk(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        self.base.append_final_result2_chunk(sctx, pr, chk)
    }

    // go: countOriginal4String.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4Count>(pr);

        for &row in rows_in_group {
            let input = self.base.base.args[0].eval_string(sctx, st, row)?;
            if input.is_none() {
                continue;
            }

            *p += 1;
        }

        Ok(())
    }

    // glue: Go type assertion agg.(SlidingWindowAggFunc) (spec §5.1)
    fn as_sliding_window(&self) -> Option<&dyn SlidingWindowAggFunc> {
        Some(self)
    }
}

impl SlidingWindowAggFunc for CountOriginal4String {
    // go: countOriginal4String.Slide L1
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
        let p = partial_result_mut::<PartialResult4Count>(pr);
        for i in 0..shift_start {
            let input = self.base.base.args[0].eval_string(sctx, st, get_row(last_start + i))?;
            if input.is_none() {
                continue;
            }
            *p -= 1;
        }
        for i in 0..shift_end {
            let input = self.base.base.args[0].eval_string(sctx, st, get_row(last_end + i))?;
            if input.is_none() {
                continue;
            }
            *p += 1;
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct CountPartial {
    pub base: BaseCount,
}

impl AggFunc for CountPartial {
    // glue: baseCount.AllocPartialResult through the embedded struct (M-SELF)
    fn alloc_partial_result(&self) -> PartialResult {
        self.base.alloc_partial_result()
    }

    // glue: baseCount.ResetPartialResult through the embedded struct (M-SELF)
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        self.base.reset_partial_result(pr)
    }

    // glue: baseCount.AppendFinalResult2Chunk through the embedded struct (M-SELF)
    fn append_final_result2_chunk(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        self.base.append_final_result2_chunk(sctx, pr, chk)
    }

    // go: countPartial.UpdatePartialResult L2 (memDelta dropped, A1; the sum wraps as Go's int64, M-INT)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4Count>(pr);
        for &row in rows_in_group {
            let Some(input) = self.base.base.args[0].eval_int(sctx, st, row)? else {
                continue;
            };

            *p = p.wrapping_add(input);
        }
        Ok(())
    }

    // go: countPartial.MergePartialResult L2 (memDelta dropped, A1; M-INT)
    fn merge_partial_result(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        src: &mut PartialResult,
        dst: &mut PartialResult,
    ) -> Result<(), Error> {
        let p1 = *partial_result_mut::<PartialResult4Count>(src);
        let p2 = partial_result_mut::<PartialResult4Count>(dst);
        *p2 = p2.wrapping_add(p1);
        Ok(())
    }
}
