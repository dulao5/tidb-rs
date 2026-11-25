//! go-file: pkg/executor/aggfuncs/func_max_min.go
//!
//! MAX and MIN without a sliding window (part A of D12.4). One implementation per result
//! type, as in Go; ENUM and SET are evaluated as datums and compared by their names with
//! the collator of the result type. The partial results are one generic struct (Go has one
//! struct per type); their `deque` field, the sliding-window implementations, `Slide` and
//! `SetWindowStart` come with the window functions (part W).

use tidb_base::Error;
use tidb_chunk::chunk::Chunk;
use tidb_chunk::row::Row;
use tidb_expression::expression::EvalState;
use tidb_parser::types::FieldType;
use tidb_types::MyDecimal;
use tidb_types::collate::Collator;
use tidb_types::compare::compare_string;
use tidb_types::enum_type::Enum;
use tidb_types::json::{BinaryJSON, compare_binary_json};
use tidb_types::set_type::Set;
use tidb_types::stringutil;
use tidb_types::time::{Duration, Time};
use tidb_types::vector::{VectorFloat32, ZeroVectorFloat32};

use std::sync::Arc;

use super::aggfuncs::{
    AggFunc, AggFuncUpdateContext, BaseAggFunc, PartialResult, partial_result_mut,
};
use super::func_sum::round_to_ret_tp;

/// `partialResult4MaxMinXxx` without the sliding window's `deque` (part W).
#[derive(Debug)]
struct PartialResult4MaxMin<T> {
    val: T,
    // isNull is used to indicates:
    // 1. whether the partial result is the initialization value which should not be compared during evaluation;
    // 2. whether all the values of arg are all null, if so, we should return null as the default value for MAX/MIN.
    is_null: bool,
}

/// baseMaxMinAggFunc, embedded in the MAX / MIN implementations.
#[derive(Clone)]
pub struct BaseMaxMinAggFunc {
    pub base: BaseAggFunc,
    pub is_max: bool,
    pub collator: &'static dyn Collator,
}

/// maxMin4Int.
#[derive(Clone)]
pub struct MaxMin4Int {
    pub base: BaseMaxMinAggFunc,
}

impl AggFunc for MaxMin4Int {
    // go: maxMin4Int.AllocPartialResult L2 (memDelta dropped, A1)
    fn alloc_partial_result(&self) -> PartialResult {
        Box::new(PartialResult4MaxMin::<i64> {
            val: 0,
            is_null: true,
        })
    }

    // go: maxMin4Int.ResetPartialResult L1
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4MaxMin<i64>>(pr);
        p.val = 0;
        p.is_null = true;
    }

    // go: maxMin4Int.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMin<i64>>(pr);
        let col = self.base.base.col();
        if p.is_null {
            chk.append_null(col);
            return Ok(());
        }
        chk.append_int64(col, p.val);
        Ok(())
    }

    // go: maxMin4Int.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMin<i64>>(pr);
        for &row in rows_in_group {
            let Some(input) = self.base.base.args[0].eval_int(sctx, st, row)? else {
                continue;
            };
            if p.is_null {
                p.val = input;
                p.is_null = false;
                continue;
            }
            if self.base.is_max && input > p.val || !self.base.is_max && input < p.val {
                p.val = input;
            }
        }
        Ok(())
    }

    /// Go copies `src` into a NULL `dst` (`*p2 = *p1`) or its value; Rust clones it.
    // go: maxMin4Int.MergePartialResult L2 (memDelta dropped, A1)
    fn merge_partial_result(
        &self,
        _ctx: &dyn AggFuncUpdateContext,
        src: &mut PartialResult,
        dst: &mut PartialResult,
    ) -> Result<(), Error> {
        let p1 = partial_result_mut::<PartialResult4MaxMin<i64>>(src);
        let p2 = partial_result_mut::<PartialResult4MaxMin<i64>>(dst);
        if p1.is_null {
            return Ok(());
        }
        if p2.is_null {
            *p2 = PartialResult4MaxMin {
                val: p1.val,
                is_null: p1.is_null,
            };
            return Ok(());
        }
        if self.base.is_max && p1.val > p2.val || !self.base.is_max && p1.val < p2.val {
            p2.val = p1.val;
            p2.is_null = false;
        }
        Ok(())
    }
}

