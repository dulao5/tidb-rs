//! go-file: pkg/executor/aggfuncs/func_percentile.go
//!
//! `APPROX_PERCENTILE` (part B2 of D12.4, spec §3.3).
//!
//! Growing value slices allocate through [`AggFunc::set_alloc`] (A1, spec §5.3); the manual
//! `MemSize` methods (`partialResult4Percentile{Int,Real,Decimal,Time,Duration}.MemSize`) and
//! `memDelta` returns are omitted as A1 exceptions (spec §3.6, §9 item 1).
//!
//! Per spec §5.6 and §9.1 item 7, production execution uses a real thread-local RNG
//! (`rand::thread_rng()`) for `selection::select`'s random pivot, while unit tests and
//! `govec` replay may inject a fixed seed via [`AggFunc::set_rng_seed_for_test`].

// go-exception: A1 partialResult4PercentileInt.MemSize (only computes Go's manual memDelta; Rust tracks the buffer in ColAlloc, spec §3.6)
// go-exception: A1 partialResult4PercentileReal.MemSize (only computes Go's manual memDelta; Rust tracks the buffer in ColAlloc, spec §3.6)
// go-exception: A1 partialResult4PercentileDecimal.MemSize (only computes Go's manual memDelta; Rust tracks the buffer in ColAlloc, spec §3.6)
// go-exception: A1 partialResult4PercentileTime.MemSize (only computes Go's manual memDelta; Rust tracks the buffer in ColAlloc, spec §3.6)
// go-exception: A1 partialResult4PercentileDuration.MemSize (only computes Go's manual memDelta; Rust tracks the buffer in ColAlloc, spec §3.6)

use allocator_api2::vec::Vec as AllocVec;
use rand::Rng;
use tidb_base::Error;
use tidb_base::gostd::conv::f64_to_i64;
use tidb_base::selection;
use tidb_chunk::ColAlloc;
use tidb_chunk::chunk::Chunk;
use tidb_chunk::row::Row;
use tidb_expression::expression::EvalState;
use tidb_types::MyDecimal;
use tidb_types::time::{Duration, Time};

use super::aggfuncs::{
    AggFunc, AggFuncUpdateContext, BaseAggFunc, PartialResult, partial_result_mut,
};

/// Deterministic SplitMix64 generator used only when a test or `govec` replay injects a seed.
// glue: seeded PRNG for selection::select (spec §5.6, §9.1 item 7)
struct SeededRng(u64);

impl SeededRng {
    // glue: next non-negative random usize for seeded selection::select
    fn next_nonneg(&mut self) -> usize {
        self.0 = self.0.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        ((z ^ (z >> 31)) >> 1) as usize
    }
}

// go: percentile L2 (threads optional test seed to selection::select per spec §5.6, §9.1 item 7 [M-F2I])
pub(crate) fn percentile<T: selection::Interface + ?Sized>(
    data: &mut T,
    percent: isize,
    rng_seed: Option<u64>,
) -> isize {
    // Ordinal rank k = Ceil(P / 100 * N)
    let mut k = f64_to_i64(((data.len() as f64) * ((percent as f64) / 100.0)).ceil()) as isize;
    if k > data.len() {
        k = data.len();
    }
    match rng_seed {
        Some(seed) => {
            let mut rng = SeededRng(seed);
            selection::select(data, k, &mut || rng.next_nonneg())
        }
        None => {
            let mut rng = rand::thread_rng();
            selection::select(data, k, &mut || rng.r#gen::<usize>() & (usize::MAX >> 1))
        }
    }
}

/// basePercentile.
#[derive(Clone, Debug)]
pub struct BasePercentile {
    pub percent: isize,
    pub base: BaseAggFunc,
    /// `None` in production (uses `rand::thread_rng()`); `Some(seed)` in tests/govec.
    rng_seed: Option<u64>,
}

impl BasePercentile {
    // glue: Go struct literal basePercentile{percent, baseAggFunc{...}}
    pub fn new(percent: isize, base: BaseAggFunc) -> Self {
        BasePercentile {
            percent,
            base,
            rng_seed: None,
        }
    }
}

impl AggFunc for BasePercentile {
    // go: basePercentile.AllocPartialResult L2 (nil PartialResult → Box::new(()); memDelta dropped, A1)
    fn alloc_partial_result(&self) -> PartialResult {
        Box::new(())
    }

    // go: basePercentile.ResetPartialResult L1
    fn reset_partial_result(&self, _pr: &mut PartialResult) {}

    // go: basePercentile.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        _st: &EvalState,
        _rows_in_group: &[Row<'_>],
        _pr: &mut PartialResult,
    ) -> Result<(), Error> {
        Ok(())
    }

    // go: basePercentile.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        _pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        chk.append_null(self.base.col());
        Ok(())
    }

    // glue: deterministic seed injection for tests/govec (spec §5.6, §9.1 item 7)
    fn set_rng_seed_for_test(&mut self, seed: u64) {
        self.rng_seed = Some(seed);
    }
}

