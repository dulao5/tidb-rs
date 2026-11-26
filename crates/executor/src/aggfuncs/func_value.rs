//! go-file: pkg/executor/aggfuncs/func_value.go
//!
//! Window value functions (`FIRST_VALUE`, `LAST_VALUE`, `NTH_VALUE`) and the shared
//! [`ValueEvaluator`] used by them as well as `LEAD` / `LAG` (part W of D12.4).
//!
//! Variable-length evaluated values (`String`, `JSON`, `VectorFloat32`) store their bytes
//! in [`ColAlloc`] so `TrackedAlloc` accounts them (A1, spec §5.3). Go's `memDelta` return
//! values are omitted (A1, spec §9 item 1).

// Go identifiers for eval-type constants in patterns (M-GONAME).
#![allow(non_upper_case_globals)]

use std::borrow::Cow;
use std::sync::Arc;

use allocator_api2::boxed::Box as AllocBox;
use tidb_base::Error;
use tidb_base::mysql;
use tidb_chunk::ColAlloc;
use tidb_chunk::chunk::Chunk;
use tidb_chunk::row::Row;
use tidb_expression::expression::{EvalState, Expr};
use tidb_parser::types::{
    ETDatetime, ETDecimal, ETDuration, ETInt, ETJson, ETReal, ETString, ETTimestamp,
    ETVectorFloat32, FieldType,
};
use tidb_types::MyDecimal;
use tidb_types::json::{BinaryJSON, JSONTypeCode};
use tidb_types::time::{Duration, Time};
use tidb_types::vector::zero_copy_deserialize_vector_float32;

use super::aggfuncs::{
    AggFunc, AggFuncUpdateContext, BaseAggFunc, PartialResult, box_bytes_in, partial_result_mut,
};

/// `valueEvaluator` is used to evaluate values for `first_value`, `last_value`, `nth_value`,
/// `lead` and `lag`.
pub(crate) trait ValueEvaluator: Send {
    /// `evaluateRow` evaluates the expression using row and stores the result inside.
    // glue: interface method of valueEvaluator (memDelta dropped, A1; `st` added, N4 §3.4)
    fn evaluate_row(
        &mut self,
        ctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        expr: &Expr,
        row: Row<'_>,
    ) -> Result<(), Error>;

    /// `appendResult` appends the result to chunk.
    // glue: interface method of valueEvaluator
    fn append_result(&self, chk: &mut Chunk, col_idx: usize);
}

#[derive(Default)]
struct Value4Int {
    val: i64,
    is_null: bool,
}

impl ValueEvaluator for Value4Int {
    // go: value4Int.evaluateRow L2 (memDelta dropped, A1)
    fn evaluate_row(
        &mut self,
        ctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        expr: &Expr,
        row: Row<'_>,
    ) -> Result<(), Error> {
        let input = expr.eval_int(ctx, st, row)?;
        self.is_null = input.is_none();
        self.val = input.unwrap_or_default();
        Ok(())
    }

    // go: value4Int.appendResult L1
    fn append_result(&self, chk: &mut Chunk, col_idx: usize) {
        if self.is_null {
            chk.append_null(col_idx);
        } else {
            chk.append_int64(col_idx, self.val);
        }
    }
}

#[derive(Default)]
struct Value4Float32 {
    val: f32,
    is_null: bool,
}

impl ValueEvaluator for Value4Float32 {
    // go: value4Float32.evaluateRow L2 (memDelta dropped, A1)
    fn evaluate_row(
        &mut self,
        ctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        expr: &Expr,
        row: Row<'_>,
    ) -> Result<(), Error> {
        let input = expr.eval_real(ctx, st, row)?;
        self.is_null = input.is_none();
        self.val = input.unwrap_or_default() as f32;
        Ok(())
    }

    // go: value4Float32.appendResult L1
    fn append_result(&self, chk: &mut Chunk, col_idx: usize) {
        if self.is_null {
            chk.append_null(col_idx);
        } else {
            chk.append_float32(col_idx, self.val);
        }
    }
}

