//! go-file: pkg/distsql/select_result.go
//!
//! Port of Go TiDB v8.5.8 `pkg/distsql/select_result.go` (block D07.3, Parts 1 & 2: 41
//! functions): `SelectResult` / `SelectResultIter` traits, `GetSelectResultConcurrency`,
//! `chunkRowHeap` + `sortedSelectResults`, `selectResult` (`Next`, `IntoIter`, `NextRaw`,
//! `fetchResp`, `fetchRespWithIntermediateResults`, `readFromChunk`, `readFromDefault`,
//! `readRowsData`, `updateCopRuntimeStats`, `memConsume`, `Close`, `close`),
//! `selRespChannelIter` + `selectResultIter`, and `selectResultRuntimeStats`.

use std::any::Any;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use protobuf::Message;
use tidb_base::Error;
use tidb_base::dbterror::ClassTiKV;
use tidb_base::gostd::heap;
use tidb_base::gostd::strconv::format_float;
use tidb_base::gostd::time::Location;
use tidb_base::kv::{StoreType, TiFlash};
use tidb_chunk::ColAlloc;
use tidb_chunk::chunk::{Chunk, new_chunk_with_capacity, renew_with_capacity_in};
use tidb_chunk::codec::{
    Decoder as ChunkDecoder, new_codec as new_chunk_codec, new_decoder as new_chunk_decoder,
};
use tidb_chunk::compare::{CompareFunc, get_compare_func};
use tidb_chunk::list::RowPtr;
use tidb_chunk::row::{Row, RowHandle};
use tidb_codec::codec::new_decoder as new_datum_decoder;
use tidb_execdetails::execdetails::{
    DurationWrapper, Int64, Percentile, RuntimeStats, TpSelectResultRuntimeStats, format_duration,
};
use tidb_expression::byitem::ByItems;
use tidb_expression::{EvalContext, Schema};
use tidb_kv::copr::CopRuntimeStats;
use tidb_kv::kv::Response;
use tidb_memory::TrackedAlloc;
use tidb_memory::sqlkiller::SqlKiller;
use tidb_memory::tracker::Tracker;
use tidb_parser::types::FieldType;
use tidb_runtime::{CheckpointHook, KillCancel};
use tidb_sessionctx::distsql_ctx::DistSQLContext;
use tidb_sessionctx::variable::tidb_defs::DefInitChunkSize;

/// Checkpoint hook that checks the statement's `SqlKiller` (1s throttled disconnect probe +
/// signal mapping) or falls back to `KillCancel` when `SqlKiller` is not attached.
// glue: N3 §3.3 cooperative cancellation checkpoint hook in DistSQL
struct KillCheckHook<'a> {
    sql_killer: Option<&'a Arc<SqlKiller>>,
    kill_cancel: Option<&'a KillCancel>,
}

impl CheckpointHook for KillCheckHook<'_> {
    type Error = Error;

    #[inline]
    fn check(&self) -> Result<(), Error> {
        if let Some(killer) = self.sql_killer {
            return killer.handle_signal();
        }
        if let Some(kc) = self.kill_cancel
            && kc.is_killed()
        {
            return Err(SqlKiller::signal_to_error(kc.signal(), 0));
        }
        Ok(())
    }
}

/// SelectResultRow indicates the row returned by [`SelectResultIter`].
#[derive(Clone, Debug)]
pub struct SelectResultRow {
    pub channel_index: isize,
    pub row: RowHandle,
}

/// SelectResultIter is an iterator used to iterate the rows from [`SelectResult`].
#[async_trait]
pub trait SelectResultIter: Send {
    // glue: Go interface method SelectResultIter.Next
    async fn next(&mut self, ctx: &tidb_kv::Context) -> Result<Option<SelectResultRow>, Error>;
    // glue: Go interface method SelectResultIter.Close
    fn close(&mut self) -> Result<(), Error>;
}

/// SelectResult is an iterator of coprocessor partial results.
#[async_trait]
pub trait SelectResult: Send {
    // glue: Go interface method SelectResult.NextRaw
    async fn next_raw(&mut self, ctx: &tidb_kv::Context) -> Result<Option<Vec<u8>>, Error>;
    // glue: Go interface method SelectResult.Next
    async fn next(&mut self, ctx: &tidb_kv::Context, chk: &mut Chunk) -> Result<(), Error>;
    // glue: Go interface method SelectResult.IntoIter (takes &mut self per spec §4.5 to keep the trait object-safe and return error on post-IntoIter use)
    #[allow(clippy::wrong_self_convention)]
    fn into_iter(
        &mut self,
        intermediate_field_types: Vec<Vec<Arc<FieldType>>>,
    ) -> Result<Box<dyn SelectResultIter>, Error>;
    // glue: Go interface method SelectResult.Close
    fn close(&mut self) -> Result<(), Error>;
    // glue: M-IFACE (Go GetSelectResultConcurrency type assertion to *selectResult + copr.CopInfo)
    fn cop_concurrency(&self) -> Option<(isize, isize)> {
        None
    }
}

/// GetSelectResultConcurrency returns the internal cop iterator concurrency for a
/// [`SelectResult`]. Returns `None` if the underlying implementation does not expose it.
// go: GetSelectResultConcurrency L1 [M-IFACE]
pub fn get_select_result_concurrency(sr: &dyn SelectResult) -> Option<(isize, isize)> {
    sr.cop_concurrency()
}

/// Min-heap view over [`SortedSelectResults`] matching Go's `chunkRowHeap` and
/// `container/heap` sift order (for exact tie-breaking parity).
struct ChunkRowHeap<'a> {
    ssr: &'a mut SortedSelectResults,
}

impl ChunkRowHeap<'_> {
    // go: chunkRowHeap.Len L1
    fn len_impl(&self) -> usize {
        self.ssr.row_ptrs.len()
    }

    // go: chunkRowHeap.Less L1
    fn less_impl(&self, i: usize, j: usize) -> bool {
        let i_ptr = self.ssr.row_ptrs[i];
        let j_ptr = self.ssr.row_ptrs[j];
        let row_i = self.ssr.cached_chunks[i_ptr.chk_idx as usize]
            .as_ref()
            .unwrap()
            .get_row(i_ptr.row_idx as usize);
        let row_j = self.ssr.cached_chunks[j_ptr.chk_idx as usize]
            .as_ref()
            .unwrap()
            .get_row(j_ptr.row_idx as usize);
        self.ssr.less_row(row_i, row_j)
    }

    // go: chunkRowHeap.Swap L1
    fn swap_impl(&mut self, i: usize, j: usize) {
        self.ssr.row_ptrs.swap(i, j);
    }

    // go: chunkRowHeap.Push L1
    fn push_impl(&mut self, x: RowPtr) {
        self.ssr.row_ptrs.push(x);
    }

    // go: chunkRowHeap.Pop L1
    fn pop_impl(&mut self) -> RowPtr {
        self.ssr.row_ptrs.pop().expect("chunkRowHeap pop on empty")
    }
}

