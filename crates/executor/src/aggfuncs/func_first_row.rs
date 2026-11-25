//! go-file: pkg/executor/aggfuncs/func_first_row.go
//!
//! FIRST_ROW (part A of D12.4): the value of the first argument on the first row of the
//! group, NULL included. One implementation per result type, as in Go; ENUM and SET are
//! evaluated as datums. The partial results are one generic struct (Go has one struct per
//! type with the same fields); the window functions' parts (part W) and the spill
//! serialization (part S) come later.

use tidb_base::Error;
use tidb_chunk::chunk::Chunk;
use tidb_chunk::row::Row;
use tidb_expression::expression::EvalState;
use tidb_types::MyDecimal;
use tidb_types::enum_type::Enum;
use tidb_types::json::BinaryJSON;
use tidb_types::set_type::Set;
use tidb_types::stringutil;
use tidb_types::time::{Duration, Time};
use tidb_types::vector::{VectorFloat32, ZeroVectorFloat32};

use super::aggfuncs::{
    AggFunc, AggFuncUpdateContext, BaseAggFunc, PartialResult, partial_result_mut,
};
use super::func_sum::round_to_ret_tp;

/// `basePartialResult4FirstRow` and the value of `partialResult4FirstRowXxx`.
#[derive(Debug)]
struct PartialResult4FirstRow<T> {
    // isNull indicates whether the first row is null.
    is_null: bool,
    // gotFirstRow indicates whether the first row has been got,
    // if so, we would avoid evaluating the values of the remained rows.
    got_first_row: bool,

    val: T,
}

/// firstRow4Int.
#[derive(Clone, Debug)]
pub struct FirstRow4Int {
    pub base: BaseAggFunc,
}

impl AggFunc for FirstRow4Int {
    // go: firstRow4Int.AllocPartialResult L2 (memDelta dropped, A1)
    fn alloc_partial_result(&self) -> PartialResult {
        Box::new(PartialResult4FirstRow::<i64> {
            is_null: false,
            got_first_row: false,
            val: 0,
        })
    }

    // go: firstRow4Int.ResetPartialResult L1
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4FirstRow<i64>>(pr);
        p.val = 0;
        p.is_null = false;
        p.got_first_row = false;
    }

    // go: firstRow4Int.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4FirstRow<i64>>(pr);
        if p.got_first_row {
            return Ok(());
        }
        if let Some(&row) = rows_in_group.first() {
            let input = self.base.args[0].eval_int(sctx, st, row)?;
            p.got_first_row = true;
            p.is_null = input.is_none();
            // Go stores the zero value of a NULL; the value is never read then.
            p.val = input.unwrap_or_default();
        }
        Ok(())
    }

    /// Go copies `src` into `dst` (`*p2 = *p1`); Rust clones it.
    // go: firstRow4Int.MergePartialResult L2 (memDelta dropped, A1)
    fn merge_partial_result(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        src: &mut PartialResult,
        dst: &mut PartialResult,
    ) -> Result<(), Error> {
        let p1 = partial_result_mut::<PartialResult4FirstRow<i64>>(src);
        let p2 = partial_result_mut::<PartialResult4FirstRow<i64>>(dst);
        if !p2.got_first_row {
            *p2 = PartialResult4FirstRow {
                is_null: p1.is_null,
                got_first_row: p1.got_first_row,
                val: p1.val,
            };
        }
        Ok(())
    }

    // go: firstRow4Int.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4FirstRow<i64>>(pr);
        let col = self.base.col();
        if p.is_null || !p.got_first_row {
            chk.append_null(col);
            return Ok(());
        }
        chk.append_int64(col, p.val);
        Ok(())
    }
}

/// firstRow4Float32.
#[derive(Clone, Debug)]
pub struct FirstRow4Float32 {
    pub base: BaseAggFunc,
}

impl AggFunc for FirstRow4Float32 {
    // go: firstRow4Float32.AllocPartialResult L2 (memDelta dropped, A1)
    fn alloc_partial_result(&self) -> PartialResult {
        Box::new(PartialResult4FirstRow::<f32> {
            is_null: false,
            got_first_row: false,
            val: 0.0,
        })
    }

