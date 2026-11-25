//! go-file: pkg/executor/aggfuncs/func_group_concat.go
//!
//! `GROUP_CONCAT` (part B1 of D12.4: `groupConcat`, `groupConcatDistinct`, `groupConcatOrder`,
//! `groupConcatDistinctOrder`, and the `topNRows` max-heap).
//!
//! Growing buffers, DISTINCT sets, and `topNRows` storage allocate through
//! [`AggFunc::set_alloc`] (A1, spec §5.3). Go's manual `GetDatumMemSize` is omitted under
//! exception A1 (spec §3.6, §9 item 1). The spill serialization methods on `groupConcat`
//! (`SerializePartialResult`, `DeserializePartialResult`, `deserializeForSpill`) belong to
//! part S.

// go-exception: A1 GetDatumMemSize (only computes Go's manual memDelta for GROUP_CONCAT ORDER BY;
// Rust accounts the growing partial result containers and SortRow.by_items Datum payloads through
// ColAlloc / TrackedAlloc, spec §3.6)

use std::borrow::Cow;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

use allocator_api2::vec::Vec as AllocVec;
use tidb_base::Error;
use tidb_base::dbterror::plannererrors;
use tidb_base::errors::RedactLogDisable;
use tidb_base::gofmt::Arg;
use tidb_base::gostd::heap::{
    self, Interface as HeapInterface, PushInterface as HeapPushInterface,
};
use tidb_base::gostd::sort::sort as go_sort;
use tidb_chunk::ColAlloc;
use tidb_chunk::chunk::Chunk;
use tidb_chunk::row::Row;
use tidb_codec::bytes::encode_bytes;
use tidb_expression::byitem::ByItems;
use tidb_expression::errors::ErrCutValueGroupConcat;
use tidb_expression::expression::EvalState;
use tidb_kv::set::{StringSetWithMemoryUsage, new_string_set_with_memory_usage};
use tidb_types::collate::{Collator, get_collator};
use tidb_types::context::Context as TypeContext;
use tidb_types::datum::Datum;

use super::aggfuncs::{
    AggFunc, AggFuncUpdateContext, BaseAggFunc, PartialResult, partial_result_mut,
};

/// baseGroupConcat4String.
#[derive(Clone)]
pub struct BaseGroupConcat4String {
    pub base: BaseAggFunc,
    pub by_items: Vec<ByItems>,

    pub sep: Vec<u8>,
    pub max_len: u64,
    // According to MySQL, a 'group_concat' function generates exactly one 'truncated' warning
    // during its life time, no matter how many group actually truncated. 'truncated' acts as a
    // sentinel to indicate whether this warning has already been generated.
    pub truncated: Arc<AtomicI32>,
}

/// Appends the `GROUP_CONCAT` string buffer (or NULL when empty) to `chk`.
// glue: shared body of baseGroupConcat4String.AppendFinalResult2Chunk for groupConcat and
// groupConcatDistinct (in Go both partial result structs share basePartialResult4GroupConcat
// at offset 0; Rust's Box<dyn Any> downcasts each concrete partial result type)
fn append_buffer_to_chunk(
    col: usize,
    buffer: Option<&AllocVec<u8, ColAlloc>>,
    chk: &mut Chunk,
) -> Result<(), Error> {
    let Some(buf) = buffer else {
        chk.append_null(col);
        return Ok(());
    };
    chk.append_string(col, buf);
    Ok(())
}