/// maxMin4Uint.
#[derive(Clone)]
pub struct MaxMin4Uint {
    pub base: BaseMaxMinAggFunc,
}

impl AggFunc for MaxMin4Uint {
    // go: maxMin4Uint.AllocPartialResult L2 (memDelta dropped, A1)
    fn alloc_partial_result(&self) -> PartialResult {
        Box::new(PartialResult4MaxMin::<u64> {
            val: 0,
            is_null: true,
        })
    }

    // go: maxMin4Uint.ResetPartialResult L1
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4MaxMin<u64>>(pr);
        p.val = 0;
        p.is_null = true;
    }

    // go: maxMin4Uint.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMin<u64>>(pr);
        let col = self.base.base.col();
        if p.is_null {
            chk.append_null(col);
            return Ok(());
        }
        chk.append_uint64(col, p.val);
        Ok(())
    }

    // go: maxMin4Uint.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMin<u64>>(pr);
        for &row in rows_in_group {
            let Some(input) = self.base.base.args[0].eval_int(sctx, st, row)? else {
                continue;
            };
            let input = input as u64;
            if p.is_null {
                p.val = input;
                p.is_null = false;
                continue;
            }
            if self.base.is_max && input > p.val || !self.base.is_max && input < p.val {
                p.val = input;
            }
        }
        Ok(())
    }

    /// Go copies `src` into a NULL `dst` (`*p2 = *p1`) or its value; Rust clones it.
    // go: maxMin4Uint.MergePartialResult L2 (memDelta dropped, A1)
    fn merge_partial_result(
        &self,
        _ctx: &dyn AggFuncUpdateContext,
        src: &mut PartialResult,
        dst: &mut PartialResult,
    ) -> Result<(), Error> {
        let p1 = partial_result_mut::<PartialResult4MaxMin<u64>>(src);
        let p2 = partial_result_mut::<PartialResult4MaxMin<u64>>(dst);
        if p1.is_null {
            return Ok(());
        }
        if p2.is_null {
            *p2 = PartialResult4MaxMin {
                val: p1.val,
                is_null: p1.is_null,
            };
            return Ok(());
        }
        if self.base.is_max && p1.val > p2.val || !self.base.is_max && p1.val < p2.val {
            p2.val = p1.val;
            p2.is_null = false;
        }
        Ok(())
    }
}

/// maxMin4Float32.
#[derive(Clone)]
pub struct MaxMin4Float32 {
    pub base: BaseMaxMinAggFunc,
}

impl AggFunc for MaxMin4Float32 {
    // go: maxMin4Float32.AllocPartialResult L2 (memDelta dropped, A1)
    fn alloc_partial_result(&self) -> PartialResult {
        Box::new(PartialResult4MaxMin::<f32> {
            val: 0.0,
            is_null: true,
        })
    }

