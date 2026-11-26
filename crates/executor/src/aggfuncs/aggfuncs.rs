//! go-file: pkg/executor/aggfuncs/aggfuncs.go
//!
//! The aggregate function interface and its partial results (spec D12.4 §5.1):
//! - Go's `PartialResult` is an `unsafe.Pointer` each implementation casts to its own
//!   struct; Rust boxes the struct as `dyn Any` and each implementation downcasts it
//!   ([`partial_result_mut`]). A partial result built by one function and read by another
//!   (the partial and the final function of one split descriptor) must have the same type;
//!   Go relies on the construction, Rust panics on a mismatch (tested for each split pair).
//! - The methods take `&self`: one function object is shared by the partial workers of the
//!   parallel hash aggregation (`pkg/executor/aggregate/agg_hash_executor.go:312`).
//! - No method returns Go's `memDelta` (spec §9 item 1, the memory exception A1): the
//!   growing containers of the partial results allocate through the allocator handed over
//!   with [`AggFunc::set_alloc`], which accounts their real size (L2).
//! - Row-by-row evaluation needs the per-execution state of the expressions (correlated
//!   column values, N4 §3.4), so `UpdatePartialResult` takes an [`EvalState`] besides the
//!   evaluation context (L2).
//! - The spill serialization (`serializer`, part S of the block) is not ported yet; the
//!   trait has no methods for it, so nothing can call an empty implementation (spec §5.4).
//! - The sliding-window interfaces ([`SlidingWindowAggFunc`], [`MaxMinSlidingWindowAggFunc`])
//!   are ported in part W and accessed through [`AggFunc::as_sliding_window`] and
//!   [`AggFunc::as_max_min_sliding_mut`].

use std::any::Any;
use std::sync::Arc;
use std::sync::atomic::AtomicI32;

use allocator_api2::alloc::{Allocator, Global};
use allocator_api2::boxed::Box as AllocBox;
use allocator_api2::vec::Vec as AllocVec;
use tidb_base::Error;
use tidb_chunk::ColAlloc;
use tidb_chunk::chunk::{Chunk, renew_with_capacity_in};
use tidb_chunk::row::{Row, RowHandle};
use tidb_expression::expression::{EvalState, ExprRef};
use tidb_kv::hack::{MemAwareMap, new_mem_aware_map_in};
use tidb_parser::types::FieldType;

pub use tidb_expression::context::EvalContext as AggFuncUpdateContext;

/// PartialResult represents data structure to store the partial result for the aggregate
/// functions (Go: an `unsafe.Pointer` to any type; Rust: a boxed `Any`, spec §9 item 2).
pub type PartialResult = Box<dyn Any + Send>;

/// AggPartialResultMapper contains aggregate function results (keys are Go strings, i.e.
/// bytes, M-BYTES; allocated in `A` for A1 real memory accounting).
pub type AggPartialResultMapper<A = Global> =
    MemAwareMap<AllocBox<[u8], A>, AllocVec<PartialResult, A>, A>;

/// NewAggPartialResultMapper creates a new AggPartialResultMapper
// go: NewAggPartialResultMapper L1
pub fn new_agg_partial_result_mapper() -> AggPartialResultMapper {
    new_agg_partial_result_mapper_with_cap(0)
}

/// NewAggPartialResultMapperWithCap creates a new AggPartialResultMapper with specified
/// capacity. Only capacity 0 is supported, the only capacity Go's callers pass
/// (`hack.NewMemAwareMap`, see `tidb_kv::hack`).
// go: NewAggPartialResultMapperWithCap L1
pub fn new_agg_partial_result_mapper_with_cap(capacity: isize) -> AggPartialResultMapper {
    new_agg_partial_result_mapper_in(Global, capacity)
}

/// NewAggPartialResultMapper in an allocator `A` (A1 3.3).
// glue: NewAggPartialResultMapper in an allocator (A1 3.3)
pub fn new_agg_partial_result_mapper_in<A: Allocator + Clone>(
    alloc: A,
    capacity: isize,
) -> AggPartialResultMapper<A> {
    new_mem_aware_map_in(capacity, alloc)
}

/// AggFunc is the interface to evaluate the aggregate functions.
pub trait AggFunc: Send + Sync {
    /// AllocPartialResult allocates a specific data structure to store the partial result,
    /// initializes it, and converts it to PartialResult to return back. Aggregate operator
    /// implementation, no matter it's a hash or stream, should hold this allocated
    /// PartialResult for the further operations like: "ResetPartialResult",
    /// "UpdatePartialResult".
    // glue: interface method of AggFunc (Go also returns memDelta, dropped: A1, spec §5.3)
    fn alloc_partial_result(&self) -> PartialResult;

    /// ResetPartialResult resets the partial result to the original state for a specific
    /// aggregate function.
    // glue: interface method of AggFunc
    fn reset_partial_result(&self, pr: &mut PartialResult);

