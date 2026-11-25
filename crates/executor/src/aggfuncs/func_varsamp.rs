//! go-file: pkg/executor/aggfuncs/func_varsamp.go
//!
//! `VAR_SAMP` (part B1 of D12.4: sample variance, embeds `varPop4Float64` /
//! `varPop4DistinctFloat64`).

use tidb_base::Error;
use tidb_chunk::ColAlloc;
use tidb_chunk::chunk::Chunk;
use tidb_chunk::row::Row;
use tidb_expression::expression::EvalState;

use super::aggfuncs::{AggFunc, AggFuncUpdateContext, PartialResult, partial_result_mut};
use super::func_varpop::{
    PartialResult4VarPopDistinctFloat64, PartialResult4VarPopFloat64, VarPop4DistinctFloat64,
    VarPop4Float64,
};

/// varSamp4Float64.
#[derive(Clone, Debug)]
pub struct VarSamp4Float64 {
    pub base: VarPop4Float64,
}

impl AggFunc for VarSamp4Float64 {
    // glue: varPop4Float64.AllocPartialResult through the embedded struct (M-SELF)
    fn alloc_partial_result(&self) -> PartialResult {
        self.base.alloc_partial_result()
    }

    // glue: varPop4Float64.ResetPartialResult through the embedded struct (M-SELF)
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        self.base.reset_partial_result(pr)
    }

    // glue: varPop4Float64.UpdatePartialResult through the embedded struct (M-SELF)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        self.base.update_partial_result(sctx, st, rows_in_group, pr)
    }

    // glue: varPop4Float64.MergePartialResult through the embedded struct (M-SELF)
    fn merge_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        src: &mut PartialResult,
        dst: &mut PartialResult,
    ) -> Result<(), Error> {
        self.base.merge_partial_result(sctx, src, dst)
    }

    // go: varSamp4Float64.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4VarPopFloat64>(pr);
        if p.count <= 1 {
            chk.append_null(self.base.base.base.col());
            return Ok(());
        }
        let variance = p.variance / (p.count - 1) as f64;
        chk.append_float64(self.base.base.base.col(), variance);
        Ok(())
    }
}

/// varSamp4DistinctFloat64.
#[derive(Clone, Debug)]
pub struct VarSamp4DistinctFloat64 {
    pub base: VarPop4DistinctFloat64,
}

impl AggFunc for VarSamp4DistinctFloat64 {
    // glue: varPop4DistinctFloat64.AllocPartialResult through the embedded struct (M-SELF)
    fn alloc_partial_result(&self) -> PartialResult {
        self.base.alloc_partial_result()
    }

    // glue: varPop4DistinctFloat64.ResetPartialResult through the embedded struct (M-SELF)
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        self.base.reset_partial_result(pr)
    }

    // glue: varPop4DistinctFloat64.UpdatePartialResult through the embedded struct (M-SELF)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        self.base.update_partial_result(sctx, st, rows_in_group, pr)
    }

    // go: varSamp4DistinctFloat64.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4VarPopDistinctFloat64>(pr);
        if p.count <= 1 {
            chk.append_null(self.base.base.base.col());
            return Ok(());
        }
        let variance = p.variance / (p.count - 1) as f64;
        chk.append_float64(self.base.base.base.col(), variance);
        Ok(())
    }

    // glue: A1 3.3 TrackedAlloc of the embedded DISTINCT set (spec §5.3)
    fn set_alloc(&mut self, alloc: ColAlloc) {
        self.base.set_alloc(alloc);
    }
}
