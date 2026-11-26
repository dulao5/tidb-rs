//! go-file: pkg/executor/aggfuncs/func_max_min.go
//!
//! MAX and MIN, both non-sliding (part A) and sliding-window (`MinMaxDeque`, `WindowInfo`,
//! and the 8 `MaxMin4*Sliding` implementations, part W of D12.4). ENUM and SET are evaluated
//! as datums and compared by their names with the collator of the result type; JSON,
//! VECTOR, ENUM and SET have no sliding variant (matching Go).

use std::sync::Arc;

use allocator_api2::boxed::Box as AllocBox;
use allocator_api2::vec::Vec as AllocVec;
use tidb_base::{Error, errors as goerrors};
use tidb_chunk::ColAlloc;
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

use super::aggfuncs::{
    AggFunc, AggFuncUpdateContext, BaseAggFunc, MaxMinSlidingWindowAggFunc, PartialResult,
    SlidingWindowAggFunc, box_bytes_in, partial_result_mut,
};
use super::func_sum::round_to_ret_tp;

/// `Pair` pairs items and their indices in [`MinMaxDeque`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pair<T> {
    pub item: T,
    pub idx: u64,
}

type DequeCmpFunc<T> = Box<dyn Fn(&T, &T) -> isize + Send + Sync>;

/// `MinMaxDeque` is a monotonic double-ended queue for sliding-window MAX/MIN.
///
/// The underlying storage allocates through [`ColAlloc`] (A1, spec §5.3).
pub struct MinMaxDeque<T> {
    items: AllocVec<Pair<T>, ColAlloc>,
    head: usize,
    pub is_max: bool,
    cmp_func: DequeCmpFunc<T>,
}

/// `NewDeque` inits a new [`MinMaxDeque`] in the global allocator.
// go: NewDeque L2 (typed generic instead of any; ColAlloc-backed)
pub fn new_deque<T, F>(is_max: bool, cmp_func: F) -> MinMaxDeque<T>
where
    F: Fn(&T, &T) -> isize + Send + Sync + 'static,
{
    new_deque_in(is_max, cmp_func, ColAlloc::Global)
}

/// `NewDeque` allocating its backing vector through `alloc` (A1 3.3).
// glue: A1 3.3 TrackedAlloc constructor for MinMaxDeque
pub fn new_deque_in<T, F>(is_max: bool, cmp_func: F, alloc: ColAlloc) -> MinMaxDeque<T>
where
    F: Fn(&T, &T) -> isize + Send + Sync + 'static,
{
    MinMaxDeque {
        items: AllocVec::new_in(alloc),
        head: 0,
        is_max,
        cmp_func: Box::new(cmp_func),
    }
}

impl<T> MinMaxDeque<T> {
    /// Current active elements in the deque.
    // glue: Go d.Items slice view (accounts for head offset)
    #[inline]
    pub fn items(&self) -> &[Pair<T>] {
        &self.items[self.head..]
    }

    /// `PushBack` pushes `idx` and `item` (wrapped in [`Pair`]) to the end of `MinMaxDeque`.
    // go: MinMaxDeque.PushBack L1
    pub fn push_back(&mut self, idx: u64, item: T) {
        self.items.push(Pair { item, idx });
    }

    /// `PopFront` pops an item from the front of `MinMaxDeque`.
    // go: MinMaxDeque.PopFront L1
    pub fn pop_front(&mut self) -> Result<(), Error> {
        if self.is_empty() {
            return Err(goerrors::new("Pop front when MinMaxDeque is empty"));
        }
        self.head += 1;
        if self.head == self.items.len() {
            self.items.clear();
            self.head = 0;
        } else if self.head >= 64 && self.head * 2 >= self.items.len() {
            self.items.drain(..self.head);
            self.head = 0;
        }
        Ok(())
    }

    /// `PopBack` pops an item from the end of `MinMaxDeque`.
    // go: MinMaxDeque.PopBack L1
    pub fn pop_back(&mut self) -> Result<(), Error> {
        if self.is_empty() {
            return Err(goerrors::new("Pop back when MinMaxDeque is empty"));
        }
        self.items.pop();
        if self.head == self.items.len() {
            self.items.clear();
            self.head = 0;
        }
        Ok(())
    }

    /// `Back` returns the element at the end of `MinMaxDeque`, or `None` if empty.
    // go: MinMaxDeque.Back L2 (Option<&Pair<T>> instead of (Pair, isEnd bool))
    pub fn back(&self) -> Option<&Pair<T>> {
        self.items().last()
    }

    /// `Front` returns the element at the front of `MinMaxDeque`, or `None` if empty.
    // go: MinMaxDeque.Front L2 (Option<&Pair<T>> instead of (Pair, isEnd bool))
    pub fn front(&self) -> Option<&Pair<T>> {
        self.items().first()
    }