#[derive(Default)]
struct Value4Decimal {
    val: MyDecimal,
    is_null: bool,
}

impl ValueEvaluator for Value4Decimal {
    // go: value4Decimal.evaluateRow L2 (memDelta dropped, A1)
    fn evaluate_row(
        &mut self,
        ctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        expr: &Expr,
        row: Row<'_>,
    ) -> Result<(), Error> {
        let input = expr.eval_decimal(ctx, st, row)?;
        self.is_null = input.is_none();
        if let Some(d) = input {
            self.val = d;
        }
        Ok(())
    }

    // go: value4Decimal.appendResult L1
    fn append_result(&self, chk: &mut Chunk, col_idx: usize) {
        if self.is_null {
            chk.append_null(col_idx);
        } else {
            chk.append_my_decimal(col_idx, &self.val);
        }
    }
}

#[derive(Default)]
struct Value4Float64 {
    val: f64,
    is_null: bool,
}

impl ValueEvaluator for Value4Float64 {
    // go: value4Float64.evaluateRow L2 (memDelta dropped, A1)
    fn evaluate_row(
        &mut self,
        ctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        expr: &Expr,
        row: Row<'_>,
    ) -> Result<(), Error> {
        let input = expr.eval_real(ctx, st, row)?;
        self.is_null = input.is_none();
        self.val = input.unwrap_or_default();
        Ok(())
    }

    // go: value4Float64.appendResult L1
    fn append_result(&self, chk: &mut Chunk, col_idx: usize) {
        if self.is_null {
            chk.append_null(col_idx);
        } else {
            chk.append_float64(col_idx, self.val);
        }
    }
}

struct Value4String {
    val: AllocBox<[u8], ColAlloc>,
    is_null: bool,
    alloc: ColAlloc,
}

impl Value4String {
    // glue: zero-initialized value4String with ColAlloc (A1 3.3)
    fn new(alloc: ColAlloc) -> Self {
        Self {
            val: box_bytes_in(&[], alloc.clone()),
            is_null: false,
            alloc,
        }
    }
}

impl ValueEvaluator for Value4String {
    // go: value4String.evaluateRow L2 (memDelta dropped, A1; bytes stored in ColAlloc)
    fn evaluate_row(
        &mut self,
        ctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        expr: &Expr,
        row: Row<'_>,
    ) -> Result<(), Error> {
        let input = expr.eval_string(ctx, st, row)?;
        self.is_null = input.is_none();
        self.val = match input {
            Some(b) => box_bytes_in(&b, self.alloc.clone()),
            None => box_bytes_in(&[], self.alloc.clone()),
        };
        Ok(())
    }

    // go: value4String.appendResult L1
    fn append_result(&self, chk: &mut Chunk, col_idx: usize) {
        if self.is_null {
            chk.append_null(col_idx);
        } else {
            chk.append_string(col_idx, &self.val);
        }
    }
}

#[derive(Default)]
struct Value4Time {
    val: Time,
    is_null: bool,
}

impl ValueEvaluator for Value4Time {
    // go: value4Time.evaluateRow L2 (memDelta dropped, A1)
    fn evaluate_row(
        &mut self,
        ctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        expr: &Expr,
        row: Row<'_>,
    ) -> Result<(), Error> {
        let input = expr.eval_time(ctx, st, row)?;
        self.is_null = input.is_none();
        self.val = input.unwrap_or_default();
        Ok(())
    }

    // go: value4Time.appendResult L1
    fn append_result(&self, chk: &mut Chunk, col_idx: usize) {
        if self.is_null {
            chk.append_null(col_idx);
        } else {
            chk.append_time(col_idx, self.val);
        }
    }
}

#[derive(Default)]
struct Value4Duration {
    val: Duration,
    is_null: bool,
}

impl ValueEvaluator for Value4Duration {
    // go: value4Duration.evaluateRow L2 (memDelta dropped, A1)
    fn evaluate_row(
        &mut self,
        ctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        expr: &Expr,
        row: Row<'_>,
    ) -> Result<(), Error> {
        let input = expr.eval_duration(ctx, st, row)?;
        self.is_null = input.is_none();
        self.val = input.unwrap_or_default();
        Ok(())
    }