impl BaseGroupConcat4String {
    // go: baseGroupConcat4String.AppendFinalResult2Chunk L1
    pub fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4GroupConcat>(pr);
        append_buffer_to_chunk(self.base.col(), p.base.buffer.as_ref(), chk)
    }

    // go: baseGroupConcat4String.handleTruncateError L1
    fn handle_truncate_error(&self, ctx: &dyn AggFuncUpdateContext) -> Result<(), Error> {
        let tc = ctx.type_ctx();

        if self
            .truncated
            .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            let arg0 = Arg::Str(Cow::Owned(
                self.base.args[0].string_with_ctx(ctx, RedactLogDisable),
            ));
            if !tc.flags().truncate_as_warning() {
                return Err(ErrCutValueGroupConcat.gen_with_stack_by_args(vec![arg0]));
            }
            tc.append_warning(ErrCutValueGroupConcat.fast_gen_by_args(vec![arg0]));
        }
        Ok(())
    }

    // go: baseGroupConcat4String.truncatePartialResultIfNeed L1
    fn truncate_partial_result_if_need(
        &self,
        ctx: &dyn AggFuncUpdateContext,
        buffer: &mut AllocVec<u8, ColAlloc>,
    ) -> Result<(), Error> {
        if self.max_len > 0 && buffer.len() as u64 > self.max_len {
            buffer.truncate(self.max_len as usize);
            return self.handle_truncate_error(ctx);
        }
        Ok(())
    }
}

#[derive(Debug)]
struct BasePartialResult4GroupConcat {
    vals_buf: AllocVec<u8, ColAlloc>,
    buffer: Option<AllocVec<u8, ColAlloc>>,
}

#[derive(Debug)]
struct PartialResult4GroupConcat {
    base: BasePartialResult4GroupConcat,
}

/// groupConcat.
#[derive(Clone)]
pub struct GroupConcat {
    pub base: BaseGroupConcat4String,
    /// The allocator of the partial result buffers (A1, spec §5.3).
    alloc: ColAlloc,
}

impl GroupConcat {
    /// The function with buffers in the global allocator until [`AggFunc::set_alloc`].
    // glue: Go struct literal groupConcat{base}
    pub fn new(base: BaseGroupConcat4String) -> Self {
        GroupConcat {
            base,
            alloc: ColAlloc::Global,
        }
    }
}

impl AggFunc for GroupConcat {
    // go: groupConcat.AllocPartialResult L2 (memDelta dropped, A1; buffers in ColAlloc, spec §5.3)
    fn alloc_partial_result(&self) -> PartialResult {
        Box::new(PartialResult4GroupConcat {
            base: BasePartialResult4GroupConcat {
                vals_buf: AllocVec::new_in(self.alloc.clone()),
                buffer: None,
            },
        })
    }

    // go: groupConcat.ResetPartialResult L1
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4GroupConcat>(pr);
        p.base.buffer = None;
    }

    // glue: baseGroupConcat4String.AppendFinalResult2Chunk through the embedded struct (M-SELF)
    fn append_final_result2_chunk(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        self.base.append_final_result2_chunk(sctx, pr, chk)
    }

    // go: groupConcat.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4GroupConcat>(pr);
        let mut is_null = false;

        for &row in rows_in_group {
            p.base.vals_buf.clear();
            for arg in &self.base.base.args {
                match arg.eval_string(sctx, st, row)? {
                    Some(v) => {
                        is_null = false;
                        p.base.vals_buf.extend_from_slice(&v);
                    }
                    None => {
                        is_null = true;
                        break;
                    }
                }
            }
            if is_null {
                continue;
            }
            match &mut p.base.buffer {
                None => {
                    let mut buf = AllocVec::new_in(self.alloc.clone());
                    buf.extend_from_slice(&p.base.vals_buf);
                    p.base.buffer = Some(buf);
                }
                Some(buf) => {
                    buf.extend_from_slice(&self.base.sep);
                    buf.extend_from_slice(&p.base.vals_buf);
                }
            }
        }
        if let Some(buf) = &mut p.base.buffer {
            return self.base.truncate_partial_result_if_need(sctx, buf);
        }
        Ok(())
    }

    // go: groupConcat.MergePartialResult L2 (memDelta dropped, A1; src buffer moved when dst is empty, spec §7 item 5)
    fn merge_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        src: &mut PartialResult,
        dst: &mut PartialResult,
    ) -> Result<(), Error> {
        let p1 = partial_result_mut::<PartialResult4GroupConcat>(src);
        let p2 = partial_result_mut::<PartialResult4GroupConcat>(dst);
        if p1.base.buffer.is_none() {
            return Ok(());
        }
        if p2.base.buffer.is_none() {
            p2.base.buffer = p1.base.buffer.take();
            return Ok(());
        }
        let p1_buf = p1.base.buffer.as_ref().unwrap();
        let p2_buf = p2.base.buffer.as_mut().unwrap();
        p2_buf.extend_from_slice(&self.base.sep);
        p2_buf.extend_from_slice(p1_buf);
        self.base.truncate_partial_result_if_need(sctx, p2_buf)
    }

    // go: groupConcat.SetTruncated L2 (Arc<AtomicI32> for *int32, spec §5.1)
    fn set_truncated(&mut self, t: Arc<AtomicI32>) {
        self.base.truncated = t;
    }

    // go: groupConcat.GetTruncated L2 (Arc<AtomicI32> for *int32, spec §5.1)
    fn get_truncated(&self) -> Option<Arc<AtomicI32>> {
        Some(Arc::clone(&self.base.truncated))
    }

    // glue: A1 3.3 TrackedAlloc of the partial result buffers (spec §5.3)
    fn set_alloc(&mut self, alloc: ColAlloc) {
        self.alloc = alloc;
    }
}