    /// `IsEmpty` returns whether `MinMaxDeque` is empty.
    // go: MinMaxDeque.IsEmpty L1
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.head >= self.items.len()
    }

    /// `Reset` resets the deque for a [`MaxMinSlidingWindowAggFunc`].
    // go: MinMaxDeque.Reset L1
    pub fn reset(&mut self) {
        self.items.clear();
        self.head = 0;
    }

    /// `Dequeue` pops elements from the front whose index is `<= boundary`.
    // go: MinMaxDeque.Dequeue L1
    pub fn dequeue(&mut self, boundary: u64) -> Result<(), Error> {
        while !self.is_empty() {
            let Some(front_ele) = self.front() else {
                return Err(goerrors::new("Dequeue empty deque"));
            };
            if front_ele.idx <= boundary {
                self.pop_front()?;
                continue;
            }
            return Ok(());
        }
        Ok(())
    }

    /// `Enqueue` puts `item` at the back of the queue, popping any dominated elements from
    /// the back.
    // go: MinMaxDeque.Enqueue L1
    pub fn enqueue(&mut self, idx: u64, item: T) -> Result<(), Error> {
        while !self.is_empty() {
            let Some(pair) = self.back() else {
                return Err(goerrors::new("Dequeue empty deque"));
            };
            let cmp = (self.cmp_func)(&item, &pair.item);
            // 1. if MinMaxDeque aims for finding max and item is equal or bigger than element at back
            // 2. if MinMaxDeque aims for finding min and item is equal or smaller than element at back
            if !(cmp >= 0 && self.is_max || cmp <= 0 && !self.is_max) {
                break;
            }
            self.pop_back()?;
        }
        self.push_back(idx, item);
        Ok(())
    }
}

/// `windowInfo` stores the absolute start position of a sliding window.
#[derive(Clone, Copy, Debug, Default)]
pub struct WindowInfo {
    pub start: u64,
}

impl WindowInfo {
    /// `SetWindowStart` sets the absolute start position of the window.
    // go: windowInfo.SetWindowStart L1
    pub fn set_window_start(&mut self, start: u64) {
        self.start = start;
    }
}

/// Go's `cmp.Compare` on `float64`: `NaN == NaN`, `NaN < non-NaN`, `-0.0 == +0.0`.
// glue: Go stdlib cmp.Compare for float64
#[inline]
fn cmp_float64(x: f64, y: f64) -> isize {
    let x_nan = x.is_nan();
    let y_nan = y.is_nan();
    if x_nan && y_nan {
        0
    } else if x_nan || x < y {
        -1
    } else if y_nan || x > y {
        1
    } else {
        0
    }
}

/// `partialResult4MaxMinXxx` for non-sliding MAX/MIN.
#[derive(Debug)]
struct PartialResult4MaxMin<T> {
    val: T,
    // isNull is used to indicates:
    // 1. whether the partial result is the initialization value which should not be compared during evaluation;
    // 2. whether all the values of arg are all null, if so, we should return null as the default value for MAX/MIN.
    is_null: bool,
}

/// `partialResult4MaxMinXxx` with the monotonic deque for sliding-window MAX/MIN.
struct PartialResult4MaxMinSliding<T> {
    val: T,
    is_null: bool,
    deque: MinMaxDeque<T>,
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

// ---------------------------------------------------------------- Sliding MAX / MIN (part W)

/// `maxMin4IntSliding`.
#[derive(Clone)]
pub struct MaxMin4IntSliding {
    pub base: MaxMin4Int,
    pub window_info: WindowInfo,
    alloc: ColAlloc,
}

impl MaxMin4IntSliding {
    // glue: Go struct literal maxMin4IntSliding{maxMin4Int, windowInfo{}}
    pub fn new(base: MaxMin4Int) -> Self {
        Self {
            base,
            window_info: WindowInfo::default(),
            alloc: ColAlloc::Global,
        }
    }
}

impl MaxMinSlidingWindowAggFunc for MaxMin4IntSliding {
    // glue: windowInfo.SetWindowStart through the embedded struct (M-SELF)
    fn set_window_start(&mut self, start: u64) {
        self.window_info.set_window_start(start);
    }
}

impl AggFunc for MaxMin4IntSliding {
    // go: maxMin4IntSliding.AllocPartialResult L2 (memDelta dropped, A1; deque in ColAlloc)
    fn alloc_partial_result(&self) -> PartialResult {
        let deque = new_deque_in(
            self.base.base.is_max,
            |i: &i64, j: &i64| i.cmp(j) as isize,
            self.alloc.clone(),
        );
        Box::new(PartialResult4MaxMinSliding {
            val: 0i64,
            is_null: true,
            deque,
        })
    }

    // go: maxMin4IntSliding.ResetPartialResult L1
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4MaxMinSliding<i64>>(pr);
        p.val = 0;
        p.is_null = true;
        p.deque.reset();
    }