    /// UpdatePartialResult updates the specific partial result for an aggregate function
    /// using the input rows which all belonging to the same data group.
    // glue: interface method of AggFunc (Go also returns memDelta, dropped: A1, spec §5.1;
    // `st` is the expressions' per-execution state, N4 §3.4)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error>;

    /// Zero-copy [`RowHandle`] ingestion for callers that already hold `Arc<Chunk>` rows
    /// (such as the window executor, M-ROW spec §5.2). Functions that store rows (`RANK`,
    /// `PERCENT_RANK`, `CUME_DIST`, `LEAD`, `LAG`) clone the handles without copying cells;
    /// all other functions borrow the rows and delegate to [`Self::update_partial_result`].
    // glue: M-ROW zero-copy RowHandle ingestion for WindowExec (spec §5.2)
    fn update_partial_result_handles(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[RowHandle],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let rows: Vec<Row<'_>> = rows_in_group.iter().map(RowHandle::row).collect();
        self.update_partial_result(sctx, st, &rows, pr)
    }

    /// MergePartialResult will be called in the final phase when parallelly executing. It
    /// converts the PartialResult `src`, `dst` to the same specific data structure which
    /// stores the partial results, and then evaluate the final result using the partial
    /// results as input values. `src` is not used by the caller afterwards (spec §5.1).
    ///
    /// The default is `baseAggFunc`'s, which does nothing.
    // go: baseAggFunc.MergePartialResult L2 (memDelta dropped, A1 spec §5.3)
    fn merge_partial_result(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        _src: &mut PartialResult,
        _dst: &mut PartialResult,
    ) -> Result<(), Error> {
        Ok(())
    }

    /// AppendFinalResult2Chunk finalizes the partial result and append the final result to
    /// the input chunk. `pr` is mutable: DECIMAL results are rounded in place, as in Go.
    // glue: interface method of AggFunc
    fn append_final_result2_chunk(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error>;

    /// Hands over the allocator of the growing containers of the partial results allocated
    /// from now on (the DISTINCT sets; spec §5.3). The aggregation executor calls it after
    /// [`super::build`] while it owns the function; without it they use the global
    /// allocator. Functions whose partial results do not grow ignore it.
    // glue: A1 3.3 TrackedAlloc of the partial results (spec §5.3), no Go counterpart
    fn set_alloc(&mut self, _alloc: ColAlloc) {}

    /// Go's `GetTruncated() *int32` duck-typed interface on `GROUP_CONCAT` functions
    /// (`builder.go:2084-2089`, spec §5.1). Implemented by `groupConcat`, `groupConcatDistinct`,
    /// and `groupConcatOrder` (not `groupConcatDistinctOrder`, matching Go).
    // glue: M-IFACE duck-typed GetTruncated on AggFunc (spec §5.1)
    fn get_truncated(&self) -> Option<Arc<AtomicI32>> {
        None
    }

    /// Go's `SetTruncated(*int32)` duck-typed interface on `GROUP_CONCAT` functions
    /// (`builder.go:2084-2089`, spec §5.1). Implemented by `groupConcat`, `groupConcatDistinct`,
    /// and `groupConcatOrder` (not `groupConcatDistinctOrder`, matching Go).
    // glue: M-IFACE duck-typed SetTruncated on AggFunc (spec §5.1)
    fn set_truncated(&mut self, _t: Arc<AtomicI32>) {}

    /// Injects a deterministic PRNG seed for `APPROX_PERCENTILE`'s `selection::select` pivot
    /// in unit tests and `govec` replay (spec D12.4 §5.6, §9.1 item 7). Production callers
    /// leave it unset so `APPROX_PERCENTILE` uses `rand::thread_rng()`.
    // glue: deterministic seed injection for APPROX_PERCENTILE tests/govec (spec §5.6, §9.1 item 7)
    fn set_rng_seed_for_test(&mut self, _seed: u64) {}

    /// Go's `agg.(SlidingWindowAggFunc)` type assertion (spec §5.1).
    // glue: M-IFACE type assertion to SlidingWindowAggFunc (spec §5.1)
    fn as_sliding_window(&self) -> Option<&dyn SlidingWindowAggFunc> {
        None
    }

    /// Go's `agg.(MaxMinSlidingWindowAggFunc)` type assertion (spec §5.1).
    // glue: M-IFACE type assertion to MaxMinSlidingWindowAggFunc (spec §5.1)
    fn as_max_min_sliding_mut(&mut self) -> Option<&mut dyn MaxMinSlidingWindowAggFunc> {
        None
    }

    /// The implementation's type name (Go's `%T` of the built function, read by the
    /// differential vectors and the tests).
    // glue: the implementation's type name, no Go counterpart
    fn type_name(&self) -> &'static str {
        std::any::type_name::<Self>()
    }
}