#[derive(Debug)]
struct PartialResult4GroupConcatDistinct {
    base: BasePartialResult4GroupConcat,
    val_set: StringSetWithMemoryUsage<ColAlloc>,
    encode_bytes_buffer: Vec<u8>,
}

/// groupConcatDistinct.
#[derive(Clone)]
pub struct GroupConcatDistinct {
    pub base: BaseGroupConcat4String,
    /// The allocator of the partial result buffers and DISTINCT set (A1, spec §5.3).
    alloc: ColAlloc,
}

impl GroupConcatDistinct {
    /// The function with containers in the global allocator until [`AggFunc::set_alloc`].
    // glue: Go struct literal groupConcatDistinct{base}
    pub fn new(base: BaseGroupConcat4String) -> Self {
        GroupConcatDistinct {
            base,
            alloc: ColAlloc::Global,
        }
    }
}

impl AggFunc for GroupConcatDistinct {
    // go: groupConcatDistinct.AllocPartialResult L2 (memDelta dropped, A1; containers in ColAlloc, spec §5.3)
    fn alloc_partial_result(&self) -> PartialResult {
        let (val_set, _) = new_string_set_with_memory_usage(self.alloc.clone());
        Box::new(PartialResult4GroupConcatDistinct {
            base: BasePartialResult4GroupConcat {
                vals_buf: AllocVec::new_in(self.alloc.clone()),
                buffer: None,
            },
            val_set,
            encode_bytes_buffer: Vec::new(),
        })
    }