impl heap::Interface for ChunkRowHeap<'_> {
    // glue: container/heap.Interface.Len -> chunkRowHeap.Len
    fn len(&self) -> usize {
        self.len_impl()
    }

    // glue: container/heap.Interface.Less -> chunkRowHeap.Less
    fn less(&self, i: usize, j: usize) -> bool {
        self.less_impl(i, j)
    }

    // glue: container/heap.Interface.Swap -> chunkRowHeap.Swap
    fn swap(&mut self, i: usize, j: usize) {
        self.swap_impl(i, j);
    }

    // glue: container/heap.Interface.Pop -> chunkRowHeap.Pop
    fn pop(&mut self) {
        let _ = self.pop_impl();
    }
}

impl heap::PushInterface for ChunkRowHeap<'_> {
    type Item = RowPtr;

    // glue: container/heap.Interface.Push -> chunkRowHeap.Push
    fn push(&mut self, x: RowPtr) {
        self.push_impl(x);
    }
}

/// NewSortedSelectResults merges multiple sorted [`SelectResult`] streams (e.g. from
/// partition tables) in `by_items` order. If `schema` is `None`, sorts by the first
/// `by_items.len()` columns.
// go: NewSortedSelectResults L2 (returns Result on unsupported compare type or non-column ByItems; A1 / M-MEM TrackedAlloc; N3 KillCancel)
pub fn new_sorted_select_results(
    eval_ctx: &dyn EvalContext,
    select_results: Vec<Box<dyn SelectResult>>,
    schema: Option<&Schema>,
    by_items: Vec<ByItems>,
    mem_tracker: Option<Arc<Tracker>>,
    sql_killer: Option<Arc<SqlKiller>>,
    kill_cancel: Option<KillCancel>,
) -> Result<Box<dyn SelectResult>, Error> {
    let n = select_results.len();
    let tracked_alloc = mem_tracker
        .as_ref()
        .map(|t| TrackedAlloc::new(Arc::clone(t)));
    let mut s = SortedSelectResults {
        select_results,
        compare_funcs: Vec::new(),
        by_items,
        key_columns: Vec::new(),
        cached_chunks: (0..n).map(|_| None).collect(),
        row_ptrs: Vec::new(),
        mem_tracker,
        tracked_alloc,
        sql_killer,
        kill_cancel,
        closed: false,
    };
    s.init_compare_funcs(eval_ctx)?;
    s.build_key_columns(schema)?;
    Ok(Box::new(s))
}

struct SortedSelectResults {
    select_results: Vec<Box<dyn SelectResult>>,
    compare_funcs: Vec<CompareFunc>,
    by_items: Vec<ByItems>,
    key_columns: Vec<usize>,

    cached_chunks: Vec<Option<Chunk>>,
    row_ptrs: Vec<RowPtr>,

    mem_tracker: Option<Arc<Tracker>>,
    tracked_alloc: Option<TrackedAlloc>,
    sql_killer: Option<Arc<SqlKiller>>,
    kill_cancel: Option<KillCancel>,
    closed: bool,
}

impl Drop for SortedSelectResults {
    /// N3 §3.5 drop safety net: closes remaining child `SelectResult`s and releases cached
    /// chunk memory if dropped without an explicit `close()`.
    // glue: Drop safety net (N3 §3.5)
    fn drop(&mut self) {
        if !self.closed {
            self.closed = true;
            for (sr, cached) in self
                .select_results
                .iter_mut()
                .zip(self.cached_chunks.iter_mut())
            {
                let _ = sr.close();
                *cached = None;
            }
            if let Some(alloc) = &self.tracked_alloc {
                alloc.flush();
            }
        }
    }
}

impl SortedSelectResults {
    // go: sortedSelectResults.updateCachedChunk L2 (A1 / M-MEM: cached chunks use TrackedAlloc + checkpoint)
    async fn update_cached_chunk(&mut self, ctx: &tidb_kv::Context, idx: u32) -> Result<(), Error> {
        let i = idx as usize;
        let chk = self.cached_chunks[i]
            .as_mut()
            .expect("cached chunk initialized");
        self.select_results[i].next(ctx, chk).await?;
        if let Some(alloc) = &self.tracked_alloc {
            alloc.flush();
        }
        if let Some(tracker) = &self.mem_tracker {
            tracker.checkpoint()?;
        }
        if chk.num_rows() == 0 {
            return Ok(());
        }
        let mut h = ChunkRowHeap { ssr: self };
        heap::push(
            &mut h,
            RowPtr {
                chk_idx: idx,
                row_idx: 0,
            },
        );
        Ok(())
    }

    // go: sortedSelectResults.initCompareFuncs L2 (returns Result if get_compare_func returns None)
    fn init_compare_funcs(&mut self, eval_ctx: &dyn EvalContext) -> Result<(), Error> {
        self.compare_funcs = Vec::with_capacity(self.by_items.len());
        for item in &self.by_items {
            let key_type = item.expr.get_type(eval_ctx);
            let cmp = get_compare_func(&key_type).ok_or_else(|| {
                Error::plain(format!(
                    "unsupported compare type for sortedSelectResults: {}",
                    key_type.get_type()
                ))
            })?;
            self.compare_funcs.push(cmp);
        }
        Ok(())
    }

    // go: sortedSelectResults.buildKeyColumns L2 (takes Option<&Schema> directly; returns Result on non-column or missing column)
    fn build_key_columns(&mut self, schema: Option<&Schema>) -> Result<(), Error> {
        self.key_columns = Vec::with_capacity(self.by_items.len());
        for (i, by) in self.by_items.iter().enumerate() {
            let col = by.expr.as_column().ok_or_else(|| {
                Error::plain("sortedSelectResults byItem expression must be a Column")
            })?;
            match schema {
                None => self.key_columns.push(i),
                Some(schema) => {
                    let idx = schema.column_index(col);
                    if idx < 0 {
                        return Err(Error::plain(format!(
                            "column {} not found in schema for sortedSelectResults",
                            col.id
                        )));
                    }
                    self.key_columns.push(idx as usize);
                }
            }
        }
        Ok(())
    }

    // go: sortedSelectResults.lessRow L1
    fn less_row(&self, row_i: Row<'_>, row_j: Row<'_>) -> bool {
        for (i, &col_idx) in self.key_columns.iter().enumerate() {
            let mut cmp = (self.compare_funcs[i])(row_i, col_idx, row_j, col_idx);
            if self.by_items[i].desc {
                cmp = -cmp;
            }
            if cmp < 0 {
                return true;
            } else if cmp > 0 {
                return false;
            }
        }
        false
    }
}

#[async_trait]
impl SelectResult for SortedSelectResults {
    // go: sortedSelectResults.NextRaw L1 [M-PANIC]
    async fn next_raw(&mut self, _ctx: &tidb_kv::Context) -> Result<Option<Vec<u8>>, Error> {
        panic!("Not support NextRaw for sortedSelectResults");
    }