    // go: maxMin4Float32.ResetPartialResult L1
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4MaxMin<f32>>(pr);
        p.val = 0.0;
        p.is_null = true;
    }

    // go: maxMin4Float32.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMin<f32>>(pr);
        let col = self.base.base.col();
        if p.is_null {
            chk.append_null(col);
            return Ok(());
        }
        chk.append_float32(col, p.val);
        Ok(())
    }

    // go: maxMin4Float32.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMin<f32>>(pr);
        for &row in rows_in_group {
            let Some(input) = self.base.base.args[0].eval_real(sctx, st, row)? else {
                continue;
            };
            let input = input as f32;
            if p.is_null {
                p.val = input;
                p.is_null = false;
                continue;
            }
            if self.base.is_max && input > p.val || !self.base.is_max && input < p.val {
                p.val = input;
            }
        }
        Ok(())
    }

    /// Go copies `src` into a NULL `dst` (`*p2 = *p1`) or its value; Rust clones it.
    // go: maxMin4Float32.MergePartialResult L2 (memDelta dropped, A1)
    fn merge_partial_result(
        &self,
        _ctx: &dyn AggFuncUpdateContext,
        src: &mut PartialResult,
        dst: &mut PartialResult,
    ) -> Result<(), Error> {
        let p1 = partial_result_mut::<PartialResult4MaxMin<f32>>(src);
        let p2 = partial_result_mut::<PartialResult4MaxMin<f32>>(dst);
        if p1.is_null {
            return Ok(());
        }
        if p2.is_null {
            *p2 = PartialResult4MaxMin {
                val: p1.val,
                is_null: p1.is_null,
            };
            return Ok(());
        }
        if self.base.is_max && p1.val > p2.val || !self.base.is_max && p1.val < p2.val {
            p2.val = p1.val;
            p2.is_null = false;
        }
        Ok(())
    }
}

/// maxMin4Float64.
#[derive(Clone)]
pub struct MaxMin4Float64 {
    pub base: BaseMaxMinAggFunc,
}

impl AggFunc for MaxMin4Float64 {
    // go: maxMin4Float64.AllocPartialResult L2 (memDelta dropped, A1)
    fn alloc_partial_result(&self) -> PartialResult {
        Box::new(PartialResult4MaxMin::<f64> {
            val: 0.0,
            is_null: true,
        })
    }

    // go: maxMin4Float64.ResetPartialResult L1
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4MaxMin<f64>>(pr);
        p.val = 0.0;
        p.is_null = true;
    }

    // go: maxMin4Float64.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMin<f64>>(pr);
        let col = self.base.base.col();
        if p.is_null {
            chk.append_null(col);
            return Ok(());
        }
        chk.append_float64(col, p.val);
        Ok(())
    }

    // go: maxMin4Float64.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMin<f64>>(pr);
        for &row in rows_in_group {
            let Some(input) = self.base.base.args[0].eval_real(sctx, st, row)? else {
                continue;
            };
            if p.is_null {
                p.val = input;
                p.is_null = false;
                continue;
            }
            if self.base.is_max && input > p.val || !self.base.is_max && input < p.val {
                p.val = input;
            }
        }
        Ok(())
    }

    /// Go copies `src` into a NULL `dst` (`*p2 = *p1`) or its value; Rust clones it.
    // go: maxMin4Float64.MergePartialResult L2 (memDelta dropped, A1)
    fn merge_partial_result(
        &self,
        _ctx: &dyn AggFuncUpdateContext,
        src: &mut PartialResult,
        dst: &mut PartialResult,
    ) -> Result<(), Error> {
        let p1 = partial_result_mut::<PartialResult4MaxMin<f64>>(src);
        let p2 = partial_result_mut::<PartialResult4MaxMin<f64>>(dst);
        if p1.is_null {
            return Ok(());
        }
        if p2.is_null {
            *p2 = PartialResult4MaxMin {
                val: p1.val,
                is_null: p1.is_null,
            };
            return Ok(());
        }
        if self.base.is_max && p1.val > p2.val || !self.base.is_max && p1.val < p2.val {
            p2.val = p1.val;
            p2.is_null = false;
        }
        Ok(())
    }
}

/// maxMin4Decimal.
#[derive(Clone)]
pub struct MaxMin4Decimal {
    pub base: BaseMaxMinAggFunc,
}

impl AggFunc for MaxMin4Decimal {
    // go: maxMin4Decimal.AllocPartialResult L2 (memDelta dropped, A1)
    fn alloc_partial_result(&self) -> PartialResult {
        Box::new(PartialResult4MaxMin::<MyDecimal> {
            val: MyDecimal::default(),
            is_null: true,
        })
    }