    // go: firstRow4Float32.ResetPartialResult L1
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4FirstRow<f32>>(pr);
        p.is_null = false;
        p.got_first_row = false;
    }

    // go: firstRow4Float32.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4FirstRow<f32>>(pr);
        if p.got_first_row {
            return Ok(());
        }
        if let Some(&row) = rows_in_group.first() {
            let input = self.base.args[0].eval_real(sctx, st, row)?;
            p.got_first_row = true;
            p.is_null = input.is_none();
            // Go stores the zero value of a NULL; the value is never read then.
            p.val = input.map(|v| v as f32).unwrap_or_default();
        }
        Ok(())
    }

    /// Go copies `src` into `dst` (`*p2 = *p1`); Rust clones it.
    // go: firstRow4Float32.MergePartialResult L2 (memDelta dropped, A1)
    fn merge_partial_result(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        src: &mut PartialResult,
        dst: &mut PartialResult,
    ) -> Result<(), Error> {
        let p1 = partial_result_mut::<PartialResult4FirstRow<f32>>(src);
        let p2 = partial_result_mut::<PartialResult4FirstRow<f32>>(dst);
        if !p2.got_first_row {
            *p2 = PartialResult4FirstRow {
                is_null: p1.is_null,
                got_first_row: p1.got_first_row,
                val: p1.val,
            };
        }
        Ok(())
    }

    // go: firstRow4Float32.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4FirstRow<f32>>(pr);
        let col = self.base.col();
        if p.is_null || !p.got_first_row {
            chk.append_null(col);
            return Ok(());
        }
        chk.append_float32(col, p.val);
        Ok(())
    }
}

/// firstRow4Float64.
#[derive(Clone, Debug)]
pub struct FirstRow4Float64 {
    pub base: BaseAggFunc,
}

impl AggFunc for FirstRow4Float64 {
    // go: firstRow4Float64.AllocPartialResult L2 (memDelta dropped, A1)
    fn alloc_partial_result(&self) -> PartialResult {
        Box::new(PartialResult4FirstRow::<f64> {
            is_null: false,
            got_first_row: false,
            val: 0.0,
        })
    }

    // go: firstRow4Float64.ResetPartialResult L1
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4FirstRow<f64>>(pr);
        p.is_null = false;
        p.got_first_row = false;
    }

    // go: firstRow4Float64.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4FirstRow<f64>>(pr);
        if p.got_first_row {
            return Ok(());
        }
        if let Some(&row) = rows_in_group.first() {
            let input = self.base.args[0].eval_real(sctx, st, row)?;
            p.got_first_row = true;
            p.is_null = input.is_none();
            // Go stores the zero value of a NULL; the value is never read then.
            p.val = input.unwrap_or_default();
        }
        Ok(())
    }

    /// Go copies `src` into `dst` (`*p2 = *p1`); Rust clones it.
    // go: firstRow4Float64.MergePartialResult L2 (memDelta dropped, A1)
    fn merge_partial_result(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        src: &mut PartialResult,
        dst: &mut PartialResult,
    ) -> Result<(), Error> {
        let p1 = partial_result_mut::<PartialResult4FirstRow<f64>>(src);
        let p2 = partial_result_mut::<PartialResult4FirstRow<f64>>(dst);
        if !p2.got_first_row {
            *p2 = PartialResult4FirstRow {
                is_null: p1.is_null,
                got_first_row: p1.got_first_row,
                val: p1.val,
            };
        }
        Ok(())
    }

    // go: firstRow4Float64.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4FirstRow<f64>>(pr);
        let col = self.base.col();
        if p.is_null || !p.got_first_row {
            chk.append_null(col);
            return Ok(());
        }
        chk.append_float64(col, p.val);
        Ok(())
    }
}

/// firstRow4String.
#[derive(Clone, Debug)]
pub struct FirstRow4String {
    pub base: BaseAggFunc,
}

impl AggFunc for FirstRow4String {
    // go: firstRow4String.AllocPartialResult L2 (memDelta dropped, A1)
    fn alloc_partial_result(&self) -> PartialResult {
        Box::new(PartialResult4FirstRow::<Vec<u8>> {
            is_null: false,
            got_first_row: false,
            val: Vec::new(),
        })
    }