    // go: groupConcatDistinct.ResetPartialResult L2 (the set's allocator, spec §5.3)
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4GroupConcatDistinct>(pr);
        p.base.buffer = None;
        p.val_set = new_string_set_with_memory_usage(self.alloc.clone()).0;
    }

    // glue: baseGroupConcat4String.AppendFinalResult2Chunk through the embedded struct (M-SELF;
    // downcasts PartialResult4GroupConcatDistinct)
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4GroupConcatDistinct>(pr);
        append_buffer_to_chunk(self.base.base.col(), p.base.buffer.as_ref(), chk)
    }

    // go: groupConcatDistinct.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4GroupConcatDistinct>(pr);
        let mut is_null = false;

        let mut collators = Vec::with_capacity(self.base.base.args.len());
        for arg in &self.base.base.args {
            collators.push(get_collator(arg.get_type(sctx).get_collate()));
        }

        for &row in rows_in_group {
            p.base.vals_buf.clear();
            p.encode_bytes_buffer.clear();
            for (i, arg) in self.base.base.args.iter().enumerate() {
                match arg.eval_string(sctx, st, row)? {
                    Some(v) => {
                        is_null = false;
                        encode_bytes(&mut p.encode_bytes_buffer, &collators[i].key(&v));
                        p.base.vals_buf.extend_from_slice(&v);
                    }
                    None => {
                        is_null = true;
                        break;
                    }
                }
            }
            if is_null {
                continue;
            }
            if p.val_set.exist(&p.encode_bytes_buffer) {
                continue;
            }
            p.val_set.insert(&p.encode_bytes_buffer);
            // write separator & values
            match &mut p.base.buffer {
                None => {
                    let mut buf = AllocVec::new_in(self.alloc.clone());
                    buf.extend_from_slice(&p.base.vals_buf);
                    p.base.buffer = Some(buf);
                }
                Some(buf) => {
                    buf.extend_from_slice(&self.base.sep);
                    buf.extend_from_slice(&p.base.vals_buf);
                }
            }
        }
        if let Some(buf) = &mut p.base.buffer {
            return self.base.truncate_partial_result_if_need(sctx, buf);
        }
        Ok(())
    }

    // go: groupConcatDistinct.SetTruncated L2 (Arc<AtomicI32> for *int32, spec §5.1)
    fn set_truncated(&mut self, t: Arc<AtomicI32>) {
        self.base.truncated = t;
    }

    // go: groupConcatDistinct.GetTruncated L2 (Arc<AtomicI32> for *int32, spec §5.1)
    fn get_truncated(&self) -> Option<Arc<AtomicI32>> {
        Some(Arc::clone(&self.base.truncated))
    }

    // glue: A1 3.3 TrackedAlloc of the partial result containers (spec §5.3)
    fn set_alloc(&mut self, alloc: ColAlloc) {
        self.alloc = alloc;
    }
}

/// RAII wrapper that accounts a `Datum`'s owned heap payload (bytes/strings/JSON/vectors/
/// boxed decimals) on `ColAlloc::Tracked`'s tracker so heap insertions, evictions, resets, and
/// drops balance automatically (A1, spec §5.3).
#[derive(Debug)]
struct TrackedDatum {
    datum: Datum,
    payload_bytes: i64,
    alloc: ColAlloc,
}

impl TrackedDatum {
    // glue: A1 payload accounting for Datum owned by topNRows.SortRow.by_items
    fn new(datum: Datum, alloc: ColAlloc) -> Self {
        let mut bytes = datum.get_bytes().len();
        if datum.kind() == tidb_types::datum::KindMysqlDecimal {
            bytes += std::mem::size_of::<tidb_types::MyDecimal>();
        }
        let payload_bytes = bytes as i64;
        if payload_bytes > 0
            && let ColAlloc::Tracked(ref ta) = alloc
        {
            ta.tracker().record(payload_bytes);
        }
        Self {
            datum,
            payload_bytes,
            alloc,
        }
    }
}

impl Drop for TrackedDatum {
    // glue: release tracked Datum payload bytes on eviction, reset, or drop
    fn drop(&mut self) {
        if self.payload_bytes > 0
            && let ColAlloc::Tracked(ref ta) = self.alloc
        {
            ta.tracker().record(-self.payload_bytes);
        }
    }
}

#[derive(Debug)]
struct SortRow {
    buffer: AllocVec<u8, ColAlloc>,
    by_items: AllocVec<TrackedDatum, ColAlloc>,
}