    // go: sortedSelectResults.Next L2 (A1 / M-MEM TrackedAlloc + checkpoint; N3 cooperative checkpoint)
    async fn next(&mut self, ctx: &tidb_kv::Context, c: &mut Chunk) -> Result<(), Error> {
        c.reset();
        for cached in &mut self.cached_chunks {
            if cached.is_none() {
                let new_chk = match &self.tracked_alloc {
                    Some(alloc) => renew_with_capacity_in(
                        c,
                        c.capacity(),
                        c.required_rows(),
                        ColAlloc::Tracked(alloc.clone()),
                    ),
                    None => c.copy_construct(),
                };
                *cached = Some(new_chk);
                if let Some(alloc) = &self.tracked_alloc {
                    alloc.flush();
                }
                if let Some(tracker) = &self.mem_tracker {
                    tracker.checkpoint()?;
                }
            }
        }

        if self.row_ptrs.is_empty() {
            for i in 0..self.cached_chunks.len() {
                self.update_cached_chunk(ctx, i as u32).await?;
            }
        }

        while c.num_rows() < c.required_rows() {
            if self.row_ptrs.is_empty() {
                break;
            }

            let idx = {
                let mut h = ChunkRowHeap { ssr: self };
                heap::pop_with(&mut h, |h| h.pop_impl())
            };
            let chk_idx = idx.chk_idx as usize;
            let row_idx = idx.row_idx as usize;
            let cached = self.cached_chunks[chk_idx].as_ref().unwrap();
            c.append_row(cached.get_row(row_idx));
            if row_idx >= cached.num_rows() - 1 {
                self.update_cached_chunk(ctx, idx.chk_idx).await?;
            } else {
                let mut h = ChunkRowHeap { ssr: self };
                heap::push(
                    &mut h,
                    RowPtr {
                        chk_idx: idx.chk_idx,
                        row_idx: idx.row_idx + 1,
                    },
                );
            }
            let hook = KillCheckHook {
                sql_killer: self.sql_killer.as_ref(),
                kill_cancel: self.kill_cancel.as_ref(),
            };
            tidb_runtime::checkpoint(&hook).await?;
        }
        Ok(())
    }

    // go: sortedSelectResults.IntoIter L1
    fn into_iter(
        &mut self,
        _intermediate_field_types: Vec<Vec<Arc<FieldType>>>,
    ) -> Result<Box<dyn SelectResultIter>, Error> {
        Err(Error::plain("not implemented"))
    }