    // glue: maxMin4Int.AppendFinalResult2Chunk through the embedded struct (M-SELF)
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMinSliding<i64>>(pr);
        let col = self.base.base.base.col();
        if p.is_null {
            chk.append_null(col);
            return Ok(());
        }
        chk.append_int64(col, p.val);
        Ok(())
    }

    // go: maxMin4IntSliding.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMinSliding<i64>>(pr);
        for (i, &row) in rows_in_group.iter().enumerate() {
            let Some(input) = self.base.base.base.args[0].eval_int(sctx, st, row)? else {
                continue;
            };
            p.deque
                .enqueue((i as u64).wrapping_add(self.window_info.start), input)?;
        }
        if let Some(front) = p.deque.front() {
            p.val = front.item;
            p.is_null = false;
        } else {
            p.is_null = true;
        }
        Ok(())
    }

    // glue: A1 3.3 TrackedAlloc for sliding deque
    fn set_alloc(&mut self, alloc: ColAlloc) {
        self.alloc = alloc;
    }

    // glue: Go type assertion agg.(SlidingWindowAggFunc) (spec §5.1)
    fn as_sliding_window(&self) -> Option<&dyn SlidingWindowAggFunc> {
        Some(self)
    }

    // glue: Go type assertion agg.(MaxMinSlidingWindowAggFunc) (spec §5.1)
    fn as_max_min_sliding_mut(&mut self) -> Option<&mut dyn MaxMinSlidingWindowAggFunc> {
        Some(self)
    }
}

impl SlidingWindowAggFunc for MaxMin4IntSliding {
    // go: maxMin4IntSliding.Slide L1
    fn slide<'a>(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        get_row: &dyn Fn(u64) -> Row<'a>,
        last_start: u64,
        last_end: u64,
        shift_start: u64,
        shift_end: u64,
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMinSliding<i64>>(pr);
        for i in 0..shift_end {
            let Some(input) =
                self.base.base.base.args[0].eval_int(sctx, st, get_row(last_end + i))?
            else {
                continue;
            };
            p.deque.enqueue(last_end + i, input)?;
        }
        if last_start + shift_start >= 1 {
            p.deque.dequeue(last_start + shift_start - 1)?;
        }
        if let Some(front) = p.deque.front() {
            p.val = front.item;
            p.is_null = false;
        } else {
            p.is_null = true;
        }
        Ok(())
    }
}

/// `maxMin4UintSliding`.
#[derive(Clone)]
pub struct MaxMin4UintSliding {
    pub base: MaxMin4Uint,
    pub window_info: WindowInfo,
    alloc: ColAlloc,
}

impl MaxMin4UintSliding {
    // glue: Go struct literal maxMin4UintSliding{maxMin4Uint, windowInfo{}}
    pub fn new(base: MaxMin4Uint) -> Self {
        Self {
            base,
            window_info: WindowInfo::default(),
            alloc: ColAlloc::Global,
        }
    }
}

impl MaxMinSlidingWindowAggFunc for MaxMin4UintSliding {
    // glue: windowInfo.SetWindowStart through the embedded struct (M-SELF)
    fn set_window_start(&mut self, start: u64) {
        self.window_info.set_window_start(start);
    }
}

impl AggFunc for MaxMin4UintSliding {
    // go: maxMin4UintSliding.AllocPartialResult L2 (memDelta dropped, A1; deque in ColAlloc)
    fn alloc_partial_result(&self) -> PartialResult {
        let deque = new_deque_in(
            self.base.base.is_max,
            |i: &u64, j: &u64| i.cmp(j) as isize,
            self.alloc.clone(),
        );
        Box::new(PartialResult4MaxMinSliding {
            val: 0u64,
            is_null: true,
            deque,
        })
    }

    // go: maxMin4UintSliding.ResetPartialResult L1
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4MaxMinSliding<u64>>(pr);
        p.val = 0;
        p.is_null = true;
        p.deque.reset();
    }

    // glue: maxMin4Uint.AppendFinalResult2Chunk through the embedded struct (M-SELF)
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMinSliding<u64>>(pr);
        let col = self.base.base.base.col();
        if p.is_null {
            chk.append_null(col);
            return Ok(());
        }
        chk.append_uint64(col, p.val);
        Ok(())
    }

    // go: maxMin4UintSliding.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMinSliding<u64>>(pr);
        for (i, &row) in rows_in_group.iter().enumerate() {
            let Some(input) = self.base.base.base.args[0].eval_int(sctx, st, row)? else {
                continue;
            };
            p.deque.enqueue(
                (i as u64).wrapping_add(self.window_info.start),
                input as u64,
            )?;
        }
        if let Some(front) = p.deque.front() {
            p.val = front.item;
            p.is_null = false;
        } else {
            p.is_null = true;
        }
        Ok(())
    }

    // glue: A1 3.3 TrackedAlloc for sliding deque
    fn set_alloc(&mut self, alloc: ColAlloc) {
        self.alloc = alloc;
    }

    // glue: Go type assertion agg.(SlidingWindowAggFunc) (spec §5.1)
    fn as_sliding_window(&self) -> Option<&dyn SlidingWindowAggFunc> {
        Some(self)
    }

    // glue: Go type assertion agg.(MaxMinSlidingWindowAggFunc) (spec §5.1)
    fn as_max_min_sliding_mut(&mut self) -> Option<&mut dyn MaxMinSlidingWindowAggFunc> {
        Some(self)
    }
}

