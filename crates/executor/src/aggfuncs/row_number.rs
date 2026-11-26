//! go-file: pkg/executor/aggfuncs/row_number.go
//!
//! `ROW_NUMBER` window function (part W of D12.4).

use tidb_base::Error;
use tidb_chunk::chunk::Chunk;
use tidb_chunk::row::Row;
use tidb_expression::expression::EvalState;

use super::aggfuncs::{
    AggFunc, AggFuncUpdateContext, BaseAggFunc, PartialResult, SlidingWindowAggFunc,
    partial_result_mut,
};

/// `rowNumber`.
#[derive(Clone, Debug)]
pub struct RowNumber {
    pub base: BaseAggFunc,
}

#[derive(Default)]
struct PartialResult4RowNumber {
    cur_idx: i64,
}

impl AggFunc for RowNumber {
    // go: rowNumber.AllocPartialResult L2 (memDelta dropped, A1)
    fn alloc_partial_result(&self) -> PartialResult {
        Box::new(PartialResult4RowNumber::default())
    }

    // go: rowNumber.ResetPartialResult L1
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4RowNumber>(pr);
        p.cur_idx = 0;
    }

    // go: rowNumber.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        _st: &EvalState,
        _rows_in_group: &[Row<'_>],
        _pr: &mut PartialResult,
    ) -> Result<(), Error> {
        Ok(())
    }

    // go: rowNumber.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4RowNumber>(pr);
        p.cur_idx += 1;
        chk.append_int64(self.base.col(), p.cur_idx);
        Ok(())
    }

    // glue: Go type assertion agg.(SlidingWindowAggFunc) (spec §5.1)
    fn as_sliding_window(&self) -> Option<&dyn SlidingWindowAggFunc> {
        Some(self)
    }
}

impl SlidingWindowAggFunc for RowNumber {
    // go: rowNumber.Slide L1
    fn slide<'a>(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        _st: &EvalState,
        _get_row: &dyn Fn(u64) -> Row<'a>,
        _last_start: u64,
        _last_end: u64,
        _shift_start: u64,
        _shift_end: u64,
        _pr: &mut PartialResult,
    ) -> Result<(), Error> {
        Ok(())
    }
}