/// Compares two `SortRow`s using the `topNRows.Less` order (max-heap order: returns true when
/// `a` should sit above `b` in the eviction heap).
// glue: shared comparison for topNRows.Less and sort.Reverse(h) in topNRows.concat
fn compare_sort_row(
    tc: &TypeContext,
    desc: &[bool],
    collators: &[&'static dyn Collator],
    a: &SortRow,
    b: &SortRow,
) -> bool {
    let n = a.by_items.len();
    for k in 0..n {
        let (mut ret, err) = a.by_items[k]
            .datum
            .compare(tc, &b.by_items[k].datum, collators[k]);
        if err.is_err() {
            // Go ignores comparison errors here (spec §8 item 4).
            return false;
        }
        if desc[k] {
            ret = -ret;
        }
        if ret > 0 {
            return true;
        }
        if ret < 0 {
            return false;
        }
    }
    false
}

struct TopNRows {
    rows: AllocVec<SortRow, ColAlloc>,
    desc: Vec<bool>,
    // Go never assigns err (kept for faithful structure).
    err: Option<Error>,

    curr_size: u64,
    limit_size: u64,
    sep_size: u64,
    // If sep is truncated, we need to append part of sep to result.
    // In the following example, session.group_concat_max_len is 10 and sep is '---'.
    // ('---', 'ccc') should be popped from heap, so '-' should be appended to result.
    // eg: 'aaa---bbb---ccc' -> 'aaa---bbb-'
    is_sep_truncated: bool,
    collators: Vec<&'static dyn Collator>,
}

/// Adapter borrowing `TopNRows` and the call's `TypeContext` for `gostd::heap`.
struct TopNHeap<'a> {
    top_n: &'a mut TopNRows,
    tc: &'a TypeContext,
}

impl HeapInterface for TopNHeap<'_> {
    // glue: heap.Interface.Len forwarding to TopNRows::len
    fn len(&self) -> usize {
        self.top_n.len()
    }

    // glue: heap.Interface.Less forwarding to TopNRows::less
    fn less(&self, i: usize, j: usize) -> bool {
        self.top_n.less(self.tc, i, j)
    }

    // glue: heap.Interface.Swap forwarding to TopNRows::swap
    fn swap(&mut self, i: usize, j: usize) {
        self.top_n.swap(i, j);
    }

    // glue: heap.Interface.Pop forwarding to TopNRows::pop
    fn pop(&mut self) {
        let _ = self.top_n.pop();
    }
}

impl HeapPushInterface for TopNHeap<'_> {
    type Item = SortRow;

    // glue: heap.Interface.Push forwarding to TopNRows::push
    fn push(&mut self, x: SortRow) {
        self.top_n.push(x);
    }
}

impl TopNRows {
    // go: topNRows.Len L1
    fn len(&self) -> usize {
        self.rows.len()
    }

    // go: topNRows.Less L2 (TypeContext passed per call instead of stored on struct, spec §7 item 4)
    fn less(&self, tc: &TypeContext, i: usize, j: usize) -> bool {
        compare_sort_row(
            tc,
            &self.desc,
            &self.collators,
            &self.rows[i],
            &self.rows[j],
        )
    }

    // go: topNRows.Swap L1
    fn swap(&mut self, i: usize, j: usize) {
        self.rows.swap(i, j);
    }

    // go: topNRows.Push L1
    fn push(&mut self, x: SortRow) {
        self.rows.push(x);
    }

    // go: topNRows.Pop L1
    fn pop(&mut self) -> SortRow {
        self.rows.pop().expect("topNRows.Pop on empty heap")
    }

    // go: topNRows.tryToAdd L2 (memDelta dropped, A1; TypeContext passed in, spec §7 item 4 [M-INT])
    fn try_to_add(&mut self, tc: &TypeContext, row: SortRow) -> bool {
        self.curr_size += row.buffer.len() as u64;
        if !self.rows.is_empty() {
            self.curr_size += self.sep_size;
        }
        {
            let mut h = TopNHeap { top_n: self, tc };
            heap::push(&mut h, row);
        }
        if self.curr_size <= self.limit_size {
            return false;
        }

        while self.curr_size > self.limit_size {
            let debt = self.curr_size - self.limit_size;
            let mut heap_pop_row = {
                let mut h = TopNHeap { top_n: self, tc };
                heap::pop_with(&mut h, |h| h.top_n.pop())
            };
            if heap_pop_row.buffer.len() as u64 > debt {
                self.curr_size -= debt;
                let new_len = heap_pop_row.buffer.len() - debt as usize;
                heap_pop_row.buffer.truncate(new_len);
                let mut h = TopNHeap { top_n: self, tc };
                heap::push(&mut h, heap_pop_row);
            } else {
                self.curr_size = self
                    .curr_size
                    .wrapping_sub((heap_pop_row.buffer.len() as u64).wrapping_add(self.sep_size));
                self.is_sep_truncated = true;
            }
        }
        true
    }