impl SlidingWindowAggFunc for MaxMin4UintSliding {
    // go: maxMin4UintSliding.Slide L1
    fn slide<'a>(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        get_row: &dyn Fn(u64) -> Row<'a>,
        last_start: u64,
        last_end: u64,
        shift_start: u64,
        shift_end: u64,
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMinSliding<u64>>(pr);
        for i in 0..shift_end {
            let Some(input) =
                self.base.base.base.args[0].eval_int(sctx, st, get_row(last_end + i))?
            else {
                continue;
            };
            p.deque.enqueue(last_end + i, input as u64)?;
        }
        if last_start + shift_start >= 1 {
            p.deque.dequeue(last_start + shift_start - 1)?;
        }
        if let Some(front) = p.deque.front() {
            p.val = front.item;
            p.is_null = false;
        } else {
            p.is_null = true;
        }
        Ok(())
    }
}

/// `maxMin4Float32Sliding`.
#[derive(Clone)]
pub struct MaxMin4Float32Sliding {
    pub base: MaxMin4Float32,
    pub window_info: WindowInfo,
    alloc: ColAlloc,
}

impl MaxMin4Float32Sliding {
    // glue: Go struct literal maxMin4Float32Sliding{maxMin4Float32, windowInfo{}}
    pub fn new(base: MaxMin4Float32) -> Self {
        Self {
            base,
            window_info: WindowInfo::default(),
            alloc: ColAlloc::Global,
        }
    }
}

impl MaxMinSlidingWindowAggFunc for MaxMin4Float32Sliding {
    // glue: windowInfo.SetWindowStart through the embedded struct (M-SELF)
    fn set_window_start(&mut self, start: u64) {
        self.window_info.set_window_start(start);
    }
}

impl AggFunc for MaxMin4Float32Sliding {
    // go: maxMin4Float32Sliding.AllocPartialResult L2 (memDelta dropped, A1; deque in ColAlloc)
    fn alloc_partial_result(&self) -> PartialResult {
        let deque = new_deque_in(
            self.base.base.is_max,
            |i: &f32, j: &f32| cmp_float64(*i as f64, *j as f64),
            self.alloc.clone(),
        );
        Box::new(PartialResult4MaxMinSliding {
            val: 0.0f32,
            is_null: true,
            deque,
        })
    }

    // go: maxMin4Float32Sliding.ResetPartialResult L1
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4MaxMinSliding<f32>>(pr);
        p.val = 0.0;
        p.is_null = true;
        p.deque.reset();
    }

    // glue: maxMin4Float32.AppendFinalResult2Chunk through the embedded struct (M-SELF)
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMinSliding<f32>>(pr);
        let col = self.base.base.base.col();
        if p.is_null {
            chk.append_null(col);
            return Ok(());
        }
        chk.append_float32(col, p.val);
        Ok(())
    }

    // go: maxMin4Float32Sliding.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMinSliding<f32>>(pr);
        for (i, &row) in rows_in_group.iter().enumerate() {
            let Some(input) = self.base.base.base.args[0].eval_real(sctx, st, row)? else {
                continue;
            };
            p.deque.enqueue(
                (i as u64).wrapping_add(self.window_info.start),
                input as f32,
            )?;
        }
        if let Some(front) = p.deque.front() {
            p.val = front.item;
            p.is_null = false;
        } else {
            p.is_null = true;
        }
        Ok(())
    }

    // glue: A1 3.3 TrackedAlloc for sliding deque
    fn set_alloc(&mut self, alloc: ColAlloc) {
        self.alloc = alloc;
    }

    // glue: Go type assertion agg.(SlidingWindowAggFunc) (spec §5.1)
    fn as_sliding_window(&self) -> Option<&dyn SlidingWindowAggFunc> {
        Some(self)
    }

    // glue: Go type assertion agg.(MaxMinSlidingWindowAggFunc) (spec §5.1)
    fn as_max_min_sliding_mut(&mut self) -> Option<&mut dyn MaxMinSlidingWindowAggFunc> {
        Some(self)
    }
}

impl SlidingWindowAggFunc for MaxMin4Float32Sliding {
    // go: maxMin4Float32Sliding.Slide L1
    fn slide<'a>(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        get_row: &dyn Fn(u64) -> Row<'a>,
        last_start: u64,
        last_end: u64,
        shift_start: u64,
        shift_end: u64,
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMinSliding<f32>>(pr);
        for i in 0..shift_end {
            let Some(input) =
                self.base.base.base.args[0].eval_real(sctx, st, get_row(last_end + i))?
            else {
                continue;
            };
            p.deque.enqueue(last_end + i, input as f32)?;
        }
        if last_start + shift_start >= 1 {
            p.deque.dequeue(last_start + shift_start - 1)?;
        }
        if let Some(front) = p.deque.front() {
            p.val = front.item;
            p.is_null = false;
        } else {
            p.is_null = true;
        }
        Ok(())
    }
}

/// `maxMin4Float64Sliding`.
#[derive(Clone)]
pub struct MaxMin4Float64Sliding {
    pub base: MaxMin4Float64,
    pub window_info: WindowInfo,
    alloc: ColAlloc,
}

