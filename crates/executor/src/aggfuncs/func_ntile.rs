//! go-file: pkg/executor/aggfuncs/func_ntile.go
//!
//! `NTILE` window function (part W of D12.4).

use tidb_base::Error;
use tidb_chunk::chunk::Chunk;
use tidb_chunk::row::Row;
use tidb_expression::expression::EvalState;

use super::aggfuncs::{
    AggFunc, AggFuncUpdateContext, BaseAggFunc, PartialResult, partial_result_mut,
};

/// `ntile` divides the partition into `n` ranked groups and returns the group number a row
/// belongs to.
#[derive(Clone, Debug)]
pub struct Ntile {
    pub n: u64,
    pub base: BaseAggFunc,
}

#[derive(Default)]
struct PartialResult4Ntile {
    cur_idx: u64,
    cur_group_idx: u64,
    remainder: u64,
    quotient: u64,
    num_rows: u64,
}

impl AggFunc for Ntile {
    // go: ntile.AllocPartialResult L2 (memDelta dropped, A1)
    fn alloc_partial_result(&self) -> PartialResult {
        Box::new(PartialResult4Ntile {
            cur_group_idx: 1,
            ..PartialResult4Ntile::default()
        })
    }

    // go: ntile.ResetPartialResult L1
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4Ntile>(pr);
        p.cur_idx = 0;
        p.cur_group_idx = 1;
        p.num_rows = 0;
    }

    // go: ntile.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        _st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4Ntile>(pr);
        p.num_rows += rows_in_group.len() as u64;
        // Update the quotient and remainder.
        if let Some(q) = p.num_rows.checked_div(self.n) {
            p.quotient = q;
            p.remainder = p.num_rows % self.n;
        }
        Ok(())
    }

    // go: ntile.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4Ntile>(pr);
        let col = self.base.col();

        // If the divisor is 0, the arg of NTILE would be NULL. So we just return NULL.
        if self.n == 0 {
            chk.append_null(col);
            return Ok(());
        }

        chk.append_uint64(col, p.cur_group_idx);

        p.cur_idx += 1;
        let mut cur_max_idx = p.quotient;
        if p.cur_group_idx <= p.remainder {
            cur_max_idx += 1;
        }
        if p.cur_idx == cur_max_idx {
            p.cur_idx = 0;
            p.cur_group_idx += 1;
        }
        Ok(())
    }
}