    // go: firstRow4String.ResetPartialResult L1
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4FirstRow<Vec<u8>>>(pr);
        p.is_null = false;
        p.got_first_row = false;
    }

    // go: firstRow4String.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4FirstRow<Vec<u8>>>(pr);
        if p.got_first_row {
            return Ok(());
        }
        if let Some(&row) = rows_in_group.first() {
            let input = self.base.args[0].eval_string(sctx, st, row)?;
            p.got_first_row = true;
            p.is_null = input.is_none();
            // Go stores the zero value of a NULL; the value is never read then.
            p.val = input.map(|v| stringutil::copy(&v)).unwrap_or_default();
        }
        Ok(())
    }

    /// Go copies `src` into `dst` (`*p2 = *p1`); Rust clones it.
    // go: firstRow4String.MergePartialResult L2 (memDelta dropped, A1)
    fn merge_partial_result(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        src: &mut PartialResult,
        dst: &mut PartialResult,
    ) -> Result<(), Error> {
        let p1 = partial_result_mut::<PartialResult4FirstRow<Vec<u8>>>(src);
        let p2 = partial_result_mut::<PartialResult4FirstRow<Vec<u8>>>(dst);
        if !p2.got_first_row {
            *p2 = PartialResult4FirstRow {
                is_null: p1.is_null,
                got_first_row: p1.got_first_row,
                val: p1.val.clone(),
            };
        }
        Ok(())
    }

    // go: firstRow4String.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4FirstRow<Vec<u8>>>(pr);
        let col = self.base.col();
        if p.is_null || !p.got_first_row {
            chk.append_null(col);
            return Ok(());
        }
        chk.append_string(col, &p.val);
        Ok(())
    }
}

/// firstRow4Time.
#[derive(Clone, Debug)]
pub struct FirstRow4Time {
    pub base: BaseAggFunc,
}

impl AggFunc for FirstRow4Time {
    // go: firstRow4Time.AllocPartialResult L2 (memDelta dropped, A1)
    fn alloc_partial_result(&self) -> PartialResult {
        Box::new(PartialResult4FirstRow::<Time> {
            is_null: false,
            got_first_row: false,
            val: Time::default(),
        })
    }

    // go: firstRow4Time.ResetPartialResult L1
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4FirstRow<Time>>(pr);
        p.is_null = false;
        p.got_first_row = false;
    }

    // go: firstRow4Time.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4FirstRow<Time>>(pr);
        if p.got_first_row {
            return Ok(());
        }
        if let Some(&row) = rows_in_group.first() {
            let input = self.base.args[0].eval_time(sctx, st, row)?;
            p.got_first_row = true;
            p.is_null = input.is_none();
            // Go stores the zero value of a NULL; the value is never read then.
            p.val = input.unwrap_or_default();
        }
        Ok(())
    }

    /// Go copies `src` into `dst` (`*p2 = *p1`); Rust clones it.
    // go: firstRow4Time.MergePartialResult L2 (memDelta dropped, A1)
    fn merge_partial_result(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        src: &mut PartialResult,
        dst: &mut PartialResult,
    ) -> Result<(), Error> {
        let p1 = partial_result_mut::<PartialResult4FirstRow<Time>>(src);
        let p2 = partial_result_mut::<PartialResult4FirstRow<Time>>(dst);
        if !p2.got_first_row {
            *p2 = PartialResult4FirstRow {
                is_null: p1.is_null,
                got_first_row: p1.got_first_row,
                val: p1.val,
            };
        }
        Ok(())
    }

    // go: firstRow4Time.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4FirstRow<Time>>(pr);
        let col = self.base.col();
        if p.is_null || !p.got_first_row {
            chk.append_null(col);
            return Ok(());
        }
        chk.append_time(col, p.val);
        Ok(())
    }
}

/// firstRow4Duration.
#[derive(Clone, Debug)]
pub struct FirstRow4Duration {
    pub base: BaseAggFunc,
}

impl AggFunc for FirstRow4Duration {
    // go: firstRow4Duration.AllocPartialResult L2 (memDelta dropped, A1)
    fn alloc_partial_result(&self) -> PartialResult {
        Box::new(PartialResult4FirstRow::<Duration> {
            is_null: false,
            got_first_row: false,
            val: Duration::default(),
        })
    }