impl MaxMin4Float64Sliding {
    // glue: Go struct literal maxMin4Float64Sliding{maxMin4Float64, windowInfo{}}
    pub fn new(base: MaxMin4Float64) -> Self {
        Self {
            base,
            window_info: WindowInfo::default(),
            alloc: ColAlloc::Global,
        }
    }
}

impl MaxMinSlidingWindowAggFunc for MaxMin4Float64Sliding {
    // glue: windowInfo.SetWindowStart through the embedded struct (M-SELF)
    fn set_window_start(&mut self, start: u64) {
        self.window_info.set_window_start(start);
    }
}

impl AggFunc for MaxMin4Float64Sliding {
    // go: maxMin4Float64Sliding.AllocPartialResult L2 (memDelta dropped, A1; deque in ColAlloc)
    fn alloc_partial_result(&self) -> PartialResult {
        let deque = new_deque_in(
            self.base.base.is_max,
            |i: &f64, j: &f64| cmp_float64(*i, *j),
            self.alloc.clone(),
        );
        Box::new(PartialResult4MaxMinSliding {
            val: 0.0f64,
            is_null: true,
            deque,
        })
    }

    // go: maxMin4Float64Sliding.ResetPartialResult L1
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4MaxMinSliding<f64>>(pr);
        p.val = 0.0;
        p.is_null = true;
        p.deque.reset();
    }

    // glue: maxMin4Float64.AppendFinalResult2Chunk through the embedded struct (M-SELF)
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMinSliding<f64>>(pr);
        let col = self.base.base.base.col();
        if p.is_null {
            chk.append_null(col);
            return Ok(());
        }
        chk.append_float64(col, p.val);
        Ok(())
    }

    // go: maxMin4Float64Sliding.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMinSliding<f64>>(pr);
        for (i, &row) in rows_in_group.iter().enumerate() {
            let Some(input) = self.base.base.base.args[0].eval_real(sctx, st, row)? else {
                continue;
            };
            p.deque
                .enqueue((i as u64).wrapping_add(self.window_info.start), input)?;
        }
        if let Some(front) = p.deque.front() {
            p.val = front.item;
            p.is_null = false;
        } else {
            p.is_null = true;
        }
        Ok(())
    }

    // glue: A1 3.3 TrackedAlloc for sliding deque
    fn set_alloc(&mut self, alloc: ColAlloc) {
        self.alloc = alloc;
    }

    // glue: Go type assertion agg.(SlidingWindowAggFunc) (spec §5.1)
    fn as_sliding_window(&self) -> Option<&dyn SlidingWindowAggFunc> {
        Some(self)
    }

    // glue: Go type assertion agg.(MaxMinSlidingWindowAggFunc) (spec §5.1)
    fn as_max_min_sliding_mut(&mut self) -> Option<&mut dyn MaxMinSlidingWindowAggFunc> {
        Some(self)
    }
}

impl SlidingWindowAggFunc for MaxMin4Float64Sliding {
    // go: maxMin4Float64Sliding.Slide L1
    fn slide<'a>(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        get_row: &dyn Fn(u64) -> Row<'a>,
        last_start: u64,
        last_end: u64,
        shift_start: u64,
        shift_end: u64,
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMinSliding<f64>>(pr);
        for i in 0..shift_end {
            let Some(input) =
                self.base.base.base.args[0].eval_real(sctx, st, get_row(last_end + i))?
            else {
                continue;
            };
            p.deque.enqueue(last_end + i, input)?;
        }
        if last_start + shift_start >= 1 {
            p.deque.dequeue(last_start + shift_start - 1)?;
        }
        if let Some(front) = p.deque.front() {
            p.val = front.item;
            p.is_null = false;
        } else {
            p.is_null = true;
        }
        Ok(())
    }
}

/// `maxMin4DecimalSliding`.
#[derive(Clone)]
pub struct MaxMin4DecimalSliding {
    pub base: MaxMin4Decimal,
    pub window_info: WindowInfo,
    alloc: ColAlloc,
}

impl MaxMin4DecimalSliding {
    // glue: Go struct literal maxMin4DecimalSliding{maxMin4Decimal, windowInfo{}}
    pub fn new(base: MaxMin4Decimal) -> Self {
        Self {
            base,
            window_info: WindowInfo::default(),
            alloc: ColAlloc::Global,
        }
    }
}

impl MaxMinSlidingWindowAggFunc for MaxMin4DecimalSliding {
    // glue: windowInfo.SetWindowStart through the embedded struct (M-SELF)
    fn set_window_start(&mut self, start: u64) {
        self.window_info.set_window_start(start);
    }
}

impl AggFunc for MaxMin4DecimalSliding {
    // go: maxMin4DecimalSliding.AllocPartialResult L2 (memDelta dropped, A1; deque in ColAlloc)
    fn alloc_partial_result(&self) -> PartialResult {
        let deque = new_deque_in(
            self.base.base.is_max,
            |i: &MyDecimal, j: &MyDecimal| i.compare(j),
            self.alloc.clone(),
        );
        Box::new(PartialResult4MaxMinSliding {
            val: MyDecimal::default(),
            is_null: true,
            deque,
        })
    }

