//! go-file: pkg/executor/aggfuncs/func_rank.go
//!
//! `RANK` / `DENSE_RANK` and the shared [`RowComparer`] used by `RANK`, `DENSE_RANK`,
//! `PERCENT_RANK`, and `CUME_DIST` (part W of D12.4).
//!
//! Stored rows use [`RowHandle`] (M-ROW, spec §5.2) in a [`ColAlloc`]-backed vector (A1,
//! spec §5.3).

use allocator_api2::vec::Vec as AllocVec;
use tidb_base::Error;
use tidb_chunk::ColAlloc;
use tidb_chunk::chunk::Chunk;
use tidb_chunk::compare::{CompareFunc, get_compare_func};
use tidb_chunk::row::{Row, RowHandle};
use tidb_expression::Column;
use tidb_expression::expression::EvalState;

use super::aggfuncs::{
    AggFunc, AggFuncUpdateContext, BaseAggFunc, PartialResult, partial_result_mut, push_rows_in,
    row_of,
};

/// `rank` (both `RANK` and `DENSE_RANK`).
pub struct Rank {
    pub base: BaseAggFunc,
    pub is_dense: bool,
    pub row_comparer: RowComparer,
    alloc: ColAlloc,
}

impl Rank {
    // glue: Go struct literal rank{baseAggFunc, isDense, rowComparer}
    pub fn new(base: BaseAggFunc, is_dense: bool, row_comparer: RowComparer) -> Self {
        Self {
            base,
            is_dense,
            row_comparer,
            alloc: ColAlloc::Global,
        }
    }
}

pub(crate) struct PartialResult4Rank {
    pub(crate) cur_idx: i64,
    pub(crate) last_rank: i64,
    pub(crate) rows: AllocVec<Option<RowHandle>, ColAlloc>,
    pub(crate) alloc: ColAlloc,
}

impl PartialResult4Rank {
    // glue: allocate partialResult4Rank in ColAlloc (A1 3.3)
    pub(crate) fn new(alloc: ColAlloc) -> Self {
        Self {
            cur_idx: 0,
            last_rank: 0,
            rows: AllocVec::new_in(alloc.clone()),
            alloc,
        }
    }
}

impl AggFunc for Rank {
    // go: rank.AllocPartialResult L2 (memDelta dropped, A1; rows allocated in ColAlloc)
    fn alloc_partial_result(&self) -> PartialResult {
        Box::new(PartialResult4Rank::new(self.alloc.clone()))
    }

    // go: rank.ResetPartialResult L1
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4Rank>(pr);
        p.cur_idx = 0;
        p.last_rank = 0;
        p.rows.clear();
    }

    // go: rank.UpdatePartialResult L2 (memDelta dropped, A1; M-ROW)
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

    // go: rank.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4Rank>(pr);
        p.cur_idx += 1;
        let col = self.base.col();
        if p.cur_idx == 1 {
            p.last_rank = 1;
            chk.append_int64(col, p.last_rank);
            return Ok(());
        }
        if self.row_comparer.compare_rows(
            row_of(&p.rows[(p.cur_idx - 2) as usize]),
            row_of(&p.rows[(p.cur_idx - 1) as usize]),
        ) == 0
        {
            chk.append_int64(col, p.last_rank);
            return Ok(());
        }
        if self.is_dense {
            p.last_rank += 1;
        } else {
            p.last_rank = p.cur_idx;
        }
        chk.append_int64(col, p.last_rank);
        Ok(())
    }

    // glue: A1 3.3 TrackedAlloc for stored row handles
    fn set_alloc(&mut self, alloc: ColAlloc) {
        self.alloc = alloc;
    }
}

/// `rowComparer` compares two rows on the ORDER BY columns of a window function.
pub struct RowComparer {
    cmp_funcs: Vec<CompareFunc>,
    col_idx: Vec<usize>,
}

// go: buildRowComparer L1
pub fn build_row_comparer(cols: &[Column]) -> RowComparer {
    let mut rc = RowComparer {
        cmp_funcs: Vec::with_capacity(cols.len()),
        col_idx: Vec::with_capacity(cols.len()),
    };
    for col in cols {
        let Some(cmp_func) = get_compare_func(&col.ret_type) else {
            continue;
        };
        rc.cmp_funcs.push(cmp_func);
        rc.col_idx.push(col.index as usize);
    }
    rc
}

impl RowComparer {
    // go: rowComparer.compareRows L1
    pub fn compare_rows(&self, prev: Row<'_>, curr: Row<'_>) -> isize {
        for (i, &idx) in self.col_idx.iter().enumerate() {
            let res = (self.cmp_funcs[i])(prev, idx, curr, idx);
            if res != 0 {
                return res;
            }
        }
        0
    }
}