    // go: topNRows.reset L1
    fn reset(&mut self) {
        self.rows.clear();
        self.err = None;
        self.curr_size = 0;
    }

    // go: topNRows.concat L2 (TypeContext passed from AppendFinalResult2Chunk, spec §7 item 4)
    fn concat(&mut self, tc: &TypeContext, sep: &[u8], _truncated: bool) -> Vec<u8> {
        let mut buffer = Vec::new();
        let desc = &self.desc;
        let collators = &self.collators;
        // Go: sort.Sort(sort.Reverse(h)) -> reverse.Less(i, j) calls h.Less(j, i).
        go_sort(&mut self.rows, |a, b| {
            compare_sort_row(tc, desc, collators, b, a)
        });
        for (i, row) in self.rows.iter().enumerate() {
            if i != 0 {
                buffer.extend_from_slice(sep);
            }
            buffer.extend_from_slice(&row.buffer);
        }
        if self.is_sep_truncated {
            buffer.extend_from_slice(sep);
            if buffer.len() as u64 > self.limit_size {
                buffer.truncate(self.limit_size as usize);
            }
        }
        buffer
    }
}

struct PartialResult4GroupConcatOrder {
    top_n: TopNRows,
}

/// groupConcatOrder.
#[derive(Clone)]
pub struct GroupConcatOrder {
    pub base: BaseGroupConcat4String,
    pub ctors: Vec<&'static dyn Collator>,
    pub desc: Vec<bool>,
    /// The allocator of `topNRows` buffers (A1, spec §5.3).
    alloc: ColAlloc,
}

impl GroupConcatOrder {
    /// The function with containers in the global allocator until [`AggFunc::set_alloc`].
    // glue: Go struct literal groupConcatOrder{base, ctors, desc}
    pub fn new(
        base: BaseGroupConcat4String,
        ctors: Vec<&'static dyn Collator>,
        desc: Vec<bool>,
    ) -> Self {
        GroupConcatOrder {
            base,
            ctors,
            desc,
            alloc: ColAlloc::Global,
        }
    }
}

impl AggFunc for GroupConcatOrder {
    // go: groupConcatOrder.AppendFinalResult2Chunk L2 (uses AppendFinal's sctx.type_ctx(), spec §7 item 4)
    fn append_final_result2_chunk(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4GroupConcatOrder>(pr);
        if p.top_n.len() == 0 {
            chk.append_null(self.base.base.col());
            return Ok(());
        }
        let tc = sctx.type_ctx();
        let s = p.top_n.concat(
            &tc,
            &self.base.sep,
            self.base.truncated.load(Ordering::SeqCst) == 1,
        );
        chk.append_string(self.base.base.col(), &s);
        Ok(())
    }

    // go: groupConcatOrder.AllocPartialResult L2 (memDelta dropped, A1; rows in ColAlloc, spec §5.3)
    fn alloc_partial_result(&self) -> PartialResult {
        Box::new(PartialResult4GroupConcatOrder {
            top_n: TopNRows {
                rows: AllocVec::new_in(self.alloc.clone()),
                desc: self.desc.clone(),
                err: None,
                curr_size: 0,
                limit_size: self.base.max_len,
                sep_size: self.base.sep.len() as u64,
                is_sep_truncated: false,
                collators: self.ctors.clone(),
            },
        })
    }

