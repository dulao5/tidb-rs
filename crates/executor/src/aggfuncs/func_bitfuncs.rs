//! go-file: pkg/executor/aggfuncs/func_bitfuncs.go
//!
//! BIT_OR, BIT_XOR and BIT_AND over the first argument evaluated as an integer (part A of
//! D12.4). Their builders leave `retTp` nil (Go), so `BaseAggFunc::ret_tp` is `None`.

use tidb_base::Error;
use tidb_chunk::chunk::Chunk;
use tidb_chunk::row::Row;
use tidb_expression::expression::EvalState;

use super::aggfuncs::{
    AggFunc, AggFuncUpdateContext, BaseAggFunc, PartialResult, SlidingWindowAggFunc,
    partial_result_mut,
};

/// baseBitAggFunc, embedded in the bit functions.
#[derive(Clone, Debug)]
pub struct BaseBitAggFunc {
    pub base: BaseAggFunc,
}

type PartialResult4BitFunc = u64;

impl BaseBitAggFunc {
    // go: baseBitAggFunc.AllocPartialResult L2 (memDelta dropped, A1)
    pub fn alloc_partial_result(&self) -> PartialResult {
        Box::new(PartialResult4BitFunc::default())
    }

    // go: baseBitAggFunc.ResetPartialResult L1
    pub fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4BitFunc>(pr);
        *p = 0;
    }

    // go: baseBitAggFunc.AppendFinalResult2Chunk L1
    pub fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4BitFunc>(pr);
        chk.append_uint64(self.base.col(), *p);
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct BitOrUint64 {
    pub base: BaseBitAggFunc,
}

impl AggFunc for BitOrUint64 {
    // glue: baseBitAggFunc.AllocPartialResult through the embedded struct (M-SELF)
    fn alloc_partial_result(&self) -> PartialResult {
        self.base.alloc_partial_result()
    }

    // glue: baseBitAggFunc.ResetPartialResult through the embedded struct (M-SELF)
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        self.base.reset_partial_result(pr)
    }

    // glue: baseBitAggFunc.AppendFinalResult2Chunk through the embedded struct (M-SELF)
    fn append_final_result2_chunk(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        self.base.append_final_result2_chunk(sctx, pr, chk)
    }

    // go: bitOrUint64.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4BitFunc>(pr);
        for &row in rows_in_group {
            let Some(input_value) = self.base.base.args[0].eval_int(sctx, st, row)? else {
                continue;
            };
            *p |= input_value as u64;
        }
        Ok(())
    }

    // go: bitOrUint64.MergePartialResult L2 (memDelta dropped, A1)
    fn merge_partial_result(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        src: &mut PartialResult,
        dst: &mut PartialResult,
    ) -> Result<(), Error> {
        let p1 = *partial_result_mut::<PartialResult4BitFunc>(src);
        let p2 = partial_result_mut::<PartialResult4BitFunc>(dst);
        *p2 |= p1;
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct BitXorUint64 {
    pub base: BaseBitAggFunc,
}

impl AggFunc for BitXorUint64 {
    // glue: baseBitAggFunc.AllocPartialResult through the embedded struct (M-SELF)
    fn alloc_partial_result(&self) -> PartialResult {
        self.base.alloc_partial_result()
    }

    // glue: baseBitAggFunc.ResetPartialResult through the embedded struct (M-SELF)
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        self.base.reset_partial_result(pr)
    }

    // glue: baseBitAggFunc.AppendFinalResult2Chunk through the embedded struct (M-SELF)
    fn append_final_result2_chunk(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        self.base.append_final_result2_chunk(sctx, pr, chk)
    }

    // go: bitXorUint64.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4BitFunc>(pr);
        for &row in rows_in_group {
            let Some(input_value) = self.base.base.args[0].eval_int(sctx, st, row)? else {
                continue;
            };
            *p ^= input_value as u64;
        }
        Ok(())
    }

    // go: bitXorUint64.MergePartialResult L2 (memDelta dropped, A1)
    fn merge_partial_result(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        src: &mut PartialResult,
        dst: &mut PartialResult,
    ) -> Result<(), Error> {
        let p1 = *partial_result_mut::<PartialResult4BitFunc>(src);
        let p2 = partial_result_mut::<PartialResult4BitFunc>(dst);
        *p2 ^= p1;
        Ok(())
    }

    // glue: Go type assertion agg.(SlidingWindowAggFunc) (spec §5.1)
    fn as_sliding_window(&self) -> Option<&dyn SlidingWindowAggFunc> {
        Some(self)
    }
}

impl SlidingWindowAggFunc for BitXorUint64 {
    // go: bitXorUint64.Slide L1
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
        let p = partial_result_mut::<PartialResult4BitFunc>(pr);
        for i in 0..shift_start {
            let Some(input_value) =
                self.base.base.args[0].eval_int(sctx, st, get_row(last_start + i))?
            else {
                continue;
            };
            *p ^= input_value as u64;
        }
        for i in 0..shift_end {
            let Some(input_value) =
                self.base.base.args[0].eval_int(sctx, st, get_row(last_end + i))?
            else {
                continue;
            };
            *p ^= input_value as u64;
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct BitAndUint64 {
    pub base: BaseBitAggFunc,
}

impl AggFunc for BitAndUint64 {
    // go: bitAndUint64.AllocPartialResult L2 (memDelta dropped, A1)
    fn alloc_partial_result(&self) -> PartialResult {
        let p: PartialResult4BitFunc = u64::MAX;
        Box::new(p)
    }

    // go: bitAndUint64.ResetPartialResult L1
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4BitFunc>(pr);
        *p = u64::MAX;
    }

    // glue: baseBitAggFunc.AppendFinalResult2Chunk through the embedded struct (M-SELF)
    fn append_final_result2_chunk(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        self.base.append_final_result2_chunk(sctx, pr, chk)
    }

    // go: bitAndUint64.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4BitFunc>(pr);
        for &row in rows_in_group {
            let Some(input_value) = self.base.base.args[0].eval_int(sctx, st, row)? else {
                continue;
            };
            *p &= input_value as u64;
        }
        Ok(())
    }

    // go: bitAndUint64.MergePartialResult L2 (memDelta dropped, A1)
    fn merge_partial_result(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        src: &mut PartialResult,
        dst: &mut PartialResult,
    ) -> Result<(), Error> {
        let p1 = *partial_result_mut::<PartialResult4BitFunc>(src);
        let p2 = partial_result_mut::<PartialResult4BitFunc>(dst);
        *p2 &= p1;
        Ok(())
    }
}