#[derive(Debug)]
struct PartialResult4PercentileInt(AllocVec<i64, ColAlloc>);

impl PartialResult4PercentileInt {
    // go: partialResult4PercentileInt.Len L1
    fn len(&self) -> isize {
        self.0.len() as isize
    }

    // go: partialResult4PercentileInt.Swap L1
    fn swap(&mut self, i: isize, j: isize) {
        self.0.swap(i as usize, j as usize);
    }

    // go: partialResult4PercentileInt.Less L1
    fn less(&self, i: isize, j: isize) -> bool {
        self.0[i as usize] < self.0[j as usize]
    }
}

impl selection::Interface for PartialResult4PercentileInt {
    // glue: selection::Interface adapter -> partialResult4PercentileInt.Len
    fn len(&self) -> isize {
        PartialResult4PercentileInt::len(self)
    }

    // glue: selection::Interface adapter -> partialResult4PercentileInt.Less
    fn less(&self, i: isize, j: isize) -> bool {
        PartialResult4PercentileInt::less(self, i, j)
    }

    // glue: selection::Interface adapter -> partialResult4PercentileInt.Swap
    fn swap(&mut self, i: isize, j: isize) {
        PartialResult4PercentileInt::swap(self, i, j)
    }
}

#[derive(Debug)]
struct PartialResult4PercentileReal(AllocVec<f64, ColAlloc>);

impl PartialResult4PercentileReal {
    // go: partialResult4PercentileReal.Len L1
    fn len(&self) -> isize {
        self.0.len() as isize
    }

    // go: partialResult4PercentileReal.Swap L1
    fn swap(&mut self, i: isize, j: isize) {
        self.0.swap(i as usize, j as usize);
    }

    // go: partialResult4PercentileReal.Less L1
    fn less(&self, i: isize, j: isize) -> bool {
        self.0[i as usize] < self.0[j as usize]
    }
}

impl selection::Interface for PartialResult4PercentileReal {
    // glue: selection::Interface adapter -> partialResult4PercentileReal.Len
    fn len(&self) -> isize {
        PartialResult4PercentileReal::len(self)
    }

    // glue: selection::Interface adapter -> partialResult4PercentileReal.Less
    fn less(&self, i: isize, j: isize) -> bool {
        PartialResult4PercentileReal::less(self, i, j)
    }

    // glue: selection::Interface adapter -> partialResult4PercentileReal.Swap
    fn swap(&mut self, i: isize, j: isize) {
        PartialResult4PercentileReal::swap(self, i, j)
    }
}

#[derive(Debug)]
struct PartialResult4PercentileDecimal(AllocVec<MyDecimal, ColAlloc>);

impl PartialResult4PercentileDecimal {
    // go: partialResult4PercentileDecimal.Len L1
    fn len(&self) -> isize {
        self.0.len() as isize
    }

    // go: partialResult4PercentileDecimal.Swap L1
    fn swap(&mut self, i: isize, j: isize) {
        self.0.swap(i as usize, j as usize);
    }

    // go: partialResult4PercentileDecimal.Less L1
    fn less(&self, i: isize, j: isize) -> bool {
        self.0[i as usize].compare(&self.0[j as usize]) < 0
    }
}

impl selection::Interface for PartialResult4PercentileDecimal {
    // glue: selection::Interface adapter -> partialResult4PercentileDecimal.Len
    fn len(&self) -> isize {
        PartialResult4PercentileDecimal::len(self)
    }