    // go: value4Duration.appendResult L1
    fn append_result(&self, chk: &mut Chunk, col_idx: usize) {
        if self.is_null {
            chk.append_null(col_idx);
        } else {
            chk.append_duration(col_idx, self.val);
        }
    }
}

struct Value4JSON {
    type_code: JSONTypeCode,
    val: AllocBox<[u8], ColAlloc>,
    is_null: bool,
    alloc: ColAlloc,
}

impl Value4JSON {
    // glue: zero-initialized value4JSON with ColAlloc (A1 3.3)
    fn new(alloc: ColAlloc) -> Self {
        Self {
            type_code: 0,
            val: box_bytes_in(&[], alloc.clone()),
            is_null: false,
            alloc,
        }
    }
}

impl ValueEvaluator for Value4JSON {
    // go: value4JSON.evaluateRow L2 (memDelta dropped, A1; deep copy stored in ColAlloc)
    fn evaluate_row(
        &mut self,
        ctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        expr: &Expr,
        row: Row<'_>,
    ) -> Result<(), Error> {
        let input = expr.eval_json(ctx, st, row)?;
        self.is_null = input.is_none();
        match input {
            Some(j) => {
                self.type_code = j.type_code;
                self.val = box_bytes_in(&j.value, self.alloc.clone());
            }
            None => {
                self.type_code = 0;
                self.val = box_bytes_in(&[], self.alloc.clone());
            }
        }
        Ok(())
    }

    // go: value4JSON.appendResult L1
    fn append_result(&self, chk: &mut Chunk, col_idx: usize) {
        if self.is_null {
            chk.append_null(col_idx);
        } else {
            let bj = BinaryJSON {
                type_code: self.type_code,
                value: Cow::Borrowed(&self.val),
            };
            chk.append_json(col_idx, &bj);
        }
    }
}

struct Value4VectorFloat32 {
    data: AllocBox<[u8], ColAlloc>,
    is_null: bool,
    alloc: ColAlloc,
}

impl Value4VectorFloat32 {
    // glue: zero-initialized value4VectorFloat32 with ColAlloc (A1 3.3)
    fn new(alloc: ColAlloc) -> Self {
        Self {
            data: box_bytes_in(&[0, 0, 0, 0], alloc.clone()),
            is_null: false,
            alloc,
        }
    }
}

impl ValueEvaluator for Value4VectorFloat32 {
    // go: value4VectorFloat32.evaluateRow L2 (memDelta dropped, A1; deep copy stored in ColAlloc)
    fn evaluate_row(
        &mut self,
        ctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        expr: &Expr,
        row: Row<'_>,
    ) -> Result<(), Error> {
        let input = expr.eval_vector_float32(ctx, st, row)?;
        self.is_null = input.is_none();
        self.data = match input {
            Some(v) => box_bytes_in(v.zero_copy_serialize(), self.alloc.clone()),
            None => box_bytes_in(&[0, 0, 0, 0], self.alloc.clone()),
        };
        Ok(())
    }

    // go: value4VectorFloat32.appendResult L1
    fn append_result(&self, chk: &mut Chunk, col_idx: usize) {
        if self.is_null {
            chk.append_null(col_idx);
        } else {
            let (vv, _) = zero_copy_deserialize_vector_float32(&self.data)
                .expect("stored VectorFloat32 bytes are valid");
            chk.append_vector_float32(col_idx, &vv);
        }
    }
}