    // go: sortedSelectResults.Close L2 (N3 §3.5 / spec §8 item 4: closes all children and drops all cached chunks even on error, returning first error)
    fn close(&mut self) -> Result<(), Error> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        let mut first_err = None;
        for (sr, cached) in self
            .select_results
            .iter_mut()
            .zip(self.cached_chunks.iter_mut())
        {
            if let Err(e) = sr.close()
                && first_err.is_none()
            {
                first_err = Some(e);
            }
            *cached = None;
        }
        if let Some(alloc) = &self.tracked_alloc {
            alloc.flush();
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

/// Concrete [`SelectResult`] wrapping a `kv::Response`.
///
/// Wraps `Option<SelectResultInner>` so Part 2's `into_iter` can move the inner state cleanly
/// and reject subsequent calls with `"selectResult is invalid after IntoIter()"` (D07.3
/// spec §4.5).
pub struct SelectResultImpl {
    inner: Option<SelectResultInner>,
}

pub(crate) struct SelectResultInner {
    label: &'static str,
    resp: Box<dyn Response>,

    row_len: usize,
    field_types: Vec<Arc<FieldType>>,
    pub(crate) intermediate_output_types: Vec<Vec<Arc<FieldType>>>,
    ctx: Arc<DistSQLContext>,

    pub(crate) select_resp: Option<tipb::SelectResponse>,
    select_resp_size: AtomicI64,
    resp_chk_idx: usize,
    rows_data_offset: usize,
    resp_chunk_decoder: Option<ChunkDecoder>,

    partial_count: i64,
    sql_type: &'static str,

    cop_plan_ids: Vec<isize>,
    root_plan_id: isize,

    store_type: StoreType,

    fetch_duration: Duration,
    duration_reported: bool,
    mem_tracker: Option<Arc<Tracker>>,

    stats: Option<SelectResultRuntimeStats>,
    dist_sql_concurrency: isize,
    paging: bool,

    closed: bool,
}

impl SelectResultImpl {
    #[allow(clippy::too_many_arguments)]
    // glue: Go composite literal &selectResult{...} in distsql.Select
    pub(crate) fn new(
        label: &'static str,
        resp: Box<dyn Response>,
        field_types: Vec<Arc<FieldType>>,
        ctx: Arc<DistSQLContext>,
        sql_type: &'static str,
        mem_tracker: Option<Arc<Tracker>>,
        store_type: StoreType,
        paging: bool,
        dist_sql_concurrency: isize,
        cop_plan_ids: Vec<isize>,
        root_plan_id: isize,
    ) -> Self {
        let row_len = field_types.len();
        SelectResultImpl {
            inner: Some(SelectResultInner {
                label,
                resp,
                row_len,
                field_types,
                intermediate_output_types: Vec::new(),
                ctx,
                select_resp: None,
                select_resp_size: AtomicI64::new(0),
                resp_chk_idx: 0,
                rows_data_offset: 0,
                resp_chunk_decoder: None,
                partial_count: 0,
                sql_type,
                cop_plan_ids,
                root_plan_id,
                store_type,
                fetch_duration: Duration::ZERO,
                duration_reported: false,
                mem_tracker,
                stats: None,
                dist_sql_concurrency,
                paging,
                closed: false,
            }),
        }
    }

    // glue: invalid-after-IntoIter guard (Go `if r.iter != nil`)
    fn inner_mut(&mut self) -> Result<&mut SelectResultInner, Error> {
        self.inner
            .as_mut()
            .ok_or_else(|| Error::plain("selectResult is invalid after IntoIter()"))
    }

    #[cfg(test)]
    // glue: test accessor for unit tests mirroring Go white-box package tests
    pub(crate) fn inner_mut_for_test(&mut self) -> &mut SelectResultInner {
        self.inner.as_mut().expect("SelectResultImpl inner present")
    }
}

impl Drop for SelectResultInner {
    /// N3 §3.5 cancellation / drop safety net: closes the underlying `kv::Response` (stopping
    /// `CopIterator` workers) and zeros `select_resp_size` on `mem_tracker`, without
    /// observing metrics or registering runtime stats.
    // glue: Drop safety net (N3 §3.5)
    fn drop(&mut self) {
        if !self.closed {
            self.closed = true;
            let resp_size = self.select_resp_size.swap(0, Ordering::SeqCst);
            if resp_size > 0 {
                let _ = self.mem_consume(-resp_size);
            }
            self.resp_chunk_decoder = None;
            self.select_resp = None;
            let _ = self.resp.close();
        }
    }
}

impl SelectResultInner {
    fn kill_hook(&self) -> KillCheckHook<'_> {
        KillCheckHook {
            sql_killer: self.ctx.sql_killer.as_ref(),
            kill_cancel: self.ctx.kill_cancel.as_ref(),
        }
    }

    // go: selectResult.fetchResp L1
    async fn fetch_resp(&mut self, ctx: &tidb_kv::Context) -> Result<(), Error> {
        self.fetch_resp_with_intermediate_results(ctx, Vec::new())
            .await
    }

    // go: selectResult.fetchRespWithIntermediateResults L1 [M-ERR, M-KILL, M-MEM]
    async fn fetch_resp_with_intermediate_results(
        &mut self,
        ctx: &tidb_kv::Context,
        intermediate_output_types: Vec<Vec<Arc<FieldType>>>,
    ) -> Result<(), Error> {
        // Telemetry counters (`telemetry.CurrentCoprCacheHitRatio*`) belong to X and are omitted.
        loop {
            self.resp_chk_idx = 0;
            self.rows_data_offset = 0;
            let start_time = Instant::now();
            let result_subset = self.resp.next(ctx).await;
            let duration = start_time.elapsed();
            self.fetch_duration += duration;
            let result_subset = result_subset?;

            if self.select_resp.is_some() {
                let prev = self.select_resp_size.load(Ordering::SeqCst);
                if prev > 0 {
                    let _ = self.mem_consume(-prev);
                }
            }
            let Some(result_subset) = result_subset else {
                self.select_resp = None;
                self.select_resp_size.store(0, Ordering::SeqCst);
                if !self.duration_reported {
                    tidb_metrics::distsql_query_with_labels(
                        self.label,
                        self.sql_type,
                        self.paging,
                        self.fetch_duration,
                    );
                    self.duration_reported = true;
                }
                return Ok(());
            };

            let mut select_resp = tipb::SelectResponse::default();
            if let Err(e) = select_resp.merge_from_bytes(result_subset.get_data()) {
                self.select_resp = None;
                self.select_resp_size.store(0, Ordering::SeqCst);
                return Err(Error::plain(e.to_string()));
            }

            let resp_size = select_resp.compute_size() as i64;
            self.select_resp = Some(select_resp);
            self.select_resp_size.store(resp_size, Ordering::SeqCst);
            self.mem_consume(resp_size)?;

            let sr = self.select_resp.as_ref().unwrap();
            if sr.has_error() {
                let err = sr.get_error();
                return Err(ClassTiKV.synthesize(err.get_code() as isize, err.get_msg().to_owned()));
            }

            if sr.get_intermediate_outputs().len() != intermediate_output_types.len() {
                return Err(Error::plain(format!(
                    "The length of intermediate output types {} mismatches the length of got intermediate outputs {}. If a response contains intermediate outputs, you should use the SelectResultIter to read the data.",
                    intermediate_output_types.len(),
                    sr.get_intermediate_outputs().len(),
                )));
            }
            self.intermediate_output_types = intermediate_output_types.clone();

            if let Some(killer) = &self.ctx.sql_killer {
                killer.handle_signal()?;
            }

            for warning in sr.get_warnings() {
                self.ctx.append_warning(
                    ClassTiKV.synthesize(warning.get_code() as isize, warning.get_msg().to_owned()),
                );
            }

            self.partial_count += 1;

            if let Some(cop_stats) = result_subset.get_cop_runtime_stats().cloned() {
                self.update_cop_runtime_stats(&cop_stats, result_subset.resp_time(), false)?;
                self.ctx.exec_details.merge_cop_exec_details(
                    Some(&cop_stats.cop_exec_details),
                    duration.as_nanos() as i64,
                );
            }

            let sr = self.select_resp.as_ref().unwrap();
            if !sr.get_chunks().is_empty() {
                tidb_runtime::checkpoint(&self.kill_hook()).await?;
                break;
            }

            let intermediates = sr.get_intermediate_outputs();
            if !intermediates.is_empty() {
                for output in intermediates {
                    if !output.get_chunks().is_empty() {
                        tidb_runtime::checkpoint(&self.kill_hook()).await?;
                        return Ok(());
                    }
                }
            }
            tidb_runtime::checkpoint(&self.kill_hook()).await?;
        }
        Ok(())
    }

    // go: selectResult.Next L1
    async fn next(&mut self, ctx: &tidb_kv::Context, chk: &mut Chunk) -> Result<(), Error> {
        chk.reset();
        let need_fetch = match &self.select_resp {
            None => true,
            Some(sr) => self.resp_chk_idx == sr.get_chunks().len(),
        };
        if need_fetch {
            self.fetch_resp(ctx).await?;
            if self.select_resp.is_none() {
                return Ok(());
            }
        }
        let encode_type = self.select_resp.as_ref().unwrap().get_encode_type();
        match encode_type {
            tipb::EncodeType::TypeDefault => self.read_from_default(ctx, chk).await,
            tipb::EncodeType::TypeChunk => self.read_from_chunk(ctx, chk).await,
            other => Err(Error::plain(format!("unsupported encode type:{other:?}"))),
        }
    }
}

impl SelectResultImpl {
    // go: selectResult.IntoIter L2 (transfers Option<SelectResultInner> ownership to SelectResultIter, spec §4.5)
    fn take_into_iter(
        &mut self,
        intermediate_field_types: Vec<Vec<Arc<FieldType>>>,
    ) -> Result<Box<dyn SelectResultIter>, Error> {
        let Some(inner) = self.inner.take() else {
            return Err(Error::plain("selectResult is invalid after IntoIter()"));
        };
        Ok(Box::new(new_select_result_iter(
            inner,
            intermediate_field_types,
        )))
    }
}

impl SelectResultInner {
    // go: selectResult.NextRaw L1
    async fn next_raw(&mut self, ctx: &tidb_kv::Context) -> Result<Option<Vec<u8>>, Error> {
        let result_subset = self.resp.next(ctx).await;
        self.partial_count += 1;
        match result_subset {
            Ok(Some(subset)) => Ok(Some(subset.get_data().to_vec())),
            Ok(None) => Ok(None),
            Err(e) => Err(e),
        }
    }

    // go: selectResult.readFromDefault L1
    async fn read_from_default(
        &mut self,
        ctx: &tidb_kv::Context,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        while !chk.is_full() {
            if self.resp_chk_idx == self.select_resp.as_ref().unwrap().get_chunks().len() {
                self.fetch_resp(ctx).await?;
                if self.select_resp.is_none() {
                    return Ok(());
                }
            }
            self.read_rows_data(chk)?;
            let chunk_len = self.select_resp.as_ref().unwrap().get_chunks()[self.resp_chk_idx]
                .get_rows_data()
                .len();
            if self.rows_data_offset >= chunk_len {
                self.resp_chk_idx += 1;
                self.rows_data_offset = 0;
            }
            tidb_runtime::checkpoint(&self.kill_hook()).await?;
        }
        Ok(())
    }