    // glue: selection::Interface adapter -> partialResult4PercentileDecimal.Less
    fn less(&self, i: isize, j: isize) -> bool {
        PartialResult4PercentileDecimal::less(self, i, j)
    }

    // glue: selection::Interface adapter -> partialResult4PercentileDecimal.Swap
    fn swap(&mut self, i: isize, j: isize) {
        PartialResult4PercentileDecimal::swap(self, i, j)
    }
}

#[derive(Debug)]
struct PartialResult4PercentileTime(AllocVec<Time, ColAlloc>);

impl PartialResult4PercentileTime {
    // go: partialResult4PercentileTime.Len L1
    fn len(&self) -> isize {
        self.0.len() as isize
    }

    // go: partialResult4PercentileTime.Swap L1
    fn swap(&mut self, i: isize, j: isize) {
        self.0.swap(i as usize, j as usize);
    }

    // go: partialResult4PercentileTime.Less L1
    fn less(&self, i: isize, j: isize) -> bool {
        self.0[i as usize].compare(&self.0[j as usize]) < 0
    }
}

impl selection::Interface for PartialResult4PercentileTime {
    // glue: selection::Interface adapter -> partialResult4PercentileTime.Len
    fn len(&self) -> isize {
        PartialResult4PercentileTime::len(self)
    }

    // glue: selection::Interface adapter -> partialResult4PercentileTime.Less
    fn less(&self, i: isize, j: isize) -> bool {
        PartialResult4PercentileTime::less(self, i, j)
    }

    // glue: selection::Interface adapter -> partialResult4PercentileTime.Swap
    fn swap(&mut self, i: isize, j: isize) {
        PartialResult4PercentileTime::swap(self, i, j)
    }
}

#[derive(Debug)]
struct PartialResult4PercentileDuration(AllocVec<Duration, ColAlloc>);

impl PartialResult4PercentileDuration {
    // go: partialResult4PercentileDuration.Len L1
    fn len(&self) -> isize {
        self.0.len() as isize
    }

    // go: partialResult4PercentileDuration.Swap L1
    fn swap(&mut self, i: isize, j: isize) {
        self.0.swap(i as usize, j as usize);
    }

    // go: partialResult4PercentileDuration.Less L1
    fn less(&self, i: isize, j: isize) -> bool {
        self.0[i as usize].compare(self.0[j as usize]) < 0
    }
}

impl selection::Interface for PartialResult4PercentileDuration {
    // glue: selection::Interface adapter -> partialResult4PercentileDuration.Len
    fn len(&self) -> isize {
        PartialResult4PercentileDuration::len(self)
    }

    // glue: selection::Interface adapter -> partialResult4PercentileDuration.Less
    fn less(&self, i: isize, j: isize) -> bool {
        PartialResult4PercentileDuration::less(self, i, j)
    }

    // glue: selection::Interface adapter -> partialResult4PercentileDuration.Swap
    fn swap(&mut self, i: isize, j: isize) {
        PartialResult4PercentileDuration::swap(self, i, j)
    }
}

/// percentileOriginal4Int.
#[derive(Clone, Debug)]
pub struct PercentileOriginal4Int {
    pub base: BasePercentile,
    alloc: ColAlloc,
}

impl PercentileOriginal4Int {
    // glue: Go struct literal percentileOriginal4Int{base}
    pub fn new(base: BasePercentile) -> Self {
        PercentileOriginal4Int {
            base,
            alloc: ColAlloc::Global,
        }
    }
}

impl AggFunc for PercentileOriginal4Int {
    // go: percentileOriginal4Int.AllocPartialResult L2 (memDelta dropped, A1; slice in ColAlloc, spec §5.3)
    fn alloc_partial_result(&self) -> PartialResult {
        Box::new(PartialResult4PercentileInt(AllocVec::new_in(
            self.alloc.clone(),
        )))
    }