    // go: maxMin4DecimalSliding.ResetPartialResult L1
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4MaxMinSliding<MyDecimal>>(pr);
        p.is_null = true;
        p.deque.reset();
    }

    // glue: maxMin4Decimal.AppendFinalResult2Chunk through the embedded struct (M-SELF)
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMinSliding<MyDecimal>>(pr);
        if p.is_null {
            chk.append_null(self.base.base.base.col());
            return Ok(());
        }
        round_to_ret_tp(&self.base.base.base, "max or min", &mut p.val)?;
        chk.append_my_decimal(self.base.base.base.col(), &p.val);
        Ok(())
    }

    // go: maxMin4DecimalSliding.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMinSliding<MyDecimal>>(pr);
        for (i, &row) in rows_in_group.iter().enumerate() {
            let Some(input) = self.base.base.base.args[0].eval_decimal(sctx, st, row)? else {
                continue;
            };
            p.deque
                .enqueue((i as u64).wrapping_add(self.window_info.start), input)?;
        }
        if let Some(front) = p.deque.front() {
            p.val = front.item;
            p.is_null = false;
        } else {
            p.is_null = true;
        }
        Ok(())
    }

    // glue: A1 3.3 TrackedAlloc for sliding deque
    fn set_alloc(&mut self, alloc: ColAlloc) {
        self.alloc = alloc;
    }

    // glue: Go type assertion agg.(SlidingWindowAggFunc) (spec §5.1)
    fn as_sliding_window(&self) -> Option<&dyn SlidingWindowAggFunc> {
        Some(self)
    }

    // glue: Go type assertion agg.(MaxMinSlidingWindowAggFunc) (spec §5.1)
    fn as_max_min_sliding_mut(&mut self) -> Option<&mut dyn MaxMinSlidingWindowAggFunc> {
        Some(self)
    }
}

impl SlidingWindowAggFunc for MaxMin4DecimalSliding {
    // go: maxMin4DecimalSliding.Slide L1
    fn slide<'a>(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        get_row: &dyn Fn(u64) -> Row<'a>,
        last_start: u64,
        last_end: u64,
        shift_start: u64,
        shift_end: u64,
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMinSliding<MyDecimal>>(pr);
        for i in 0..shift_end {
            let Some(input) =
                self.base.base.base.args[0].eval_decimal(sctx, st, get_row(last_end + i))?
            else {
                continue;
            };
            p.deque.enqueue(last_end + i, input)?;
        }
        if last_start + shift_start >= 1 {
            p.deque.dequeue(last_start + shift_start - 1)?;
        }
        if let Some(front) = p.deque.front() {
            p.val = front.item;
            p.is_null = false;
        } else {
            p.is_null = true;
        }
        Ok(())
    }
}

/// `maxMin4StringSliding`.
#[derive(Clone)]
pub struct MaxMin4StringSliding {
    pub base: MaxMin4String,
    pub window_info: WindowInfo,
    pub collate: String,
    alloc: ColAlloc,
}

impl MaxMin4StringSliding {
    // glue: Go struct literal maxMin4StringSliding{maxMin4String, windowInfo{}, collate}
    pub fn new(base: MaxMin4String, collate: String) -> Self {
        Self {
            base,
            window_info: WindowInfo::default(),
            collate,
            alloc: ColAlloc::Global,
        }
    }
}

impl MaxMinSlidingWindowAggFunc for MaxMin4StringSliding {
    // glue: windowInfo.SetWindowStart through the embedded struct (M-SELF)
    fn set_window_start(&mut self, start: u64) {
        self.window_info.set_window_start(start);
    }
}

impl AggFunc for MaxMin4StringSliding {
    // go: maxMin4StringSliding.AllocPartialResult L2 (memDelta dropped, A1; deque and string bytes in ColAlloc)
    fn alloc_partial_result(&self) -> PartialResult {
        let collate = self.collate.clone();
        let deque = new_deque_in(
            self.base.base.is_max,
            move |i: &AllocBox<[u8], ColAlloc>, j: &AllocBox<[u8], ColAlloc>| {
                compare_string(i, j, &collate)
            },
            self.alloc.clone(),
        );
        Box::new(PartialResult4MaxMinSliding {
            val: box_bytes_in(&[], self.alloc.clone()),
            is_null: true,
            deque,
        })
    }

