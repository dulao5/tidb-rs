//! go-file: pkg/executor/aggfuncs/func_lead_lag.go
//!
//! `LEAD` and `LAG` window functions (part W of D12.4).
//!
//! Per spec §5.2 and §9.2 decision 6, the mutable [`ValueEvaluator`] lives inside
//! [`PartialResult4LeadLag`] instead of [`BaseLeadLag`] so `AggFunc` remains `Sync` and
//! `&self`. Stored rows use [`RowHandle`] (M-ROW) in a [`ColAlloc`]-backed vector (A1).
//!
//! Per ruling R26 (#405, constitution art. 9.3), [`Lead::append_final_result2_chunk`] uses
//! checked addition (`cur_idx.checked_add(offset)`) so a large `uint64` offset such as
//! `u64::MAX` does not wrap around and read an earlier row; [`Lag`] already uses a
//! comparison (`cur_idx >= offset`) and does not wrap.

use std::sync::Arc;

use allocator_api2::vec::Vec as AllocVec;
use tidb_base::Error;
use tidb_chunk::ColAlloc;
use tidb_chunk::chunk::Chunk;
use tidb_chunk::row::{Row, RowHandle};
use tidb_expression::expression::{EvalState, ExprRef};
use tidb_parser::types::FieldType;

use super::aggfuncs::{
    AggFunc, AggFuncUpdateContext, BaseAggFunc, PartialResult, partial_result_mut, push_rows_in,
    row_of,
};
use super::func_value::{ValueEvaluator, build_value_evaluator};

/// `baseLeadLag`, embedded in [`Lead`] and [`Lag`].
#[derive(Clone, Debug)]
pub struct BaseLeadLag {
    pub base: BaseAggFunc,
    pub ret_tp: Arc<FieldType>,
    pub default_expr: ExprRef,
    pub offset: u64,
    alloc: ColAlloc,
}

impl BaseLeadLag {
    // glue: Go struct literal baseLeadLag{...} with evaluator built per partial result (spec §9.2 #6)
    pub fn new(
        base: BaseAggFunc,
        ret_tp: Arc<FieldType>,
        default_expr: ExprRef,
        offset: u64,
    ) -> Self {
        Self {
            base,
            ret_tp,
            default_expr,
            offset,
            alloc: ColAlloc::Global,
        }
    }

    // go: baseLeadLag.AllocPartialResult L2 (memDelta dropped, A1; evaluator in partial result, spec §5.2)
    pub fn alloc_partial_result(&self) -> PartialResult {
        let evaluator = build_value_evaluator(&self.ret_tp, self.alloc.clone());
        Box::new(PartialResult4LeadLag {
            rows: AllocVec::new_in(self.alloc.clone()),
            cur_idx: 0,
            evaluator,
            eval_state: EvalState::new(),
            alloc: self.alloc.clone(),
        })
    }

    // go: baseLeadLag.ResetPartialResult L1
    pub fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4LeadLag>(pr);
        p.rows.clear();
        p.cur_idx = 0;
    }

    // go: baseLeadLag.UpdatePartialResult L2 (memDelta dropped, A1; M-ROW; captures EvalState for AppendFinal)
    pub fn update_partial_result(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4LeadLag>(pr);
        p.eval_state = st.clone();
        push_rows_in(&mut p.rows, rows_in_group, &p.alloc);
        Ok(())
    }

    // glue: M-ROW zero-copy RowHandle ingestion for WindowExec (spec §5.2)
    pub fn update_partial_result_handles(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[RowHandle],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4LeadLag>(pr);
        p.eval_state = st.clone();
        p.rows.extend(rows_in_group.iter().cloned().map(Some));
        Ok(())
    }

    // glue: A1 3.3 TrackedAlloc for stored row handles and evaluator payloads
    pub fn set_alloc(&mut self, alloc: ColAlloc) {
        self.alloc = alloc;
    }
}

struct PartialResult4LeadLag {
    rows: AllocVec<Option<RowHandle>, ColAlloc>,
    cur_idx: u64,
    evaluator: Option<Box<dyn ValueEvaluator>>,
    eval_state: EvalState,
    alloc: ColAlloc,
}

impl PartialResult4LeadLag {
    // glue: dereference optional evaluator with Go's nil-interface panic (M-PANIC)
    fn evaluator_mut(&mut self) -> &mut dyn ValueEvaluator {
        match &mut self.evaluator {
            Some(e) => &mut **e,
            None => panic!("runtime error: invalid memory address or nil pointer dereference"),
        }
    }
}