    // go: percentileOriginal4Int.ResetPartialResult L2 (reinitializes slice in ColAlloc, spec §5.3)
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4PercentileInt>(pr);
        p.0 = AllocVec::new_in(self.alloc.clone());
    }

    // go: percentileOriginal4Int.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4PercentileInt>(pr);
        for &row in rows_in_group {
            let Some(v) = self.base.base.args[0].eval_int(sctx, st, row)? else {
                continue;
            };
            p.0.push(v);
        }
        Ok(())
    }

    // go: percentileOriginal4Int.MergePartialResult L2 (memDelta dropped, A1; drains src into dst's ColAlloc, spec §5.1)
    fn merge_partial_result(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        src: &mut PartialResult,
        dst: &mut PartialResult,
    ) -> Result<(), Error> {
        let p1 = partial_result_mut::<PartialResult4PercentileInt>(src);
        let p2 = partial_result_mut::<PartialResult4PercentileInt>(dst);
        p2.0.extend_from_slice(&p1.0);
        p1.0 = AllocVec::new_in(self.alloc.clone());
        Ok(())
    }

    // go: percentileOriginal4Int.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4PercentileInt>(pr);
        if p.0.is_empty() {
            chk.append_null(self.base.base.col());
            return Ok(());
        }
        let index = percentile(p, self.base.percent, self.base.rng_seed);
        chk.append_int64(self.base.base.col(), p.0[index as usize]);
        Ok(())
    }

    // glue: A1 3.3 TrackedAlloc of the percentile buffer (spec §5.3)
    fn set_alloc(&mut self, alloc: ColAlloc) {
        self.alloc = alloc;
    }

    // glue: deterministic seed injection for tests/govec (spec §5.6, §9.1 item 7)
    fn set_rng_seed_for_test(&mut self, seed: u64) {
        self.base.rng_seed = Some(seed);
    }
}

/// percentileOriginal4Real.
#[derive(Clone, Debug)]
pub struct PercentileOriginal4Real {
    pub base: BasePercentile,
    alloc: ColAlloc,
}

impl PercentileOriginal4Real {
    // glue: Go struct literal percentileOriginal4Real{base}
    pub fn new(base: BasePercentile) -> Self {
        PercentileOriginal4Real {
            base,
            alloc: ColAlloc::Global,
        }
    }
}

impl AggFunc for PercentileOriginal4Real {
    // go: percentileOriginal4Real.AllocPartialResult L2 (memDelta dropped, A1; slice in ColAlloc, spec §5.3)
    fn alloc_partial_result(&self) -> PartialResult {
        Box::new(PartialResult4PercentileReal(AllocVec::new_in(
            self.alloc.clone(),
        )))
    }

    // go: percentileOriginal4Real.ResetPartialResult L2 (reinitializes slice in ColAlloc, spec §5.3)
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4PercentileReal>(pr);
        p.0 = AllocVec::new_in(self.alloc.clone());
    }

    // go: percentileOriginal4Real.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4PercentileReal>(pr);
        for &row in rows_in_group {
            let Some(v) = self.base.base.args[0].eval_real(sctx, st, row)? else {
                continue;
            };
            p.0.push(v);
        }
        Ok(())
    }

    // go: percentileOriginal4Real.MergePartialResult L2 (memDelta dropped, A1; drains src into dst's ColAlloc, spec §5.1)
    fn merge_partial_result(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        src: &mut PartialResult,
        dst: &mut PartialResult,
    ) -> Result<(), Error> {
        let p1 = partial_result_mut::<PartialResult4PercentileReal>(src);
        let p2 = partial_result_mut::<PartialResult4PercentileReal>(dst);
        p2.0.extend_from_slice(&p1.0);
        p1.0 = AllocVec::new_in(self.alloc.clone());
        Ok(())
    }

    // go: percentileOriginal4Real.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4PercentileReal>(pr);
        if p.0.is_empty() {
            chk.append_null(self.base.base.col());
            return Ok(());
        }
        let index = percentile(p, self.base.percent, self.base.rng_seed);
        chk.append_float64(self.base.base.col(), p.0[index as usize]);
        Ok(())
    }

    // glue: A1 3.3 TrackedAlloc of the percentile buffer (spec §5.3)
    fn set_alloc(&mut self, alloc: ColAlloc) {
        self.alloc = alloc;
    }

    // glue: deterministic seed injection for tests/govec (spec §5.6, §9.1 item 7)
    fn set_rng_seed_for_test(&mut self, seed: u64) {
        self.base.rng_seed = Some(seed);
    }
}