    // go: groupConcatOrder.ResetPartialResult L1
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4GroupConcatOrder>(pr);
        p.top_n.reset();
    }

    // go: groupConcatOrder.UpdatePartialResult L2 (memDelta dropped, A1; sctx.type_ctx() passed into try_to_add, spec §7 item 4)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4GroupConcatOrder>(pr);
        let tc = sctx.type_ctx();
        let mut is_null = false;

        for &row in rows_in_group {
            let mut buffer = AllocVec::new_in(self.alloc.clone());
            for arg in &self.base.base.args {
                match arg.eval_string(sctx, st, row)? {
                    Some(v) => {
                        is_null = false;
                        buffer.extend_from_slice(&v);
                    }
                    None => {
                        is_null = true;
                        break;
                    }
                }
            }
            if is_null {
                continue;
            }
            let mut by_items =
                AllocVec::with_capacity_in(self.base.by_items.len(), self.alloc.clone());
            for by_item in &self.base.by_items {
                let d = by_item.expr.eval(sctx, st, row)?;
                by_items.push(TrackedDatum::new(d.clone_datum(), self.alloc.clone()));
            }
            let sort_row = SortRow { buffer, by_items };
            let truncated = p.top_n.try_to_add(&tc, sort_row);
            if let Some(err) = p.top_n.err.clone() {
                return Err(err);
            }
            if truncated {
                self.base.handle_truncate_error(sctx)?;
            }
        }
        Ok(())
    }

    // go: groupConcatOrder.MergePartialResult L2 (memDelta dropped, A1)
    fn merge_partial_result(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        _src: &mut PartialResult,
        _dst: &mut PartialResult,
    ) -> Result<(), Error> {
        // If order by exists, the parallel hash aggregation is forbidden in
        // executorBuilder.buildHashAgg. So MergePartialResult will not be called.
        Err(plannererrors::ErrInternal.gen_with_stack(
            "groupConcatOrder.MergePartialResult should not be called",
            vec![],
        ))
    }

    // go: groupConcatOrder.SetTruncated L2 (Arc<AtomicI32> for *int32, spec §5.1)
    fn set_truncated(&mut self, t: Arc<AtomicI32>) {
        self.base.truncated = t;
    }

    // go: groupConcatOrder.GetTruncated L2 (Arc<AtomicI32> for *int32, spec §5.1)
    fn get_truncated(&self) -> Option<Arc<AtomicI32>> {
        Some(Arc::clone(&self.base.truncated))
    }

    // glue: A1 3.3 TrackedAlloc of topNRows storage (spec §5.3)
    fn set_alloc(&mut self, alloc: ColAlloc) {
        self.alloc = alloc;
    }
}

struct PartialResult4GroupConcatOrderDistinct {
    top_n: TopNRows,
    val_set: StringSetWithMemoryUsage<ColAlloc>,
    encode_bytes_buffer: Vec<u8>,
}

/// groupConcatDistinctOrder.
///
/// Note: matching Go (`func_group_concat.go`), this type intentionally does not implement
/// `GetTruncated` / `SetTruncated` (spec §5.1).
#[derive(Clone)]
pub struct GroupConcatDistinctOrder {
    pub base: BaseGroupConcat4String,
    pub ctors: Vec<&'static dyn Collator>,
    pub desc: Vec<bool>,
    /// The allocator of `topNRows` buffers and DISTINCT set (A1, spec §5.3).
    alloc: ColAlloc,
}

impl GroupConcatDistinctOrder {
    /// The function with containers in the global allocator until [`AggFunc::set_alloc`].
    // glue: Go struct literal groupConcatDistinctOrder{base, ctors, desc}
    pub fn new(
        base: BaseGroupConcat4String,
        ctors: Vec<&'static dyn Collator>,
        desc: Vec<bool>,
    ) -> Self {
        GroupConcatDistinctOrder {
            base,
            ctors,
            desc,
            alloc: ColAlloc::Global,
        }
    }
}

impl AggFunc for GroupConcatDistinctOrder {
    // go: groupConcatDistinctOrder.AppendFinalResult2Chunk L2 (uses AppendFinal's sctx.type_ctx(), spec §7 item 4)
    fn append_final_result2_chunk(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4GroupConcatOrderDistinct>(pr);
        if p.top_n.len() == 0 {
            chk.append_null(self.base.base.col());
            return Ok(());
        }
        let tc = sctx.type_ctx();
        let s = p.top_n.concat(
            &tc,
            &self.base.sep,
            self.base.truncated.load(Ordering::SeqCst) == 1,
        );
        chk.append_string(self.base.base.col(), &s);
        Ok(())
    }