    // go: firstRow4Duration.ResetPartialResult L1
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4FirstRow<Duration>>(pr);
        p.is_null = false;
        p.got_first_row = false;
    }

    // go: firstRow4Duration.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4FirstRow<Duration>>(pr);
        if p.got_first_row {
            return Ok(());
        }
        if let Some(&row) = rows_in_group.first() {
            let input = self.base.args[0].eval_duration(sctx, st, row)?;
            p.got_first_row = true;
            p.is_null = input.is_none();
            // Go stores the zero value of a NULL; the value is never read then.
            p.val = input.unwrap_or_default();
        }
        Ok(())
    }

    /// Go copies `src` into `dst` (`*p2 = *p1`); Rust clones it.
    // go: firstRow4Duration.MergePartialResult L2 (memDelta dropped, A1)
    fn merge_partial_result(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        src: &mut PartialResult,
        dst: &mut PartialResult,
    ) -> Result<(), Error> {
        let p1 = partial_result_mut::<PartialResult4FirstRow<Duration>>(src);
        let p2 = partial_result_mut::<PartialResult4FirstRow<Duration>>(dst);
        if !p2.got_first_row {
            *p2 = PartialResult4FirstRow {
                is_null: p1.is_null,
                got_first_row: p1.got_first_row,
                val: p1.val,
            };
        }
        Ok(())
    }

    // go: firstRow4Duration.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4FirstRow<Duration>>(pr);
        let col = self.base.col();
        if p.is_null || !p.got_first_row {
            chk.append_null(col);
            return Ok(());
        }
        chk.append_duration(col, p.val);
        Ok(())
    }
}

/// firstRow4JSON.
#[derive(Clone, Debug)]
pub struct FirstRow4JSON {
    pub base: BaseAggFunc,
}

impl AggFunc for FirstRow4JSON {
    // go: firstRow4JSON.AllocPartialResult L2 (memDelta dropped, A1)
    fn alloc_partial_result(&self) -> PartialResult {
        Box::new(PartialResult4FirstRow::<BinaryJSON<'static>> {
            is_null: false,
            got_first_row: false,
            val: BinaryJSON::default(),
        })
    }

    // go: firstRow4JSON.ResetPartialResult L1
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4FirstRow<BinaryJSON<'static>>>(pr);
        p.is_null = false;
        p.got_first_row = false;
    }

    // go: firstRow4JSON.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4FirstRow<BinaryJSON<'static>>>(pr);
        if p.got_first_row {
            return Ok(());
        }
        if let Some(&row) = rows_in_group.first() {
            let input = self.base.args[0].eval_json(sctx, st, row)?;
            p.got_first_row = true;
            p.is_null = input.is_none();
            // Go stores the zero value of a NULL; the value is never read then.
            p.val = input.map(|v| v.copy()).unwrap_or_default();
        }
        Ok(())
    }

    /// Go copies `src` into `dst` (`*p2 = *p1`); Rust clones it.
    // go: firstRow4JSON.MergePartialResult L2 (memDelta dropped, A1)
    fn merge_partial_result(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        src: &mut PartialResult,
        dst: &mut PartialResult,
    ) -> Result<(), Error> {
        let p1 = partial_result_mut::<PartialResult4FirstRow<BinaryJSON<'static>>>(src);
        let p2 = partial_result_mut::<PartialResult4FirstRow<BinaryJSON<'static>>>(dst);
        if !p2.got_first_row {
            *p2 = PartialResult4FirstRow {
                is_null: p1.is_null,
                got_first_row: p1.got_first_row,
                val: p1.val.clone(),
            };
        }
        Ok(())
    }

    // go: firstRow4JSON.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4FirstRow<BinaryJSON<'static>>>(pr);
        let col = self.base.col();
        if p.is_null || !p.got_first_row {
            chk.append_null(col);
            return Ok(());
        }
        chk.append_json(col, &p.val);
        Ok(())
    }
}

/// firstRow4VectorFloat32.
#[derive(Clone, Debug)]
pub struct FirstRow4VectorFloat32 {
    pub base: BaseAggFunc,
}

