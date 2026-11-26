//! go-file: pkg/executor/aggfuncs/func_cume_dist.go
//!
//! `CUME_DIST` window function (part W of D12.4).

use allocator_api2::vec::Vec as AllocVec;
use tidb_base::Error;
use tidb_chunk::ColAlloc;
use tidb_chunk::chunk::Chunk;
use tidb_chunk::row::{Row, RowHandle};
use tidb_expression::expression::EvalState;

use super::aggfuncs::{
    AggFunc, AggFuncUpdateContext, BaseAggFunc, PartialResult, partial_result_mut, push_rows_in,
    row_of,
};
use super::func_rank::RowComparer;

/// `cumeDist`.
pub struct CumeDist {
    pub base: BaseAggFunc,
    pub row_comparer: RowComparer,
    alloc: ColAlloc,
}

impl CumeDist {
    // glue: Go struct literal cumeDist{baseAggFunc, rowComparer}
    pub fn new(base: BaseAggFunc, row_comparer: RowComparer) -> Self {
        Self {
            base,
            row_comparer,
            alloc: ColAlloc::Global,
        }
    }
}

struct PartialResult4CumeDist {
    cur_idx: usize,
    last_rank: usize,
    rows: AllocVec<Option<RowHandle>, ColAlloc>,
    alloc: ColAlloc,
}

impl AggFunc for CumeDist {
    // go: cumeDist.AllocPartialResult L2 (memDelta dropped, A1; rows allocated in ColAlloc)
    fn alloc_partial_result(&self) -> PartialResult {
        Box::new(PartialResult4CumeDist {
            cur_idx: 0,
            last_rank: 0,
            rows: AllocVec::new_in(self.alloc.clone()),
            alloc: self.alloc.clone(),
        })
    }

    // go: cumeDist.ResetPartialResult L1
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4CumeDist>(pr);
        p.cur_idx = 0;
        p.last_rank = 0;
        p.rows.clear();
    }

    // go: cumeDist.UpdatePartialResult L2 (memDelta dropped, A1; M-ROW)
    fn update_partial_result(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        _st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4CumeDist>(pr);
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
        let p = partial_result_mut::<PartialResult4CumeDist>(pr);
        p.rows.extend(rows_in_group.iter().cloned().map(Some));
        Ok(())
    }

    // go: cumeDist.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4CumeDist>(pr);
        let num_rows = p.rows.len();
        while p.last_rank < num_rows
            && self
                .row_comparer
                .compare_rows(row_of(&p.rows[p.cur_idx]), row_of(&p.rows[p.last_rank]))
                == 0
        {
            p.last_rank += 1;
        }
        p.cur_idx += 1;
        chk.append_float64(self.base.col(), p.last_rank as f64 / num_rows as f64);
        Ok(())
    }

    // glue: A1 3.3 TrackedAlloc for stored row handles
    fn set_alloc(&mut self, alloc: ColAlloc) {
        self.alloc = alloc;
    }
}