    // go: groupConcatDistinctOrder.AllocPartialResult L2 (memDelta dropped, A1; containers in ColAlloc, spec §5.3)
    fn alloc_partial_result(&self) -> PartialResult {
        let (val_set, _) = new_string_set_with_memory_usage(self.alloc.clone());
        Box::new(PartialResult4GroupConcatOrderDistinct {
            top_n: TopNRows {
                rows: AllocVec::new_in(self.alloc.clone()),
                desc: self.desc.clone(),
                err: None,
                curr_size: 0,
                limit_size: self.base.max_len,
                sep_size: self.base.sep.len() as u64,
                is_sep_truncated: false,
                collators: self.ctors.clone(),
            },
            val_set,
            encode_bytes_buffer: Vec::new(),
        })
    }

    // go: groupConcatDistinctOrder.ResetPartialResult L2 (the set's allocator, spec §5.3)
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4GroupConcatOrderDistinct>(pr);
        p.top_n.reset();
        p.val_set = new_string_set_with_memory_usage(self.alloc.clone()).0;
    }

    // go: groupConcatDistinctOrder.UpdatePartialResult L2 (memDelta dropped, A1; sctx.type_ctx() passed into try_to_add, spec §7 item 4)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4GroupConcatOrderDistinct>(pr);
        let tc = sctx.type_ctx();
        let mut is_null = false;

        let mut collators = Vec::with_capacity(self.base.base.args.len());
        for arg in &self.base.base.args {
            collators.push(get_collator(arg.get_type(sctx).get_collate()));
        }

        for &row in rows_in_group {
            let mut buffer = AllocVec::new_in(self.alloc.clone());
            p.encode_bytes_buffer.clear();
            for (i, arg) in self.base.base.args.iter().enumerate() {
                match arg.eval_string(sctx, st, row)? {
                    Some(v) => {
                        is_null = false;
                        encode_bytes(&mut p.encode_bytes_buffer, &collators[i].key(&v));
                        buffer.extend_from_slice(&v);
                    }
                    None => {
                        is_null = true;
                        break;
                    }
                }
            }
            if is_null {
                continue;
            }
            if p.val_set.exist(&p.encode_bytes_buffer) {
                continue;
            }
            p.val_set.insert(&p.encode_bytes_buffer);
            let mut by_items =
                AllocVec::with_capacity_in(self.base.by_items.len(), self.alloc.clone());
            for by_item in &self.base.by_items {
                let d = by_item.expr.eval(sctx, st, row)?;
                by_items.push(TrackedDatum::new(d.clone_datum(), self.alloc.clone()));
            }
            let sort_row = SortRow { buffer, by_items };
            let truncated = p.top_n.try_to_add(&tc, sort_row);
            if let Some(err) = p.top_n.err.clone() {
                return Err(err);
            }
            if truncated {
                self.base.handle_truncate_error(sctx)?;
            }
        }
        Ok(())
    }

    // go: groupConcatDistinctOrder.MergePartialResult L2 (memDelta dropped, A1)
    fn merge_partial_result(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        _src: &mut PartialResult,
        _dst: &mut PartialResult,
    ) -> Result<(), Error> {
        // If order by exists, the parallel hash aggregation is forbidden in
        // executorBuilder.buildHashAgg. So MergePartialResult will not be called.
        Err(plannererrors::ErrInternal.gen_with_stack(
            "groupConcatDistinctOrder.MergePartialResult should not be called",
            vec![],
        ))
    }

    // glue: A1 3.3 TrackedAlloc of topNRows storage and DISTINCT set (spec §5.3)
    fn set_alloc(&mut self, alloc: ColAlloc) {
        self.alloc = alloc;
    }
}