    // go: maxMin4Decimal.ResetPartialResult L1
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4MaxMin<MyDecimal>>(pr);
        p.is_null = true;
    }

    // go: maxMin4Decimal.AppendFinalResult2Chunk L1 [M-ALIAS]
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMin<MyDecimal>>(pr);
        if p.is_null {
            chk.append_null(self.base.base.col());
            return Ok(());
        }
        round_to_ret_tp(&self.base.base, "max or min", &mut p.val)?;
        chk.append_my_decimal(self.base.base.col(), &p.val);
        Ok(())
    }

    // go: maxMin4Decimal.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMin<MyDecimal>>(pr);
        for &row in rows_in_group {
            let Some(input) = self.base.base.args[0].eval_decimal(sctx, st, row)? else {
                continue;
            };
            if p.is_null {
                p.val = input;
                p.is_null = false;
                continue;
            }
            let cmp = input.compare(&p.val);
            if self.base.is_max && cmp > 0 || !self.base.is_max && cmp < 0 {
                p.val = input;
            }
        }
        Ok(())
    }

    /// Go copies `src` into a NULL `dst` (`*p2 = *p1`) or its value; Rust clones it.
    // go: maxMin4Decimal.MergePartialResult L2 (memDelta dropped, A1)
    fn merge_partial_result(
        &self,
        _ctx: &dyn AggFuncUpdateContext,
        src: &mut PartialResult,
        dst: &mut PartialResult,
    ) -> Result<(), Error> {
        let p1 = partial_result_mut::<PartialResult4MaxMin<MyDecimal>>(src);
        let p2 = partial_result_mut::<PartialResult4MaxMin<MyDecimal>>(dst);
        if p1.is_null {
            return Ok(());
        }
        if p2.is_null {
            *p2 = PartialResult4MaxMin {
                val: p1.val,
                is_null: p1.is_null,
            };
            return Ok(());
        }
        let cmp = p1.val.compare(&p2.val);
        if self.base.is_max && cmp > 0 || !self.base.is_max && cmp < 0 {
            p2.val = p1.val;
            p2.is_null = false;
        }
        Ok(())
    }
}

/// maxMin4String.
#[derive(Clone)]
pub struct MaxMin4String {
    pub base: BaseMaxMinAggFunc,
    pub ret_tp: Option<Arc<FieldType>>,
}

impl AggFunc for MaxMin4String {
    // go: maxMin4String.AllocPartialResult L2 (memDelta dropped, A1)
    fn alloc_partial_result(&self) -> PartialResult {
        Box::new(PartialResult4MaxMin::<Vec<u8>> {
            val: Vec::new(),
            is_null: true,
        })
    }

    // go: maxMin4String.ResetPartialResult L1
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4MaxMin<Vec<u8>>>(pr);
        p.is_null = true;
    }

    // go: maxMin4String.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMin<Vec<u8>>>(pr);
        let col = self.base.base.col();
        if p.is_null {
            chk.append_null(col);
            return Ok(());
        }
        chk.append_string(col, &p.val);
        Ok(())
    }

    // go: maxMin4String.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMin<Vec<u8>>>(pr);
        let tp = self.base.base.args[0].get_type(sctx);
        for &row in rows_in_group {
            let Some(input) = self.base.base.args[0].eval_string(sctx, st, row)? else {
                continue;
            };
            if p.is_null {
                p.val = stringutil::copy(&input);
                p.is_null = false;
                continue;
            }
            let cmp = compare_string(&input, &p.val, tp.get_collate());
            if self.base.is_max && cmp == 1 || !self.base.is_max && cmp == -1 {
                p.val = stringutil::copy(&input);
            }
        }
        Ok(())
    }

    /// Go copies `src` into a NULL `dst` (`*p2 = *p1`) or its value; Rust clones it.
    // go: maxMin4String.MergePartialResult L2 (memDelta dropped, A1)
    fn merge_partial_result(
        &self,
        ctx: &dyn AggFuncUpdateContext,
        src: &mut PartialResult,
        dst: &mut PartialResult,
    ) -> Result<(), Error> {
        let p1 = partial_result_mut::<PartialResult4MaxMin<Vec<u8>>>(src);
        let p2 = partial_result_mut::<PartialResult4MaxMin<Vec<u8>>>(dst);
        if p1.is_null {
            return Ok(());
        }
        if p2.is_null {
            *p2 = PartialResult4MaxMin {
                val: p1.val.clone(),
                is_null: p1.is_null,
            };
            return Ok(());
        }
        let tp = self.base.base.args[0].get_type(ctx);
        let cmp = compare_string(&p1.val, &p2.val, tp.get_collate());
        if self.base.is_max && cmp > 0 || !self.base.is_max && cmp < 0 {
            p2.val = p1.val.clone();
            p2.is_null = false;
        }
        Ok(())
    }
}