    // go: selectResult.readFromChunk L1
    async fn read_from_chunk(
        &mut self,
        ctx: &tidb_kv::Context,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        if self.resp_chunk_decoder.is_none() {
            let col_types: Vec<FieldType> =
                self.field_types.iter().map(|ft| (**ft).clone()).collect();
            self.resp_chunk_decoder = Some(new_chunk_decoder(
                new_chunk_with_capacity(&self.field_types, 0),
                col_types,
            ));
        }

        while !chk.is_full() {
            if self.resp_chk_idx == self.select_resp.as_ref().unwrap().get_chunks().len() {
                self.fetch_resp(ctx).await?;
                if self.select_resp.is_none() {
                    return Ok(());
                }
            }

            let decoder = self.resp_chunk_decoder.as_mut().unwrap();
            if decoder.is_finished() {
                let rows_data = self.select_resp.as_mut().unwrap().mut_chunks()[self.resp_chk_idx]
                    .take_rows_data();
                decoder.reset(Bytes::from(rows_data));
            }
            // If the next chunk size is greater than required rows * 0.8, reuse the memory of
            // the next chunk and return immediately. Otherwise, splice the data to one chunk
            // and wait the next chunk.
            let threshold = ((chk.required_rows() as f64) * 0.8) as usize;
            if decoder.remained_rows() > threshold {
                if chk.num_rows() > 0 {
                    return Ok(());
                }
                decoder.reuse_interm_chk(chk);
                self.resp_chk_idx += 1;
                return Ok(());
            }
            decoder.decode(chk);
            if decoder.is_finished() {
                self.resp_chk_idx += 1;
            }
            tidb_runtime::checkpoint(&self.kill_hook()).await?;
        }
        Ok(())
    }

    // go: selectResult.updateCopRuntimeStats L1
    fn update_cop_runtime_stats(
        &mut self,
        cop_stats: &CopRuntimeStats,
        resp_time: Duration,
        for_unconsumed_stats: bool,
    ) -> Result<(), Error> {
        let callee = &cop_stats.cop_exec_details.callee_address;
        let Some(coll) = &self.ctx.runtime_stats_coll else {
            return Ok(());
        };
        if self.root_plan_id <= 0 || callee.is_empty() {
            return Ok(());
        }

        // N9: tikvmetrics.ObserveReadSLI has no client-rust counterpart (spec §7 item 10).

        if self.stats.is_none() {
            let mut st = SelectResultRuntimeStats {
                dist_sql_concurrency: self.dist_sql_concurrency,
                ..Default::default()
            };
            if let Some((conc, extra_conc)) = self.resp.cop_concurrency() {
                st.dist_sql_concurrency = conc;
                st.extra_concurrency = extra_conc;
            }
            self.stats = Some(st);
        }
        self.stats
            .as_mut()
            .unwrap()
            .merge_cop_runtime_stats(cop_stats, resp_time);

        let summaries: &[tipb::ExecutorExecutionSummary] = self
            .select_resp
            .as_ref()
            .map_or(&[], |sr| sr.get_execution_summaries());

        // If hasExecutor is true, it means the summary is returned from TiFlash (DEF.MPP).
        let mut has_executor = false;
        for detail in summaries {
            if detail.has_time_processed_ns()
                && detail.has_num_produced_rows()
                && detail.has_num_iterations()
            {
                if detail.has_executor_id() {
                    has_executor = true;
                }
                break;
            }
        }

        let proc_time_ns = cop_stats.cop_exec_details.time_detail.process_time;
        if proc_time_ns > 0 {
            self.ctx
                .cpu_usage
                .merge_tikv_cpu_time(Duration::from_nanos(proc_time_ns as u64));
        }

        if has_executor {
            // DEF.MPP: TiFlash execution summaries are not ported; record cop stats on the
            // last plan ID if present, omitting recordExecutionSummariesForTiFlashTasks.
            if let Some(&last_plan_id) = self.cop_plan_ids.last() {
                coll.record_cop_stats(
                    last_plan_id as i64,
                    self.store_type,
                    Some(&cop_stats.cop_exec_details.scan_detail),
                    cop_stats.cop_exec_details.time_detail,
                    None,
                );
            }
        } else {
            // For cop task cases, we still need this protection.
            if summaries.len() != self.cop_plan_ids.len() {
                if !for_unconsumed_stats && !(self.store_type == TiFlash && summaries.is_empty()) {
                    tidb_base::log_warn!(
                        "invalid cop task execution summaries length",
                        "expected" => self.cop_plan_ids.len(),
                        "received" => summaries.len(),
                    );
                }
                return Ok(());
            }
            let last_idx = self.cop_plan_ids.len().saturating_sub(1);
            for (i, detail) in summaries.iter().enumerate() {
                let summary = (detail.has_time_processed_ns()
                    && detail.has_num_produced_rows()
                    && detail.has_num_iterations())
                .then_some(detail);
                let plan_id = self.cop_plan_ids[i] as i64;
                if i == last_idx {
                    coll.record_cop_stats(
                        plan_id,
                        self.store_type,
                        Some(&cop_stats.cop_exec_details.scan_detail),
                        cop_stats.cop_exec_details.time_detail,
                        summary,
                    );
                } else if let Some(summary) = summary {
                    coll.record_one_cop_task(plan_id, self.store_type, summary);
                }
            }
        }
        Ok(())
    }

    // go: selectResult.readRowsData L1
    fn read_rows_data(&mut self, chk: &mut Chunk) -> Result<(), Error> {
        let full_data =
            self.select_resp.as_ref().unwrap().get_chunks()[self.resp_chk_idx].get_rows_data();
        let mut rows_data = &full_data[self.rows_data_offset..];
        let mut decoder = new_datum_decoder(chk, Arc::clone(&self.ctx.location));
        while !decoder.chunk().is_full() && !rows_data.is_empty() {
            for i in 0..self.row_len {
                rows_data = decoder.decode_one(rows_data, i, &self.field_types[i])?;
            }
        }
        self.rows_data_offset = full_data.len() - rows_data.len();
        Ok(())
    }

    // go: selectResult.memConsume L2 (returns Result on OOM instead of panicking, A1 / M-MEM)
    fn mem_consume(&self, bytes: i64) -> Result<(), Error> {
        if let Some(t) = &self.mem_tracker {
            t.consume(bytes)?;
        }
        Ok(())
    }
}

#[async_trait]
impl SelectResult for SelectResultImpl {
    // glue: SelectResult trait dispatch to selectResult.NextRaw
    async fn next_raw(&mut self, ctx: &tidb_kv::Context) -> Result<Option<Vec<u8>>, Error> {
        self.inner_mut()?.next_raw(ctx).await
    }