/// percentileOriginal4Decimal.
#[derive(Clone, Debug)]
pub struct PercentileOriginal4Decimal {
    pub base: BasePercentile,
    alloc: ColAlloc,
}

impl PercentileOriginal4Decimal {
    // glue: Go struct literal percentileOriginal4Decimal{base}
    pub fn new(base: BasePercentile) -> Self {
        PercentileOriginal4Decimal {
            base,
            alloc: ColAlloc::Global,
        }
    }
}

impl AggFunc for PercentileOriginal4Decimal {
    // go: percentileOriginal4Decimal.AllocPartialResult L2 (memDelta dropped, A1; slice in ColAlloc, spec §5.3)
    fn alloc_partial_result(&self) -> PartialResult {
        Box::new(PartialResult4PercentileDecimal(AllocVec::new_in(
            self.alloc.clone(),
        )))
    }

    // go: percentileOriginal4Decimal.ResetPartialResult L2 (reinitializes slice in ColAlloc, spec §5.3)
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4PercentileDecimal>(pr);
        p.0 = AllocVec::new_in(self.alloc.clone());
    }

    // go: percentileOriginal4Decimal.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4PercentileDecimal>(pr);
        for &row in rows_in_group {
            let Some(v) = self.base.base.args[0].eval_decimal(sctx, st, row)? else {
                continue;
            };
            p.0.push(v);
        }
        Ok(())
    }

    // go: percentileOriginal4Decimal.MergePartialResult L2 (memDelta dropped, A1; drains src into dst's ColAlloc, spec §5.1)
    fn merge_partial_result(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        src: &mut PartialResult,
        dst: &mut PartialResult,
    ) -> Result<(), Error> {
        let p1 = partial_result_mut::<PartialResult4PercentileDecimal>(src);
        let p2 = partial_result_mut::<PartialResult4PercentileDecimal>(dst);
        p2.0.extend_from_slice(&p1.0);
        p1.0 = AllocVec::new_in(self.alloc.clone());
        Ok(())
    }

    // go: percentileOriginal4Decimal.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4PercentileDecimal>(pr);
        if p.0.is_empty() {
            chk.append_null(self.base.base.col());
            return Ok(());
        }
        let index = percentile(p, self.base.percent, self.base.rng_seed);
        chk.append_my_decimal(self.base.base.col(), &p.0[index as usize]);
        Ok(())
    }

    // glue: A1 3.3 TrackedAlloc of the percentile buffer (spec §5.3)
    fn set_alloc(&mut self, alloc: ColAlloc) {
        self.alloc = alloc;
    }

    // glue: deterministic seed injection for tests/govec (spec §5.6, §9.1 item 7)
    fn set_rng_seed_for_test(&mut self, seed: u64) {
        self.base.rng_seed = Some(seed);
    }
}

/// percentileOriginal4Time.
#[derive(Clone, Debug)]
pub struct PercentileOriginal4Time {
    pub base: BasePercentile,
    alloc: ColAlloc,
}

impl PercentileOriginal4Time {
    // glue: Go struct literal percentileOriginal4Time{base}
    pub fn new(base: BasePercentile) -> Self {
        PercentileOriginal4Time {
            base,
            alloc: ColAlloc::Global,
        }
    }
}

impl AggFunc for PercentileOriginal4Time {
    // go: percentileOriginal4Time.AllocPartialResult L2 (memDelta dropped, A1; slice in ColAlloc, spec §5.3)
    fn alloc_partial_result(&self) -> PartialResult {
        Box::new(PartialResult4PercentileTime(AllocVec::new_in(
            self.alloc.clone(),
        )))
    }