/// maxMin4Time.
#[derive(Clone)]
pub struct MaxMin4Time {
    pub base: BaseMaxMinAggFunc,
}

impl AggFunc for MaxMin4Time {
    // go: maxMin4Time.AllocPartialResult L2 (memDelta dropped, A1)
    fn alloc_partial_result(&self) -> PartialResult {
        Box::new(PartialResult4MaxMin::<Time> {
            val: Time::default(),
            is_null: true,
        })
    }

    // go: maxMin4Time.ResetPartialResult L1
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4MaxMin<Time>>(pr);
        p.is_null = true;
    }

    // go: maxMin4Time.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMin<Time>>(pr);
        let col = self.base.base.col();
        if p.is_null {
            chk.append_null(col);
            return Ok(());
        }
        chk.append_time(col, p.val);
        Ok(())
    }

    // go: maxMin4Time.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMin<Time>>(pr);
        for &row in rows_in_group {
            let Some(input) = self.base.base.args[0].eval_time(sctx, st, row)? else {
                continue;
            };
            if p.is_null {
                p.val = input;
                p.is_null = false;
                continue;
            }
            let cmp = input.compare(&p.val);
            if self.base.is_max && cmp == 1 || !self.base.is_max && cmp == -1 {
                p.val = input;
            }
        }
        Ok(())
    }

    /// Go copies `src` into a NULL `dst` (`*p2 = *p1`) or its value; Rust clones it.
    // go: maxMin4Time.MergePartialResult L2 (memDelta dropped, A1)
    fn merge_partial_result(
        &self,
        _ctx: &dyn AggFuncUpdateContext,
        src: &mut PartialResult,
        dst: &mut PartialResult,
    ) -> Result<(), Error> {
        let p1 = partial_result_mut::<PartialResult4MaxMin<Time>>(src);
        let p2 = partial_result_mut::<PartialResult4MaxMin<Time>>(dst);
        if p1.is_null {
            return Ok(());
        }
        if p2.is_null {
            *p2 = PartialResult4MaxMin {
                val: p1.val,
                is_null: p1.is_null,
            };
            return Ok(());
        }
        let cmp = p1.val.compare(&p2.val);
        if self.base.is_max && cmp == 1 || !self.base.is_max && cmp == -1 {
            p2.val = p1.val;
            p2.is_null = false;
        }
        Ok(())
    }
}

/// maxMin4Duration.
#[derive(Clone)]
pub struct MaxMin4Duration {
    pub base: BaseMaxMinAggFunc,
}

impl AggFunc for MaxMin4Duration {
    // go: maxMin4Duration.AllocPartialResult L2 (memDelta dropped, A1)
    fn alloc_partial_result(&self) -> PartialResult {
        Box::new(PartialResult4MaxMin::<Duration> {
            val: Duration::default(),
            is_null: true,
        })
    }