// go: buildValueEvaluator L2 (memDelta dropped, A1; allocator passed for variable-length values)
pub(crate) fn build_value_evaluator(
    tp: &FieldType,
    alloc: ColAlloc,
) -> Option<Box<dyn ValueEvaluator>> {
    let mut eval_type = tp.eval_type();
    if tp.get_type() == mysql::TypeBit {
        eval_type = ETString;
    }
    match eval_type {
        ETInt => Some(Box::new(Value4Int::default())),
        ETReal => match tp.get_type() {
            mysql::TypeFloat => Some(Box::new(Value4Float32::default())),
            mysql::TypeDouble => Some(Box::new(Value4Float64::default())),
            _ => None,
        },
        ETDecimal => Some(Box::new(Value4Decimal::default())),
        ETDatetime | ETTimestamp => Some(Box::new(Value4Time::default())),
        ETDuration => Some(Box::new(Value4Duration::default())),
        ETString => Some(Box::new(Value4String::new(alloc))),
        ETJson => Some(Box::new(Value4JSON::new(alloc))),
        ETVectorFloat32 => Some(Box::new(Value4VectorFloat32::new(alloc))),
        _ => panic!("unsupported eval type {}", eval_type.to_string()),
    }
}

// glue: dereference optional FieldType with Go's nil-pointer panic (M-PANIC)
fn deref_tp(tp: &Option<Arc<FieldType>>) -> &FieldType {
    match tp {
        Some(t) => t,
        None => panic!("runtime error: invalid memory address or nil pointer dereference"),
    }
}

// glue: dereference optional evaluator with Go's nil-interface panic (M-PANIC)
fn evaluator_mut(ev: &mut Option<Box<dyn ValueEvaluator>>) -> &mut dyn ValueEvaluator {
    match ev {
        Some(e) => &mut **e,
        None => panic!("runtime error: invalid memory address or nil pointer dereference"),
    }
}

// glue: dereference optional evaluator with Go's nil-interface panic (M-PANIC)
fn evaluator_ref(ev: &Option<Box<dyn ValueEvaluator>>) -> &dyn ValueEvaluator {
    match ev {
        Some(e) => &**e,
        None => panic!("runtime error: invalid memory address or nil pointer dereference"),
    }
}

/// `firstValue`.
#[derive(Clone, Debug)]
pub struct FirstValue {
    pub base: BaseAggFunc,
    pub tp: Option<Arc<FieldType>>,
    alloc: ColAlloc,
}

impl FirstValue {
    // glue: Go struct literal firstValue{baseAggFunc, tp}
    pub fn new(base: BaseAggFunc, tp: Option<Arc<FieldType>>) -> Self {
        Self {
            base,
            tp,
            alloc: ColAlloc::Global,
        }
    }
}

struct PartialResult4FirstValue {
    got_first_value: bool,
    evaluator: Option<Box<dyn ValueEvaluator>>,
}

impl AggFunc for FirstValue {
    // go: firstValue.AllocPartialResult L2 (memDelta dropped, A1)
    fn alloc_partial_result(&self) -> PartialResult {
        let ve = build_value_evaluator(deref_tp(&self.tp), self.alloc.clone());
        Box::new(PartialResult4FirstValue {
            got_first_value: false,
            evaluator: ve,
        })
    }

    // go: firstValue.ResetPartialResult L1
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4FirstValue>(pr);
        p.got_first_value = false;
    }

    // go: firstValue.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4FirstValue>(pr);
        if p.got_first_value {
            return Ok(());
        }
        if let Some(&first) = rows_in_group.first() {
            p.got_first_value = true;
            evaluator_mut(&mut p.evaluator).evaluate_row(sctx, st, &self.base.args[0], first)?;
        }
        Ok(())
    }

    // go: firstValue.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4FirstValue>(pr);
        let col = self.base.col();
        if !p.got_first_value {
            chk.append_null(col);
        } else {
            evaluator_ref(&p.evaluator).append_result(chk, col);
        }
        Ok(())
    }

    // glue: A1 3.3 TrackedAlloc for variable-length values
    fn set_alloc(&mut self, alloc: ColAlloc) {
        self.alloc = alloc;
    }
}

/// `lastValue`.
#[derive(Clone, Debug)]
pub struct LastValue {
    pub base: BaseAggFunc,
    pub tp: Option<Arc<FieldType>>,
    alloc: ColAlloc,
}