    // glue: SelectResult trait dispatch to selectResult.Next
    async fn next(&mut self, ctx: &tidb_kv::Context, chk: &mut Chunk) -> Result<(), Error> {
        self.inner_mut()?.next(ctx, chk).await
    }

    // glue: SelectResult trait dispatch to selectResult.IntoIter
    fn into_iter(
        &mut self,
        intermediate_field_types: Vec<Vec<Arc<FieldType>>>,
    ) -> Result<Box<dyn SelectResultIter>, Error> {
        self.take_into_iter(intermediate_field_types)
    }

    // go: selectResult.Close L1
    fn close(&mut self) -> Result<(), Error> {
        self.inner_mut()?.close_inner()
    }

    // glue: M-IFACE for GetSelectResultConcurrency
    fn cop_concurrency(&self) -> Option<(isize, isize)> {
        self.inner.as_ref()?.resp.cop_concurrency()
    }
}

impl SelectResultInner {
    // go: selectResult.close L1 [M-DEFER, M-MEM]
    fn close_inner(&mut self) -> Result<(), Error> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        tidb_metrics::distsql_partial_num(self.partial_count as f64);
        let resp_size = self.select_resp_size.swap(0, Ordering::SeqCst);
        if resp_size > 0 {
            let _ = self.mem_consume(-resp_size);
        }
        // Release decoded chunk buffers (including any TrackedAlloc columns swapped in by
        // `Decoder::reuse_interm_chk`) and cached response protobuf immediately on close.
        self.resp_chunk_decoder = None;
        self.select_resp = None;
        let unconsumed_cop_stats = self.resp.collect_unconsumed_cop_runtime_stats();
        for cop_stats in unconsumed_cop_stats {
            let _ = self.update_cop_runtime_stats(&cop_stats, Duration::ZERO, true);
            self.ctx
                .exec_details
                .merge_cop_exec_details(Some(&cop_stats.cop_exec_details), 0);
        }
        let close_res = self.resp.close();
        // Mirrors Go's `defer` running after `r.resp.Close()`.
        if let Some(stats) = self.stats.as_mut() {
            if let Some(elapsed) = self.resp.cop_build_task_elapsed() {
                stats.build_task_duration = elapsed.as_nanos() as i64;
            }
            if let Some((batched, fallback)) = self.resp.cop_store_batch_info()
                && (batched != 0 || fallback != 0)
            {
                stats.store_batched_num = batched;
                stats.store_batched_fallback_num = fallback;
            }
            if let Some(coll) = &self.ctx.runtime_stats_coll {
                coll.register_stats(self.root_plan_id as i64, Box::new(stats.clone()));
            }
        }
        close_res
    }
}

pub(crate) struct SelRespChannelIter {
    pub(crate) channel: isize,
    pub(crate) loc: Arc<Location>,
    pub(crate) row_len: usize,
    pub(crate) field_types: Vec<Arc<FieldType>>,
    pub(crate) encode_type: tipb::EncodeType,
    pub(crate) chk_data: Vec<tipb::Chunk>,

    /// Reserved size for each chunk (only for `TypeDefault` encoding).
    pub(crate) reserve_chk_size: usize,
    /// Index of the current chunk in `chk_data`.
    cur_chk_idx: usize,
    /// Byte offset within `chk_data[cur_chk_idx].rows_data` (only for `TypeDefault`).
    cur_rows_offset: usize,
    /// Buffered decoded chunk for the current response (`Arc` for `RowHandle`, M-ROW).
    chk: Option<Arc<Chunk>>,
    /// Next row offset inside `chk`.
    chk_offset: usize,
}

// go: newSelRespChannelIter L1
pub(crate) fn new_sel_resp_channel_iter(
    result: &SelectResultInner,
    channel: isize,
) -> Result<SelRespChannelIter, Error> {
    let sr = result
        .select_resp
        .as_ref()
        .expect("select_resp must be present");
    let intermediate_outputs = sr.get_intermediate_outputs();
    debug_assert_eq!(
        intermediate_outputs.len(),
        result.intermediate_output_types.len()
    );

    let intermediate_outputs_len = intermediate_outputs.len();
    let (row_len, field_types, encode_type, chk_data) = if channel >= 0
        && (channel as usize) < intermediate_outputs_len
    {
        let idx = channel as usize;
        let fts = result.intermediate_output_types[idx].clone();
        let rlen = fts.len();
        let io = &intermediate_outputs[idx];
        (rlen, fts, io.get_encode_type(), io.get_chunks().to_vec())
    } else if channel >= 0 && (channel as usize) == intermediate_outputs_len {
        (
            result.row_len,
            result.field_types.clone(),
            sr.get_encode_type(),
            sr.get_chunks().to_vec(),
        )
    } else {
        return Err(Error::plain(format!(
            "invalid channel {channel} for selectResp with {intermediate_outputs_len} intermediate outputs"
        )));
    };

    Ok(SelRespChannelIter {
        channel,
        loc: Arc::clone(&result.ctx.location),
        row_len,
        field_types,
        encode_type,
        chk_data,
        reserve_chk_size: DefInitChunkSize as usize,
        cur_chk_idx: 0,
        cur_rows_offset: 0,
        chk: None,
        chk_offset: 0,
    })
}

impl SelRespChannelIter {
    // go: selRespChannelIter.Channel L1
    pub(crate) fn channel(&self) -> isize {
        self.channel
    }

    // go: selRespChannelIter.Next L2 (M-ROW: returns Option<SelectResultRow> carrying RowHandle)
    pub(crate) fn next(&mut self) -> Result<Option<SelectResultRow>, Error> {
        if let Some(chk) = &self.chk
            && self.chk_offset < chk.num_rows()
        {
            let idx = self.chk_offset;
            self.chk_offset += 1;
            return Ok(Some(SelectResultRow {
                channel_index: self.channel(),
                row: RowHandle::new(Arc::clone(chk), idx),
            }));
        }

        self.next_chunk()?;
        let Some(chk) = &self.chk else {
            return Ok(None);
        };
        self.chk_offset = 1;
        Ok(Some(SelectResultRow {
            channel_index: self.channel(),
            row: RowHandle::new(Arc::clone(chk), 0),
        }))
    }