    // go: maxMin4Duration.ResetPartialResult L1
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4MaxMin<Duration>>(pr);
        p.is_null = true;
    }

    // go: maxMin4Duration.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMin<Duration>>(pr);
        let col = self.base.base.col();
        if p.is_null {
            chk.append_null(col);
            return Ok(());
        }
        chk.append_duration(col, p.val);
        Ok(())
    }

    // go: maxMin4Duration.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMin<Duration>>(pr);
        for &row in rows_in_group {
            let Some(input) = self.base.base.args[0].eval_duration(sctx, st, row)? else {
                continue;
            };
            if p.is_null {
                p.val = input;
                p.is_null = false;
                continue;
            }
            let cmp = input.compare(p.val);
            if self.base.is_max && cmp == 1 || !self.base.is_max && cmp == -1 {
                p.val = input;
            }
        }
        Ok(())
    }

    /// Go copies `src` into a NULL `dst` (`*p2 = *p1`) or its value; Rust clones it.
    // go: maxMin4Duration.MergePartialResult L2 (memDelta dropped, A1)
    fn merge_partial_result(
        &self,
        _ctx: &dyn AggFuncUpdateContext,
        src: &mut PartialResult,
        dst: &mut PartialResult,
    ) -> Result<(), Error> {
        let p1 = partial_result_mut::<PartialResult4MaxMin<Duration>>(src);
        let p2 = partial_result_mut::<PartialResult4MaxMin<Duration>>(dst);
        if p1.is_null {
            return Ok(());
        }
        if p2.is_null {
            *p2 = PartialResult4MaxMin {
                val: p1.val,
                is_null: p1.is_null,
            };
            return Ok(());
        }
        let cmp = p1.val.compare(p2.val);
        if self.base.is_max && cmp == 1 || !self.base.is_max && cmp == -1 {
            p2.val = p1.val;
            p2.is_null = false;
        }
        Ok(())
    }
}

/// maxMin4JSON.
#[derive(Clone)]
pub struct MaxMin4JSON {
    pub base: BaseMaxMinAggFunc,
}

impl AggFunc for MaxMin4JSON {
    // go: maxMin4JSON.AllocPartialResult L2 (memDelta dropped, A1)
    fn alloc_partial_result(&self) -> PartialResult {
        Box::new(PartialResult4MaxMin::<BinaryJSON<'static>> {
            val: BinaryJSON::default(),
            is_null: true,
        })
    }

    // go: maxMin4JSON.ResetPartialResult L1
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4MaxMin<BinaryJSON<'static>>>(pr);
        p.is_null = true;
    }

    // go: maxMin4JSON.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMin<BinaryJSON<'static>>>(pr);
        let col = self.base.base.col();
        if p.is_null {
            chk.append_null(col);
            return Ok(());
        }
        chk.append_json(col, &p.val);
        Ok(())
    }

    // go: maxMin4JSON.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMin<BinaryJSON<'static>>>(pr);
        for &row in rows_in_group {
            let Some(input) = self.base.base.args[0].eval_json(sctx, st, row)? else {
                continue;
            };
            if p.is_null {
                p.val = input.copy();
                p.is_null = false;
                continue;
            }
            let cmp = compare_binary_json(input.as_ref(), p.val.as_ref());
            if self.base.is_max && cmp > 0 || !self.base.is_max && cmp < 0 {
                p.val = input.copy();
            }
        }
        Ok(())
    }

    /// Go copies `src` into a NULL `dst` (`*p2 = *p1`) or its value; Rust clones it.
    // go: maxMin4JSON.MergePartialResult L2 (memDelta dropped, A1)
    fn merge_partial_result(
        &self,
        _ctx: &dyn AggFuncUpdateContext,
        src: &mut PartialResult,
        dst: &mut PartialResult,
    ) -> Result<(), Error> {
        let p1 = partial_result_mut::<PartialResult4MaxMin<BinaryJSON<'static>>>(src);
        let p2 = partial_result_mut::<PartialResult4MaxMin<BinaryJSON<'static>>>(dst);
        if p1.is_null {
            return Ok(());
        }
        if p2.is_null {
            *p2 = PartialResult4MaxMin {
                val: p1.val.clone(),
                is_null: p1.is_null,
            };
            return Ok(());
        }
        let cmp = compare_binary_json(p1.val.as_ref(), p2.val.as_ref());
        if self.base.is_max && cmp > 0 || !self.base.is_max && cmp < 0 {
            p2.val = p1.val.clone();
            p2.is_null = false;
        }
        Ok(())
    }
}