    // go: maxMin4StringSliding.ResetPartialResult L1
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4MaxMinSliding<AllocBox<[u8], ColAlloc>>>(pr);
        p.is_null = true;
        p.deque.reset();
    }

    // glue: maxMin4String.AppendFinalResult2Chunk through the embedded struct (M-SELF)
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMinSliding<AllocBox<[u8], ColAlloc>>>(pr);
        let col = self.base.base.base.col();
        if p.is_null {
            chk.append_null(col);
            return Ok(());
        }
        chk.append_string(col, &p.val);
        Ok(())
    }

    // go: maxMin4StringSliding.UpdatePartialResult L2 (memDelta dropped, A1; owned string copy in ColAlloc, spec §5.6)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMinSliding<AllocBox<[u8], ColAlloc>>>(pr);
        for (i, &row) in rows_in_group.iter().enumerate() {
            let Some(input) = self.base.base.base.args[0].eval_string(sctx, st, row)? else {
                continue;
            };
            let boxed = box_bytes_in(&input, self.alloc.clone());
            p.deque
                .enqueue((i as u64).wrapping_add(self.window_info.start), boxed)?;
        }
        if let Some(front) = p.deque.front() {
            p.val = box_bytes_in(&front.item, self.alloc.clone());
            p.is_null = false;
        } else {
            p.is_null = true;
        }
        Ok(())
    }

    // glue: A1 3.3 TrackedAlloc for sliding deque and string payloads
    fn set_alloc(&mut self, alloc: ColAlloc) {
        self.alloc = alloc;
    }

    // glue: Go type assertion agg.(SlidingWindowAggFunc) (spec §5.1)
    fn as_sliding_window(&self) -> Option<&dyn SlidingWindowAggFunc> {
        Some(self)
    }

    // glue: Go type assertion agg.(MaxMinSlidingWindowAggFunc) (spec §5.1)
    fn as_max_min_sliding_mut(&mut self) -> Option<&mut dyn MaxMinSlidingWindowAggFunc> {
        Some(self)
    }
}

impl SlidingWindowAggFunc for MaxMin4StringSliding {
    // go: maxMin4StringSliding.Slide L2 (owned string copy in ColAlloc, spec §5.6)
    fn slide<'a>(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        get_row: &dyn Fn(u64) -> Row<'a>,
        last_start: u64,
        last_end: u64,
        shift_start: u64,
        shift_end: u64,
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMinSliding<AllocBox<[u8], ColAlloc>>>(pr);
        for i in 0..shift_end {
            let Some(input) =
                self.base.base.base.args[0].eval_string(sctx, st, get_row(last_end + i))?
            else {
                continue;
            };
            let boxed = box_bytes_in(&input, self.alloc.clone());
            p.deque.enqueue(last_end + i, boxed)?;
        }
        if last_start + shift_start >= 1 {
            p.deque.dequeue(last_start + shift_start - 1)?;
        }
        if let Some(front) = p.deque.front() {
            p.val = box_bytes_in(&front.item, self.alloc.clone());
            p.is_null = false;
        } else {
            p.is_null = true;
        }
        Ok(())
    }
}

/// `maxMin4TimeSliding`.
#[derive(Clone)]
pub struct MaxMin4TimeSliding {
    pub base: MaxMin4Time,
    pub window_info: WindowInfo,
    alloc: ColAlloc,
}

impl MaxMin4TimeSliding {
    // glue: Go struct literal maxMin4TimeSliding{maxMin4Time, windowInfo{}}
    pub fn new(base: MaxMin4Time) -> Self {
        Self {
            base,
            window_info: WindowInfo::default(),
            alloc: ColAlloc::Global,
        }
    }
}

impl MaxMinSlidingWindowAggFunc for MaxMin4TimeSliding {
    // glue: windowInfo.SetWindowStart through the embedded struct (M-SELF)
    fn set_window_start(&mut self, start: u64) {
        self.window_info.set_window_start(start);
    }
}

impl AggFunc for MaxMin4TimeSliding {
    // go: maxMin4TimeSliding.AllocPartialResult L2 (memDelta dropped, A1; deque in ColAlloc)
    fn alloc_partial_result(&self) -> PartialResult {
        let deque = new_deque_in(
            self.base.base.is_max,
            |i: &Time, j: &Time| i.compare(j),
            self.alloc.clone(),
        );
        Box::new(PartialResult4MaxMinSliding {
            val: Time::default(),
            is_null: true,
            deque,
        })
    }

    // go: maxMin4TimeSliding.ResetPartialResult L1
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4MaxMinSliding<Time>>(pr);
        p.is_null = true;
        p.deque.reset();
    }

    // glue: maxMin4Time.AppendFinalResult2Chunk through the embedded struct (M-SELF)
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMinSliding<Time>>(pr);
        let col = self.base.base.base.col();
        if p.is_null {
            chk.append_null(col);
            return Ok(());
        }
        chk.append_time(col, p.val);
        Ok(())
    }

    // go: maxMin4TimeSliding.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMinSliding<Time>>(pr);
        for (i, &row) in rows_in_group.iter().enumerate() {
            let Some(input) = self.base.base.base.args[0].eval_time(sctx, st, row)? else {
                continue;
            };
            p.deque
                .enqueue((i as u64).wrapping_add(self.window_info.start), input)?;
        }
        if let Some(front) = p.deque.front() {
            p.val = front.item;
            p.is_null = false;
        } else {
            p.is_null = true;
        }
        Ok(())
    }

    // glue: A1 3.3 TrackedAlloc for sliding deque
    fn set_alloc(&mut self, alloc: ColAlloc) {
        self.alloc = alloc;
    }

    // glue: Go type assertion agg.(SlidingWindowAggFunc) (spec §5.1)
    fn as_sliding_window(&self) -> Option<&dyn SlidingWindowAggFunc> {
        Some(self)
    }

    // glue: Go type assertion agg.(MaxMinSlidingWindowAggFunc) (spec §5.1)
    fn as_max_min_sliding_mut(&mut self) -> Option<&mut dyn MaxMinSlidingWindowAggFunc> {
        Some(self)
    }
}