impl AggFunc for FirstRow4VectorFloat32 {
    // go: firstRow4VectorFloat32.AllocPartialResult L2 (memDelta dropped, A1)
    fn alloc_partial_result(&self) -> PartialResult {
        Box::new(PartialResult4FirstRow::<VectorFloat32<'static>> {
            is_null: false,
            got_first_row: false,
            val: ZeroVectorFloat32.clone(),
        })
    }

    // go: firstRow4VectorFloat32.ResetPartialResult L1
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4FirstRow<VectorFloat32<'static>>>(pr);
        p.is_null = false;
        p.got_first_row = false;
    }

    // go: firstRow4VectorFloat32.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4FirstRow<VectorFloat32<'static>>>(pr);
        if p.got_first_row {
            return Ok(());
        }
        if let Some(&row) = rows_in_group.first() {
            let input = self.base.args[0].eval_vector_float32(sctx, st, row)?;
            p.got_first_row = true;
            p.is_null = input.is_none();
            // Go clones the zero vector of a NULL; the value is never read then.
            p.val = match input {
                Some(v) => v.clone(),
                None => ZeroVectorFloat32.clone(),
            };
        }
        Ok(())
    }

    /// Go copies `src` into `dst` (`*p2 = *p1`); Rust clones it.
    // go: firstRow4VectorFloat32.MergePartialResult L2 (memDelta dropped, A1)
    fn merge_partial_result(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        src: &mut PartialResult,
        dst: &mut PartialResult,
    ) -> Result<(), Error> {
        let p1 = partial_result_mut::<PartialResult4FirstRow<VectorFloat32<'static>>>(src);
        let p2 = partial_result_mut::<PartialResult4FirstRow<VectorFloat32<'static>>>(dst);
        if !p2.got_first_row {
            *p2 = PartialResult4FirstRow {
                is_null: p1.is_null,
                got_first_row: p1.got_first_row,
                val: p1.val.clone(),
            };
        }
        Ok(())
    }

    // go: firstRow4VectorFloat32.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4FirstRow<VectorFloat32<'static>>>(pr);
        let col = self.base.col();
        if p.is_null || !p.got_first_row {
            chk.append_null(col);
            return Ok(());
        }
        chk.append_vector_float32(col, &p.val);
        Ok(())
    }
}

/// firstRow4Decimal.
#[derive(Clone, Debug)]
pub struct FirstRow4Decimal {
    pub base: BaseAggFunc,
}

impl AggFunc for FirstRow4Decimal {
    // go: firstRow4Decimal.AllocPartialResult L2 (memDelta dropped, A1)
    fn alloc_partial_result(&self) -> PartialResult {
        Box::new(PartialResult4FirstRow::<MyDecimal> {
            is_null: false,
            got_first_row: false,
            val: MyDecimal::default(),
        })
    }

    // go: firstRow4Decimal.ResetPartialResult L1
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4FirstRow<MyDecimal>>(pr);
        p.is_null = false;
        p.got_first_row = false;
    }

    // go: firstRow4Decimal.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4FirstRow<MyDecimal>>(pr);
        if p.got_first_row {
            return Ok(());
        }
        if let Some(&row) = rows_in_group.first() {
            let input = self.base.args[0].eval_decimal(sctx, st, row)?;
            p.got_first_row = true;
            p.is_null = input.is_none();
            if let Some(v) = input {
                p.val = v;
            }
        }
        Ok(())
    }

    /// Go copies `src` into `dst` (`*p2 = *p1`); Rust clones it.
    // go: firstRow4Decimal.MergePartialResult L2 (memDelta dropped, A1)
    fn merge_partial_result(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        src: &mut PartialResult,
        dst: &mut PartialResult,
    ) -> Result<(), Error> {
        let p1 = partial_result_mut::<PartialResult4FirstRow<MyDecimal>>(src);
        let p2 = partial_result_mut::<PartialResult4FirstRow<MyDecimal>>(dst);
        if !p2.got_first_row {
            *p2 = PartialResult4FirstRow {
                is_null: p1.is_null,
                got_first_row: p1.got_first_row,
                val: p1.val,
            };
        }
        Ok(())
    }

    // go: firstRow4Decimal.AppendFinalResult2Chunk L1 [M-ALIAS]
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4FirstRow<MyDecimal>>(pr);
        if p.is_null || !p.got_first_row {
            chk.append_null(self.base.col());
            return Ok(());
        }
        round_to_ret_tp(&self.base, "first_row", &mut p.val)?;
        chk.append_my_decimal(self.base.col(), &p.val);
        Ok(())
    }
}