    // go: selRespChannelIter.nextChunk L1
    fn next_chunk(&mut self) -> Result<(), Error> {
        self.chk = None;
        let mut working_chk: Option<Chunk> = None;
        while self.cur_chk_idx < self.chk_data.len() {
            let remaining_len = self.chk_data[self.cur_chk_idx]
                .get_rows_data()
                .len()
                .saturating_sub(self.cur_rows_offset);
            if remaining_len == 0 {
                self.cur_chk_idx += 1;
                self.cur_rows_offset = 0;
                continue;
            }

            match self.encode_type {
                tipb::EncodeType::TypeDefault => {
                    let full_data = self.chk_data[self.cur_chk_idx].get_rows_data();
                    let cur_slice = &full_data[self.cur_rows_offset..];
                    let (new_chk, left_len) =
                        self.fill_chunk_from_default(working_chk.take(), cur_slice)?;
                    self.cur_rows_offset = full_data.len() - left_len;
                    let is_not_full = new_chk.num_rows() < new_chk.required_rows();
                    working_chk = Some(new_chk);
                    if is_not_full {
                        continue;
                    }
                }
                tipb::EncodeType::TypeChunk => {
                    let mut chk = new_chunk_with_capacity(&self.field_types, 0);
                    let col_types: Vec<FieldType> =
                        self.field_types.iter().map(|ft| (**ft).clone()).collect();
                    let rows_data = self.chk_data[self.cur_chk_idx].take_rows_data();
                    self.cur_rows_offset = 0;
                    new_chunk_codec(col_types).decode_to_chunk(Bytes::from(rows_data), &mut chk);
                    working_chk = Some(chk);
                }
                other => {
                    return Err(Error::plain(format!("unsupported encode type: {other:?}")));
                }
            }

            if working_chk.as_ref().is_some_and(|c| c.num_rows() > 0) {
                break;
            }
        }
        self.chk = working_chk.filter(|c| c.num_rows() > 0).map(Arc::new);
        Ok(())
    }

    // go: selRespChannelIter.fillChunkFromDefault L1
    fn fill_chunk_from_default(
        &self,
        chk: Option<Chunk>,
        mut rows_data: &[u8],
    ) -> Result<(Chunk, usize), Error> {
        let mut chk = chk
            .unwrap_or_else(|| new_chunk_with_capacity(&self.field_types, self.reserve_chk_size));
        let mut decoder = new_datum_decoder(&mut chk, Arc::clone(&self.loc));
        while !rows_data.is_empty() && decoder.chunk().num_rows() < decoder.chunk().required_rows()
        {
            for i in 0..self.row_len {
                rows_data = decoder.decode_one(rows_data, i, &self.field_types[i])?;
            }
        }
        let left_len = rows_data.len();
        Ok((chk, left_len))
    }
}

struct SelectResultIterImpl {
    result: SelectResultInner,
    channels: Vec<SelRespChannelIter>,
    intermediate_output_types: Vec<Vec<Arc<FieldType>>>,
}

// go: newSelectResultIter L2 (takes ownership of SelectResultInner, spec §4.5)
fn new_select_result_iter(
    result: SelectResultInner,
    intermediate_output_types: Vec<Vec<Arc<FieldType>>>,
) -> SelectResultIterImpl {
    SelectResultIterImpl {
        result,
        channels: Vec::new(),
        intermediate_output_types,
    }
}

#[async_trait]
impl SelectResultIter for SelectResultIterImpl {
    // go: selectResultIter.Next L2 (A1 / #415 go-bug-fixed: decrements select_resp_size before clearing select_resp; N3 cooperative checkpoint)
    async fn next(&mut self, ctx: &tidb_kv::Context) -> Result<Option<SelectResultRow>, Error> {
        loop {
            if self.result.select_resp.is_none() {
                self.result
                    .fetch_resp_with_intermediate_results(
                        ctx,
                        self.intermediate_output_types.clone(),
                    )
                    .await?;

                if self.result.select_resp.is_none() {
                    return Ok(None);
                }

                let num_channels = self.intermediate_output_types.len() + 1;
                if self.channels.capacity() < num_channels {
                    self.channels.reserve(num_channels - self.channels.len());
                }
                for i in 0..num_channels {
                    let ch = new_sel_resp_channel_iter(&self.result, i as isize)?;
                    self.channels.push(ch);
                }
            }

            while let Some(channel) = self.channels.last_mut() {
                // Read channels in reverse order so more complete data (e.g. final rows
                // after push-down IndexLookUp) is consumed before un-looked-up index rows.
                match channel.next()? {
                    Some(row) => return Ok(Some(row)),
                    None => {
                        self.channels.pop();
                    }
                }
            }

            // A1 / #415 (go-bug-fixed): release the consumed response's mem_tracker accounting
            // before clearing select_resp so mem_tracker returns to 0 across responses.
            let prev = self.result.select_resp_size.swap(0, Ordering::SeqCst);
            if prev > 0 {
                let _ = self.result.mem_consume(-prev);
            }
            self.result.select_resp = None;
            tidb_runtime::checkpoint(&self.result.kill_hook()).await?;
        }
    }

    // go: selectResultIter.Close L1
    fn close(&mut self) -> Result<(), Error> {
        self.result.close_inner()
    }
}

/// `selectResultRuntimeStats`: aggregated coprocessor runtime statistics for one root plan.
///
/// Uses `BTreeMap` for `backoff_sleep` so `String()` output order is deterministic
/// (`M-MAPORDER`). `req_stat` (client-go `RegionRequestRuntimeStats`) is omitted per D07.2
/// §6.3 item 4 and D07.3 spec §7 item 4.
#[derive(Clone, Debug, Default)]
pub struct SelectResultRuntimeStats {
    pub cop_resp_time: Percentile<DurationWrapper>,
    pub proc_keys: Percentile<Int64>,
    pub backoff_sleep: BTreeMap<String, i64>,
    pub total_process_time: i64,
    pub total_wait_time: i64,
    pub dist_sql_concurrency: isize,
    pub extra_concurrency: isize,
    pub copr_cache_hit_num: i64,
    pub store_batched_num: u64,
    pub store_batched_fallback_num: u64,
    pub build_task_duration: i64,
}

impl SelectResultRuntimeStats {
    /// Merge one `CopRuntimeStats` sample into self.
    ///
    /// Keeps Go's `maps.Copy` overwrite semantics on `backoff_sleep` (D07.3 spec §8 item 3,
    /// `go-bug-kept`).
    // go: selectResultRuntimeStats.mergeCopRuntimeStats L1 [M-MAPORDER]
    pub fn merge_cop_runtime_stats(&mut self, cop_stats: &CopRuntimeStats, resp_time: Duration) {
        self.cop_resp_time
            .add(DurationWrapper(resp_time.as_nanos() as i64));
        self.proc_keys
            .add(Int64(cop_stats.cop_exec_details.scan_detail.processed_keys));
        if !cop_stats.cop_exec_details.backoff_sleep.is_empty() {
            for (k, &v) in &cop_stats.cop_exec_details.backoff_sleep {
                self.backoff_sleep.insert(k.clone(), v);
            }
        }
        self.total_process_time = self
            .total_process_time
            .wrapping_add(cop_stats.cop_exec_details.time_detail.process_time);
        self.total_wait_time = self
            .total_wait_time
            .wrapping_add(cop_stats.cop_exec_details.time_detail.wait_time);
        if cop_stats.copr_cache_hit {
            self.copr_cache_hit_num += 1;
        }
    }