impl SlidingWindowAggFunc for MaxMin4TimeSliding {
    // go: maxMin4TimeSliding.Slide L1
    fn slide<'a>(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        get_row: &dyn Fn(u64) -> Row<'a>,
        last_start: u64,
        last_end: u64,
        shift_start: u64,
        shift_end: u64,
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMinSliding<Time>>(pr);
        for i in 0..shift_end {
            let Some(input) =
                self.base.base.base.args[0].eval_time(sctx, st, get_row(last_end + i))?
            else {
                continue;
            };
            p.deque.enqueue(last_end + i, input)?;
        }
        if last_start + shift_start >= 1 {
            p.deque.dequeue(last_start + shift_start - 1)?;
        }
        if let Some(front) = p.deque.front() {
            p.val = front.item;
            p.is_null = false;
        } else {
            p.is_null = true;
        }
        Ok(())
    }
}

/// `maxMin4DurationSliding`.
#[derive(Clone)]
pub struct MaxMin4DurationSliding {
    pub base: MaxMin4Duration,
    pub window_info: WindowInfo,
    alloc: ColAlloc,
}

impl MaxMin4DurationSliding {
    // glue: Go struct literal maxMin4DurationSliding{maxMin4Duration, windowInfo{}}
    pub fn new(base: MaxMin4Duration) -> Self {
        Self {
            base,
            window_info: WindowInfo::default(),
            alloc: ColAlloc::Global,
        }
    }
}

impl MaxMinSlidingWindowAggFunc for MaxMin4DurationSliding {
    // glue: windowInfo.SetWindowStart through the embedded struct (M-SELF)
    fn set_window_start(&mut self, start: u64) {
        self.window_info.set_window_start(start);
    }
}

impl AggFunc for MaxMin4DurationSliding {
    // go: maxMin4DurationSliding.AllocPartialResult L2 (memDelta dropped, A1; deque in ColAlloc)
    fn alloc_partial_result(&self) -> PartialResult {
        let deque = new_deque_in(
            self.base.base.is_max,
            |i: &Duration, j: &Duration| i.compare(*j),
            self.alloc.clone(),
        );
        Box::new(PartialResult4MaxMinSliding {
            val: Duration::default(),
            is_null: true,
            deque,
        })
    }

    // go: maxMin4DurationSliding.ResetPartialResult L1
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4MaxMinSliding<Duration>>(pr);
        p.is_null = true;
        p.deque.reset();
    }

    // glue: maxMin4Duration.AppendFinalResult2Chunk through the embedded struct (M-SELF)
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMinSliding<Duration>>(pr);
        let col = self.base.base.base.col();
        if p.is_null {
            chk.append_null(col);
            return Ok(());
        }
        chk.append_duration(col, p.val);
        Ok(())
    }

    // go: maxMin4DurationSliding.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMinSliding<Duration>>(pr);
        for (i, &row) in rows_in_group.iter().enumerate() {
            let Some(input) = self.base.base.base.args[0].eval_duration(sctx, st, row)? else {
                continue;
            };
            p.deque
                .enqueue((i as u64).wrapping_add(self.window_info.start), input)?;
        }
        if let Some(front) = p.deque.front() {
            p.val = front.item;
            p.is_null = false;
        } else {
            p.is_null = true;
        }
        Ok(())
    }

    // glue: A1 3.3 TrackedAlloc for sliding deque
    fn set_alloc(&mut self, alloc: ColAlloc) {
        self.alloc = alloc;
    }

    // glue: Go type assertion agg.(SlidingWindowAggFunc) (spec §5.1)
    fn as_sliding_window(&self) -> Option<&dyn SlidingWindowAggFunc> {
        Some(self)
    }

    // glue: Go type assertion agg.(MaxMinSlidingWindowAggFunc) (spec §5.1)
    fn as_max_min_sliding_mut(&mut self) -> Option<&mut dyn MaxMinSlidingWindowAggFunc> {
        Some(self)
    }
}

impl SlidingWindowAggFunc for MaxMin4DurationSliding {
    // go: maxMin4DurationSliding.Slide L1
    fn slide<'a>(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        get_row: &dyn Fn(u64) -> Row<'a>,
        last_start: u64,
        last_end: u64,
        shift_start: u64,
        shift_end: u64,
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4MaxMinSliding<Duration>>(pr);
        for i in 0..shift_end {
            let Some(input) =
                self.base.base.base.args[0].eval_duration(sctx, st, get_row(last_end + i))?
            else {
                continue;
            };
            p.deque.enqueue(last_end + i, input)?;
        }
        if last_start + shift_start >= 1 {
            p.deque.dequeue(last_start + shift_start - 1)?;
        }
        if let Some(front) = p.deque.front() {
            p.val = front.item;
            p.is_null = false;
        } else {
            p.is_null = true;
        }
        Ok(())
    }
}