/// maxMin4VectorFloat32.
#[derive(Clone)]
pub struct MaxMin4VectorFloat32 {
    pub base: BaseMaxMinAggFunc,
}

impl AggFunc for MaxMin4VectorFloat32 {
    // go: maxMin4VectorFloat32.AllocPartialResult L2 (memDelta dropped, A1)
    fn alloc_partial_result(&self) -> PartialResult {
        Box::new(PartialResult4MaxMin::<VectorFloat32<'static>> {
            val: ZeroVectorFloat32.clone(),
            is_null: true,
        })
    }

    // go: maxMin4VectorFloat32.ResetPartialResult L1
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4MaxMin<VectorFloat32<'static>>>(pr);
        p.is_null = true;
    }

    // go: maxMin4VectorFloat32.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMin<VectorFloat32<'static>>>(pr);
        let col = self.base.base.col();
        if p.is_null {
            chk.append_null(col);
            return Ok(());
        }
        chk.append_vector_float32(col, &p.val);
        Ok(())
    }

    // go: maxMin4VectorFloat32.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMin<VectorFloat32<'static>>>(pr);
        for &row in rows_in_group {
            let Some(input) = self.base.base.args[0].eval_vector_float32(sctx, st, row)? else {
                continue;
            };
            if p.is_null {
                p.val = input.clone();
                p.is_null = false;
                continue;
            }
            let cmp = input.compare(&p.val);
            if self.base.is_max && cmp > 0 || !self.base.is_max && cmp < 0 {
                p.val = input.clone();
            }
        }
        Ok(())
    }

    /// Go copies `src` into a NULL `dst` (`*p2 = *p1`) or its value; Rust clones it.
    // go: maxMin4VectorFloat32.MergePartialResult L2 (memDelta dropped, A1)
    fn merge_partial_result(
        &self,
        _ctx: &dyn AggFuncUpdateContext,
        src: &mut PartialResult,
        dst: &mut PartialResult,
    ) -> Result<(), Error> {
        let p1 = partial_result_mut::<PartialResult4MaxMin<VectorFloat32<'static>>>(src);
        let p2 = partial_result_mut::<PartialResult4MaxMin<VectorFloat32<'static>>>(dst);
        if p1.is_null {
            return Ok(());
        }
        if p2.is_null {
            *p2 = PartialResult4MaxMin {
                val: p1.val.clone(),
                is_null: p1.is_null,
            };
            return Ok(());
        }
        let cmp = p1.val.compare(&p2.val);
        if self.base.is_max && cmp > 0 || !self.base.is_max && cmp < 0 {
            p2.val = p1.val.clone();
            p2.is_null = false;
        }
        Ok(())
    }
}

/// maxMin4Enum.
#[derive(Clone)]
pub struct MaxMin4Enum {
    pub base: BaseMaxMinAggFunc,
}

impl AggFunc for MaxMin4Enum {
    // go: maxMin4Enum.AllocPartialResult L2 (memDelta dropped, A1)
    fn alloc_partial_result(&self) -> PartialResult {
        Box::new(PartialResult4MaxMin::<Enum> {
            val: Enum::default(),
            is_null: true,
        })
    }