/// `SlidingWindowAggFunc` is the interface to evaluate aggregate functions using a sliding
/// window (part W of D12.4, spec §5.1).
pub trait SlidingWindowAggFunc {
    /// `Slide` evaluates the aggregate function using a sliding window. `last_start` and
    /// `last_end` are the interval of the former sliding window, and `shift_start` /
    /// `shift_end` are the sliding window offsets.
    // glue: interface method of SlidingWindowAggFunc (`st` added, N4 §3.4)
    #[allow(clippy::too_many_arguments)]
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
    ) -> Result<(), Error>;
}

/// `MaxMinSlidingWindowAggFunc` is the interface to evaluate the max/min agg function using
/// a sliding window (part W of D12.4, spec §5.1).
pub trait MaxMinSlidingWindowAggFunc {
    /// `SetWindowStart` sets the start position of the window.
    // glue: interface method of MaxMinSlidingWindowAggFunc
    fn set_window_start(&mut self, start: u64);
}

/// baseAggFunc, embedded in every implementation (M-SELF: a field of the implementations).
#[derive(Clone, Debug)]
pub struct BaseAggFunc {
    /// args stores the input arguments for an aggregate function, we should call
    /// arg.EvalXXX to get the actual input data for this function.
    pub args: Vec<ExprRef>,

    /// ordinal stores the ordinal of the columns in the output chunk, which is used to
    /// append the final result of this function.
    pub ordinal: isize,

    /// retTp means the target type of the final agg should return (`None`: Go's nil; the
    /// bit functions' builders do not set it).
    pub ret_tp: Option<Arc<FieldType>>,
}

impl BaseAggFunc {
    /// The output column of the final result (`e.ordinal` as a chunk column index).
    // glue: Go's int ordinal as a column index
    #[inline]
    pub fn col(&self) -> usize {
        self.ordinal as usize
    }
}

/// The partial result as the implementation's struct: Go's `(*partialResult4Xxx)(pr)`.
///
/// # Panics
///
/// When the partial result was allocated by an aggregate function of another kind, which
/// Go's construction excludes (module docs).
// glue: Go's unsafe.Pointer cast of a PartialResult (spec §5.1)
#[inline]
pub(crate) fn partial_result_mut<T: Any>(pr: &mut PartialResult) -> &mut T {
    match (**pr).downcast_mut::<T>() {
        Some(p) => p,
        None => panic!(
            "aggregate function partial result is not a {}",
            std::any::type_name::<T>()
        ),
    }
}

/// Copies `bytes` into a boxed slice allocated through `alloc` (A1 3.3).
// glue: A1 3.3 TrackedAlloc boxed byte slice helper
pub(crate) fn box_bytes_in(bytes: &[u8], alloc: ColAlloc) -> AllocBox<[u8], ColAlloc> {
    let mut v = AllocVec::with_capacity_in(bytes.len(), alloc);
    v.extend_from_slice(bytes);
    v.into_boxed_slice()
}

/// Borrows the [`Row`] from an optional [`RowHandle`] (`None` corresponds to Go's zero
/// `chunk.Row{}`).
// glue: M-ROW borrow helper for stored window rows
#[inline]
pub(crate) fn row_of(h: &Option<RowHandle>) -> Row<'_> {
    match h {
        Some(handle) => handle.row(),
        None => Row::default(),
    }
}

/// Copies borrowed `rows` into an `alloc`-backed `Arc<Chunk>` and appends their
/// [`RowHandle`]s to `dst` (M-ROW + A1 3.3).
// glue: M-ROW / A1 3.3 copy borrowed rows into ColAlloc-backed Arc<Chunk> for window partial results
pub(crate) fn push_rows_in(
    dst: &mut AllocVec<Option<RowHandle>, ColAlloc>,
    rows: &[Row<'_>],
    alloc: &ColAlloc,
) {
    if rows.is_empty() {
        return;
    }
    let Some(first_non_empty) = rows.iter().find(|r| !r.is_empty()) else {
        dst.extend(std::iter::repeat_n(None, rows.len()));
        return;
    };
    let non_empty_count = rows.iter().filter(|r| !r.is_empty()).count();
    let mut owned = renew_with_capacity_in(
        first_non_empty.chunk(),
        non_empty_count,
        non_empty_count,
        alloc.clone(),
    );
    let mut indices = Vec::with_capacity(rows.len());
    for &row in rows {
        if row.is_empty() {
            indices.push(None);
        } else {
            let idx = owned.num_rows();
            owned.append_row(row);
            indices.push(Some(idx));
        }
    }
    let arc_chk = Arc::new(owned);
    dst.reserve(rows.len());
    for idx in indices {
        dst.push(idx.map(|i| RowHandle::new(Arc::clone(&arc_chk), i)));
    }
}