impl LastValue {
    // glue: Go struct literal lastValue{baseAggFunc, tp}
    pub fn new(base: BaseAggFunc, tp: Option<Arc<FieldType>>) -> Self {
        Self {
            base,
            tp,
            alloc: ColAlloc::Global,
        }
    }
}

struct PartialResult4LastValue {
    got_last_value: bool,
    evaluator: Option<Box<dyn ValueEvaluator>>,
}

impl AggFunc for LastValue {
    // go: lastValue.AllocPartialResult L2 (memDelta dropped, A1; allocates typed PartialResult4LastValue)
    fn alloc_partial_result(&self) -> PartialResult {
        let ve = build_value_evaluator(deref_tp(&self.tp), self.alloc.clone());
        Box::new(PartialResult4LastValue {
            got_last_value: false,
            evaluator: ve,
        })
    }

    // go: lastValue.ResetPartialResult L1
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4LastValue>(pr);
        p.got_last_value = false;
    }

    // go: lastValue.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4LastValue>(pr);
        if let Some(&last) = rows_in_group.last() {
            p.got_last_value = true;
            evaluator_mut(&mut p.evaluator).evaluate_row(sctx, st, &self.base.args[0], last)?;
        }
        Ok(())
    }

    // go: lastValue.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4LastValue>(pr);
        let col = self.base.col();
        if !p.got_last_value {
            chk.append_null(col);
        } else {
            evaluator_ref(&p.evaluator).append_result(chk, col);
        }
        Ok(())
    }

    // glue: A1 3.3 TrackedAlloc for variable-length values
    fn set_alloc(&mut self, alloc: ColAlloc) {
        self.alloc = alloc;
    }
}

/// `nthValue`.
#[derive(Clone, Debug)]
pub struct NthValue {
    pub base: BaseAggFunc,
    pub tp: Option<Arc<FieldType>>,
    pub nth: u64,
    alloc: ColAlloc,
}

impl NthValue {
    // glue: Go struct literal nthValue{baseAggFunc, tp, nth}
    pub fn new(base: BaseAggFunc, tp: Option<Arc<FieldType>>, nth: u64) -> Self {
        Self {
            base,
            tp,
            nth,
            alloc: ColAlloc::Global,
        }
    }
}

struct PartialResult4NthValue {
    seen_rows: u64,
    evaluator: Option<Box<dyn ValueEvaluator>>,
}

impl AggFunc for NthValue {
    // go: nthValue.AllocPartialResult L2 (memDelta dropped, A1; allocates typed PartialResult4NthValue)
    fn alloc_partial_result(&self) -> PartialResult {
        let ve = build_value_evaluator(deref_tp(&self.tp), self.alloc.clone());
        Box::new(PartialResult4NthValue {
            seen_rows: 0,
            evaluator: ve,
        })
    }

    // go: nthValue.ResetPartialResult L1
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4NthValue>(pr);
        p.seen_rows = 0;
    }

    // go: nthValue.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        if self.nth == 0 {
            return Ok(());
        }
        let p = partial_result_mut::<PartialResult4NthValue>(pr);
        let num_rows = rows_in_group.len() as u64;
        if self.nth > p.seen_rows && self.nth - p.seen_rows <= num_rows {
            let idx = (self.nth - p.seen_rows - 1) as usize;
            evaluator_mut(&mut p.evaluator).evaluate_row(
                sctx,
                st,
                &self.base.args[0],
                rows_in_group[idx],
            )?;
        }
        p.seen_rows = p.seen_rows.wrapping_add(num_rows);
        Ok(())
    }

    // go: nthValue.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4NthValue>(pr);
        let col = self.base.col();
        if self.nth == 0 || p.seen_rows < self.nth {
            chk.append_null(col);
        } else {
            evaluator_ref(&p.evaluator).append_result(chk, col);
        }
        Ok(())
    }

    // glue: A1 3.3 TrackedAlloc for variable-length values
    fn set_alloc(&mut self, alloc: ColAlloc) {
        self.alloc = alloc;
    }
}