    // go: percentileOriginal4Time.ResetPartialResult L2 (reinitializes slice in ColAlloc, spec §5.3)
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4PercentileTime>(pr);
        p.0 = AllocVec::new_in(self.alloc.clone());
    }

    // go: percentileOriginal4Time.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4PercentileTime>(pr);
        for &row in rows_in_group {
            let Some(v) = self.base.base.args[0].eval_time(sctx, st, row)? else {
                continue;
            };
            p.0.push(v);
        }
        Ok(())
    }

    // go: percentileOriginal4Time.MergePartialResult L2 (memDelta dropped, A1; drains src into dst's ColAlloc, spec §5.1)
    fn merge_partial_result(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        src: &mut PartialResult,
        dst: &mut PartialResult,
    ) -> Result<(), Error> {
        let p1 = partial_result_mut::<PartialResult4PercentileTime>(src);
        let p2 = partial_result_mut::<PartialResult4PercentileTime>(dst);
        p2.0.extend_from_slice(&p1.0);
        p1.0 = AllocVec::new_in(self.alloc.clone());
        Ok(())
    }

    // go: percentileOriginal4Time.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4PercentileTime>(pr);
        if p.0.is_empty() {
            chk.append_null(self.base.base.col());
            return Ok(());
        }
        let index = percentile(p, self.base.percent, self.base.rng_seed);
        chk.append_time(self.base.base.col(), p.0[index as usize]);
        Ok(())
    }

    // glue: A1 3.3 TrackedAlloc of the percentile buffer (spec §5.3)
    fn set_alloc(&mut self, alloc: ColAlloc) {
        self.alloc = alloc;
    }

    // glue: deterministic seed injection for tests/govec (spec §5.6, §9.1 item 7)
    fn set_rng_seed_for_test(&mut self, seed: u64) {
        self.base.rng_seed = Some(seed);
    }
}

/// percentileOriginal4Duration.
#[derive(Clone, Debug)]
pub struct PercentileOriginal4Duration {
    pub base: BasePercentile,
    alloc: ColAlloc,
}

impl PercentileOriginal4Duration {
    // glue: Go struct literal percentileOriginal4Duration{base}
    pub fn new(base: BasePercentile) -> Self {
        PercentileOriginal4Duration {
            base,
            alloc: ColAlloc::Global,
        }
    }
}

impl AggFunc for PercentileOriginal4Duration {
    // go: percentileOriginal4Duration.AllocPartialResult L2 (Go allocates an empty partialResult4PercentileTime slice header and casts via unsafe.Pointer; Rust allocates PartialResult4PercentileDuration in ColAlloc for type-safe Box<dyn Any>, memDelta dropped, A1)
    fn alloc_partial_result(&self) -> PartialResult {
        Box::new(PartialResult4PercentileDuration(AllocVec::new_in(
            self.alloc.clone(),
        )))
    }

    // go: percentileOriginal4Duration.ResetPartialResult L2 (reinitializes slice in ColAlloc, spec §5.3)
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4PercentileDuration>(pr);
        p.0 = AllocVec::new_in(self.alloc.clone());
    }

    // go: percentileOriginal4Duration.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4PercentileDuration>(pr);
        for &row in rows_in_group {
            let Some(v) = self.base.base.args[0].eval_duration(sctx, st, row)? else {
                continue;
            };
            p.0.push(v);
        }
        Ok(())
    }

    // go: percentileOriginal4Duration.MergePartialResult L2 (memDelta dropped, A1; drains src into dst's ColAlloc, spec §5.1)
    fn merge_partial_result(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        src: &mut PartialResult,
        dst: &mut PartialResult,
    ) -> Result<(), Error> {
        let p1 = partial_result_mut::<PartialResult4PercentileDuration>(src);
        let p2 = partial_result_mut::<PartialResult4PercentileDuration>(dst);
        p2.0.extend_from_slice(&p1.0);
        p1.0 = AllocVec::new_in(self.alloc.clone());
        Ok(())
    }

    // go: percentileOriginal4Duration.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4PercentileDuration>(pr);
        if p.0.is_empty() {
            chk.append_null(self.base.base.col());
            return Ok(());
        }
        let index = percentile(p, self.base.percent, self.base.rng_seed);
        chk.append_duration(self.base.base.col(), p.0[index as usize]);
        Ok(())
    }

    // glue: A1 3.3 TrackedAlloc of the percentile buffer (spec §5.3)
    fn set_alloc(&mut self, alloc: ColAlloc) {
        self.alloc = alloc;
    }

    // glue: deterministic seed injection for tests/govec (spec §5.6, §9.1 item 7)
    fn set_rng_seed_for_test(&mut self, seed: u64) {
        self.base.rng_seed = Some(seed);
    }
}