    /// Format with explicit `copr_cache_enabled` flag (useful for tests/govec without
    /// mutating global config).
    // go-part: selectResultRuntimeStats.String
    pub fn string_with_cache_enabled(&self, copr_cache_enabled: bool) -> String {
        let mut buf = String::new();
        if self.cop_resp_time.size() > 0 {
            let mut cop_resp_time = self.cop_resp_time.clone();
            let mut proc_keys = self.proc_keys.clone();
            let size = cop_resp_time.size();
            if size == 1 {
                let max_d = cop_resp_time.get_percentile(0.0) as i64;
                let pk = proc_keys.get_percentile(0.0);
                let _ = write!(
                    buf,
                    "cop_task: {{num: 1, max: {}, proc_keys: {}",
                    format_duration(max_d),
                    format_float(pk, b'g', -1, 64),
                );
            } else {
                let v_max = cop_resp_time.get_max().0;
                let v_min = cop_resp_time.get_min().0;
                let v_p95 = cop_resp_time.get_percentile(0.95) as i64;
                let sum = cop_resp_time.sum();
                let v_avg = (sum / size as f64) as i64;

                let key_max = proc_keys.get_max().0;
                let key_p95 = proc_keys.get_percentile(0.95) as i64;
                let _ = write!(
                    buf,
                    "cop_task: {{num: {}, max: {}, min: {}, avg: {}, p95: {}",
                    size,
                    format_duration(v_max),
                    format_duration(v_min),
                    format_duration(v_avg),
                    format_duration(v_p95),
                );
                if key_max > 0 {
                    buf.push_str(", max_proc_keys: ");
                    buf.push_str(&key_max.to_string());
                    buf.push_str(", p95_proc_keys: ");
                    buf.push_str(&key_p95.to_string());
                }
            }
            if self.total_process_time > 0 {
                buf.push_str(", tot_proc: ");
                buf.push_str(&format_duration(self.total_process_time));
                if self.total_wait_time > 0 {
                    buf.push_str(", tot_wait: ");
                    buf.push_str(&format_duration(self.total_wait_time));
                }
            }
            if copr_cache_enabled {
                let _ = write!(buf, ", copr_cache_hit_ratio: {:.2}", self.calc_cache_hit());
            } else {
                buf.push_str(", copr_cache: disabled");
            }
            if self.build_task_duration > 0 {
                buf.push_str(", build_task_duration: ");
                buf.push_str(&format_duration(self.build_task_duration));
            }
            if self.dist_sql_concurrency > 0 {
                buf.push_str(", max_distsql_concurrency: ");
                buf.push_str(&self.dist_sql_concurrency.to_string());
            }
            if self.extra_concurrency > 0 {
                buf.push_str(", max_extra_concurrency: ");
                buf.push_str(&self.extra_concurrency.to_string());
            }
            if self.store_batched_num > 0 {
                buf.push_str(", store_batch_num: ");
                buf.push_str(&self.store_batched_num.to_string());
            }
            if self.store_batched_fallback_num > 0 {
                buf.push_str(", store_batch_fallback_num: ");
                buf.push_str(&self.store_batched_fallback_num.to_string());
            }
            buf.push('}');
        }

        // D07.2 §6.3 item 4 / D07.3 spec §7 item 4: no client-go RegionRequestRuntimeStats (`rpc_info`).

        if !self.backoff_sleep.is_empty() {
            buf.push_str(", backoff{");
            for (idx, (k, &d)) in self.backoff_sleep.iter().enumerate() {
                if idx > 0 {
                    buf.push_str(", ");
                }
                let _ = write!(buf, "{k}: {}", format_duration(d));
            }
            buf.push('}');
        }
        buf
    }
}

impl RuntimeStats for SelectResultRuntimeStats {
    // go: selectResultRuntimeStats.Clone L1
    fn clone_stats(&self) -> Box<dyn RuntimeStats> {
        let mut new_rs = SelectResultRuntimeStats {
            cop_resp_time: Percentile::default(),
            proc_keys: Percentile::default(),
            backoff_sleep: self.backoff_sleep.clone(),
            total_process_time: self.total_process_time,
            total_wait_time: self.total_wait_time,
            dist_sql_concurrency: self.dist_sql_concurrency,
            extra_concurrency: self.extra_concurrency,
            copr_cache_hit_num: self.copr_cache_hit_num,
            store_batched_num: self.store_batched_num,
            store_batched_fallback_num: self.store_batched_fallback_num,
            build_task_duration: self.build_task_duration,
        };
        let mut resp = self.cop_resp_time.clone();
        new_rs.cop_resp_time.merge_percentile(&mut resp);
        let mut keys = self.proc_keys.clone();
        new_rs.proc_keys.merge_percentile(&mut keys);
        Box::new(new_rs)
    }

    // go: selectResultRuntimeStats.Merge L1
    fn merge(&mut self, other: &dyn RuntimeStats) {
        let Some(other) = other.as_any().downcast_ref::<SelectResultRuntimeStats>() else {
            return;
        };
        let mut other_resp = other.cop_resp_time.clone();
        self.cop_resp_time.merge_percentile(&mut other_resp);
        let mut other_keys = other.proc_keys.clone();
        self.proc_keys.merge_percentile(&mut other_keys);

        for (k, &v) in &other.backoff_sleep {
            *self.backoff_sleep.entry(k.clone()).or_insert(0) += v;
        }
        self.total_process_time = self
            .total_process_time
            .wrapping_add(other.total_process_time);
        self.total_wait_time = self.total_wait_time.wrapping_add(other.total_wait_time);
        self.copr_cache_hit_num += other.copr_cache_hit_num;
        if other.dist_sql_concurrency > self.dist_sql_concurrency {
            self.dist_sql_concurrency = other.dist_sql_concurrency;
        }
        if other.extra_concurrency > self.extra_concurrency {
            self.extra_concurrency = other.extra_concurrency;
        }
        self.store_batched_num += other.store_batched_num;
        self.store_batched_fallback_num += other.store_batched_fallback_num;
        self.build_task_duration = self
            .build_task_duration
            .wrapping_add(other.build_task_duration);
    }

    // go: selectResultRuntimeStats.String L1 [M-MAPORDER]
    fn string(&self) -> String {
        let cache_enabled = tidb_base::config::get_global_config()
            .tikv_client
            .copr_cache
            .capacity_mb
            > 0.0;
        self.string_with_cache_enabled(cache_enabled)
    }

    // go: selectResultRuntimeStats.Tp L1
    fn tp(&self) -> i64 {
        TpSelectResultRuntimeStats
    }

    // glue: type assertion support for RuntimeStats.Merge
    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl SelectResultRuntimeStats {
    // go: selectResultRuntimeStats.calcCacheHit L1
    pub fn calc_cache_hit(&self) -> f64 {
        let hit = self.copr_cache_hit_num;
        let mut tot = self.cop_resp_time.size();
        if self.store_batched_num > 0 {
            tot += self.store_batched_num as i64;
        }
        if tot == 0 {
            return 0.0;
        }
        hit as f64 / tot as f64
    }
}