    // go: maxMin4Enum.ResetPartialResult L1
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4MaxMin<Enum>>(pr);
        p.is_null = true;
    }

    // go: maxMin4Enum.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMin<Enum>>(pr);
        let col = self.base.base.col();
        if p.is_null {
            chk.append_null(col);
            return Ok(());
        }
        chk.append_enum(col, &p.val);
        Ok(())
    }

    // go: maxMin4Enum.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        ctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMin<Enum>>(pr);
        for &row in rows_in_group {
            let d = self.base.base.args[0].eval(ctx, st, row)?;
            if d.is_null() {
                continue;
            }
            if p.is_null {
                p.val = d.get_mysql_enum().copy();
                p.is_null = false;
                continue;
            }
            let en = d.get_mysql_enum();
            if self.base.is_max && self.base.collator.compare(&en.name, &p.val.name) > 0
                || !self.base.is_max && self.base.collator.compare(&en.name, &p.val.name) < 0
            {
                p.val = en.copy();
            }
        }
        Ok(())
    }

    /// Go copies `src` into a NULL `dst` (`*p2 = *p1`) or its value; Rust clones it.
    // go: maxMin4Enum.MergePartialResult L2 (memDelta dropped, A1)
    fn merge_partial_result(
        &self,
        _ctx: &dyn AggFuncUpdateContext,
        src: &mut PartialResult,
        dst: &mut PartialResult,
    ) -> Result<(), Error> {
        let p1 = partial_result_mut::<PartialResult4MaxMin<Enum>>(src);
        let p2 = partial_result_mut::<PartialResult4MaxMin<Enum>>(dst);
        if p1.is_null {
            return Ok(());
        }
        if p2.is_null {
            *p2 = PartialResult4MaxMin {
                val: p1.val.clone(),
                is_null: p1.is_null,
            };
            return Ok(());
        }
        if self.base.is_max && self.base.collator.compare(&p1.val.name, &p2.val.name) > 0
            || !self.base.is_max && self.base.collator.compare(&p1.val.name, &p2.val.name) < 0
        {
            p2.val = p1.val.clone();
            p2.is_null = false;
        }
        Ok(())
    }
}

/// maxMin4Set.
#[derive(Clone)]
pub struct MaxMin4Set {
    pub base: BaseMaxMinAggFunc,
}

impl AggFunc for MaxMin4Set {
    // go: maxMin4Set.AllocPartialResult L2 (memDelta dropped, A1)
    fn alloc_partial_result(&self) -> PartialResult {
        Box::new(PartialResult4MaxMin::<Set> {
            val: Set::default(),
            is_null: true,
        })
    }

    // go: maxMin4Set.ResetPartialResult L1
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4MaxMin<Set>>(pr);
        p.is_null = true;
    }

    // go: maxMin4Set.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMin<Set>>(pr);
        let col = self.base.base.col();
        if p.is_null {
            chk.append_null(col);
            return Ok(());
        }
        chk.append_set(col, &p.val);
        Ok(())
    }

    // go: maxMin4Set.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        ctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMin<Set>>(pr);
        for &row in rows_in_group {
            let d = self.base.base.args[0].eval(ctx, st, row)?;
            if d.is_null() {
                continue;
            }
            if p.is_null {
                p.val = d.get_mysql_set().copy();
                p.is_null = false;
                continue;
            }
            let en = d.get_mysql_set();
            if self.base.is_max && self.base.collator.compare(&en.name, &p.val.name) > 0
                || !self.base.is_max && self.base.collator.compare(&en.name, &p.val.name) < 0
            {
                p.val = en.copy();
            }
        }
        Ok(())
    }

    /// Go copies `src` into a NULL `dst` (`*p2 = *p1`) or its value; Rust clones it.
    // go: maxMin4Set.MergePartialResult L2 (memDelta dropped, A1)
    fn merge_partial_result(
        &self,
        _ctx: &dyn AggFuncUpdateContext,
        src: &mut PartialResult,
        dst: &mut PartialResult,
    ) -> Result<(), Error> {
        let p1 = partial_result_mut::<PartialResult4MaxMin<Set>>(src);
        let p2 = partial_result_mut::<PartialResult4MaxMin<Set>>(dst);
        if p1.is_null {
            return Ok(());
        }
        if p2.is_null {
            *p2 = PartialResult4MaxMin {
                val: p1.val.clone(),
                is_null: p1.is_null,
            };
            return Ok(());
        }
        if self.base.is_max && self.base.collator.compare(&p1.val.name, &p2.val.name) > 0
            || !self.base.is_max && self.base.collator.compare(&p1.val.name, &p2.val.name) < 0
        {
            p2.val = p1.val.clone();
            p2.is_null = false;
        }
        Ok(())
    }
}