/// `lead`.
#[derive(Clone, Debug)]
pub struct Lead {
    pub base: BaseLeadLag,
}

impl AggFunc for Lead {
    // glue: baseLeadLag.AllocPartialResult through the embedded struct (M-SELF)
    fn alloc_partial_result(&self) -> PartialResult {
        self.base.alloc_partial_result()
    }

    // glue: baseLeadLag.ResetPartialResult through the embedded struct (M-SELF)
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        self.base.reset_partial_result(pr)
    }

    // glue: baseLeadLag.UpdatePartialResult through the embedded struct (M-SELF)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        self.base.update_partial_result(sctx, st, rows_in_group, pr)
    }

    // glue: baseLeadLag.update_partial_result_handles through the embedded struct (M-SELF)
    fn update_partial_result_handles(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[RowHandle],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        self.base
            .update_partial_result_handles(sctx, st, rows_in_group, pr)
    }

    // go: lead.AppendFinalResult2Chunk L2 (checked_add fixes uint64 wrap-around, #405 / R26; evaluator in partial result, spec §5.2)
    fn append_final_result2_chunk(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4LeadLag>(pr);
        let cur_idx = p.cur_idx;
        let num_rows = p.rows.len() as u64;
        if let Some(target) = cur_idx.checked_add(self.base.offset)
            && target < num_rows
        {
            let row = row_of(&p.rows[target as usize]);
            let ev = p
                .evaluator
                .as_deref_mut()
                .expect("runtime error: invalid memory address or nil pointer dereference");
            ev.evaluate_row(sctx, &p.eval_state, &self.base.base.args[0], row)?;
        } else {
            let row = row_of(&p.rows[cur_idx as usize]);
            let ev = p
                .evaluator
                .as_deref_mut()
                .expect("runtime error: invalid memory address or nil pointer dereference");
            ev.evaluate_row(sctx, &p.eval_state, &self.base.default_expr, row)?;
        }
        p.evaluator_mut().append_result(chk, self.base.base.col());
        p.cur_idx += 1;
        Ok(())
    }

    // glue: A1 3.3 TrackedAlloc forwarding
    fn set_alloc(&mut self, alloc: ColAlloc) {
        self.base.set_alloc(alloc);
    }
}

/// `lag`.
#[derive(Clone, Debug)]
pub struct Lag {
    pub base: BaseLeadLag,
}

impl AggFunc for Lag {
    // glue: baseLeadLag.AllocPartialResult through the embedded struct (M-SELF)
    fn alloc_partial_result(&self) -> PartialResult {
        self.base.alloc_partial_result()
    }

    // glue: baseLeadLag.ResetPartialResult through the embedded struct (M-SELF)
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        self.base.reset_partial_result(pr)
    }

    // glue: baseLeadLag.UpdatePartialResult through the embedded struct (M-SELF)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        self.base.update_partial_result(sctx, st, rows_in_group, pr)
    }

    // glue: baseLeadLag.update_partial_result_handles through the embedded struct (M-SELF)
    fn update_partial_result_handles(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[RowHandle],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        self.base
            .update_partial_result_handles(sctx, st, rows_in_group, pr)
    }

    // go: lag.AppendFinalResult2Chunk L2 (evaluator in partial result, spec §5.2)
    fn append_final_result2_chunk(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4LeadLag>(pr);
        let cur_idx = p.cur_idx;
        if cur_idx >= self.base.offset {
            let row = row_of(&p.rows[(cur_idx - self.base.offset) as usize]);
            let ev = p
                .evaluator
                .as_deref_mut()
                .expect("runtime error: invalid memory address or nil pointer dereference");
            ev.evaluate_row(sctx, &p.eval_state, &self.base.base.args[0], row)?;
        } else {
            let row = row_of(&p.rows[cur_idx as usize]);
            let ev = p
                .evaluator
                .as_deref_mut()
                .expect("runtime error: invalid memory address or nil pointer dereference");
            ev.evaluate_row(sctx, &p.eval_state, &self.base.default_expr, row)?;
        }
        p.evaluator_mut().append_result(chk, self.base.base.col());
        p.cur_idx += 1;
        Ok(())
    }

    // glue: A1 3.3 TrackedAlloc forwarding
    fn set_alloc(&mut self, alloc: ColAlloc) {
        self.base.set_alloc(alloc);
    }
}