/// firstRow4Enum.
#[derive(Clone, Debug)]
pub struct FirstRow4Enum {
    pub base: BaseAggFunc,
}

impl AggFunc for FirstRow4Enum {
    // go: firstRow4Enum.AllocPartialResult L2 (memDelta dropped, A1)
    fn alloc_partial_result(&self) -> PartialResult {
        Box::new(PartialResult4FirstRow::<Enum> {
            is_null: false,
            got_first_row: false,
            val: Enum::default(),
        })
    }

    // go: firstRow4Enum.ResetPartialResult L1
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4FirstRow<Enum>>(pr);
        p.is_null = false;
        p.got_first_row = false;
    }

    // go: firstRow4Enum.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        ctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4FirstRow<Enum>>(pr);
        if p.got_first_row {
            return Ok(());
        }
        if let Some(&row) = rows_in_group.first() {
            let d = self.base.args[0].eval(ctx, st, row)?;
            p.got_first_row = true;
            p.is_null = d.is_null();
            p.val = d.get_mysql_enum().copy();
        }
        Ok(())
    }

    /// Go copies `src` into `dst` (`*p2 = *p1`); Rust clones it.
    // go: firstRow4Enum.MergePartialResult L2 (memDelta dropped, A1)
    fn merge_partial_result(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        src: &mut PartialResult,
        dst: &mut PartialResult,
    ) -> Result<(), Error> {
        let p1 = partial_result_mut::<PartialResult4FirstRow<Enum>>(src);
        let p2 = partial_result_mut::<PartialResult4FirstRow<Enum>>(dst);
        if !p2.got_first_row {
            *p2 = PartialResult4FirstRow {
                is_null: p1.is_null,
                got_first_row: p1.got_first_row,
                val: p1.val.clone(),
            };
        }
        Ok(())
    }

    // go: firstRow4Enum.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4FirstRow<Enum>>(pr);
        let col = self.base.col();
        if p.is_null || !p.got_first_row {
            chk.append_null(col);
            return Ok(());
        }
        chk.append_enum(col, &p.val);
        Ok(())
    }
}

/// firstRow4Set.
#[derive(Clone, Debug)]
pub struct FirstRow4Set {
    pub base: BaseAggFunc,
}

impl AggFunc for FirstRow4Set {
    // go: firstRow4Set.AllocPartialResult L2 (memDelta dropped, A1)
    fn alloc_partial_result(&self) -> PartialResult {
        Box::new(PartialResult4FirstRow::<Set> {
            is_null: false,
            got_first_row: false,
            val: Set::default(),
        })
    }

    // go: firstRow4Set.ResetPartialResult L1
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4FirstRow<Set>>(pr);
        p.is_null = false;
        p.got_first_row = false;
    }

    // go: firstRow4Set.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        ctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4FirstRow<Set>>(pr);
        if p.got_first_row {
            return Ok(());
        }
        if let Some(&row) = rows_in_group.first() {
            let d = self.base.args[0].eval(ctx, st, row)?;
            p.got_first_row = true;
            p.is_null = d.is_null();
            p.val = d.get_mysql_set().copy();
        }
        Ok(())
    }

    /// Go copies `src` into `dst` (`*p2 = *p1`); Rust clones it.
    // go: firstRow4Set.MergePartialResult L2 (memDelta dropped, A1)
    fn merge_partial_result(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        src: &mut PartialResult,
        dst: &mut PartialResult,
    ) -> Result<(), Error> {
        let p1 = partial_result_mut::<PartialResult4FirstRow<Set>>(src);
        let p2 = partial_result_mut::<PartialResult4FirstRow<Set>>(dst);
        if !p2.got_first_row {
            *p2 = PartialResult4FirstRow {
                is_null: p1.is_null,
                got_first_row: p1.got_first_row,
                val: p1.val.clone(),
            };
        }
        Ok(())
    }

    // go: firstRow4Set.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4FirstRow<Set>>(pr);
        let col = self.base.col();
        if p.is_null || !p.got_first_row {
            chk.append_null(col);
            return Ok(());
        }
        chk.append_set(col, &p.val);
        Ok(())
    }
}
