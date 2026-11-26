//! go-file: pkg/executor/aggfuncs/func_percent_rank.go
//!
//! `PERCENT_RANK` window function (part W of D12.4).

use tidb_base::Error;
use tidb_chunk::ColAlloc;
use tidb_chunk::chunk::Chunk;
use tidb_chunk::row::{Row, RowHandle};
use tidb_expression::expression::EvalState;

use super::aggfuncs::{
    AggFunc, AggFuncUpdateContext, BaseAggFunc, PartialResult, partial_result_mut, push_rows_in,
    row_of,
};
use super::func_rank::{PartialResult4Rank, RowComparer};

/// `percentRank` calculates the percentage of partition values less than the value in the
/// current row, excluding the highest value: `(rank - 1) / (total_rows_in_set - 1)`.
pub struct PercentRank {
    pub base: BaseAggFunc,
    pub row_comparer: RowComparer,
    alloc: ColAlloc,
}

impl PercentRank {
    // glue: Go struct literal percentRank{baseAggFunc, rowComparer}
    pub fn new(base: BaseAggFunc, row_comparer: RowComparer) -> Self {
        Self {
            base,
            row_comparer,
            alloc: ColAlloc::Global,
        }
    }
}

impl AggFunc for PercentRank {
    // go: percentRank.AllocPartialResult L2 (memDelta dropped, A1; rows allocated in ColAlloc)
    fn alloc_partial_result(&self) -> PartialResult {
        Box::new(PartialResult4Rank::new(self.alloc.clone()))
    }

    // go: percentRank.ResetPartialResult L1
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4Rank>(pr);
        p.cur_idx = 0;
        p.last_rank = 0;
        p.rows.clear();
    }

    // go: percentRank.UpdatePartialResult L2 (memDelta dropped, A1; M-ROW)
    fn update_partial_result(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        _st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4Rank>(pr);
        push_rows_in(&mut p.rows, rows_in_group, &p.alloc);
        Ok(())
    }

    // glue: M-ROW zero-copy RowHandle ingestion for WindowExec (spec §5.2)
    fn update_partial_result_handles(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        _st: &EvalState,
        rows_in_group: &[RowHandle],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4Rank>(pr);
        p.rows.extend(rows_in_group.iter().cloned().map(Some));
        Ok(())
    }

    // go: percentRank.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4Rank>(pr);
        let num_rows = p.rows.len() as i64;
        p.cur_idx += 1;
        let col = self.base.col();
        if p.cur_idx == 1 {
            p.last_rank = 1;
            chk.append_float64(col, 0.0);
            return Ok(());
        }
        if self.row_comparer.compare_rows(
            row_of(&p.rows[(p.cur_idx - 2) as usize]),
            row_of(&p.rows[(p.cur_idx - 1) as usize]),
        ) == 0
        {
            chk.append_float64(col, (p.last_rank - 1) as f64 / (num_rows - 1) as f64);
            return Ok(());
        }
        p.last_rank = p.cur_idx;
        chk.append_float64(col, (p.last_rank - 1) as f64 / (num_rows - 1) as f64);
        Ok(())
    }

    // glue: A1 3.3 TrackedAlloc for stored row handles
    fn set_alloc(&mut self, alloc: ColAlloc) {
        self.alloc = alloc;
    }
}
