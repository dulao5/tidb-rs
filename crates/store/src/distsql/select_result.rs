//! go-file: pkg/distsql/select_result.go
//!
//! Port of Go TiDB v8.5.8 `pkg/distsql/select_result.go` (block D07.3, Part 1: 18 functions):
//! `SelectResult` trait, `GetSelectResultConcurrency`, `selectResult` (`Next`, `NextRaw`,
//! `fetchResp`, `fetchRespWithIntermediateResults`, `readFromChunk`, `readFromDefault`,
//! `readRowsData`, `updateCopRuntimeStats`, `memConsume`, `Close`, `close`), and
//! `selectResultRuntimeStats` (`mergeCopRuntimeStats`, `Clone`, `Merge`, `String`, `Tp`,
//! `calcCacheHit`).

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
use tidb_base::blocked::blocked;
use tidb_base::dbterror::ClassTiKV;
use tidb_base::gostd::strconv::format_float;
use tidb_base::kv::{StoreType, TiFlash};
use tidb_chunk::chunk::{Chunk, new_chunk_with_capacity};
use tidb_chunk::codec::{Decoder as ChunkDecoder, new_decoder as new_chunk_decoder};
use tidb_chunk::row::RowHandle;
use tidb_codec::codec::new_decoder as new_datum_decoder;
use tidb_execdetails::execdetails::{
    DurationWrapper, Int64, Percentile, RuntimeStats, TpSelectResultRuntimeStats, format_duration,
};
use tidb_kv::copr::CopRuntimeStats;
use tidb_kv::kv::Response;
use tidb_memory::tracker::Tracker;
use tidb_parser::types::FieldType;
use tidb_sessionctx::distsql_ctx::DistSQLContext;

/// SelectResultRow indicates the row returned by [`SelectResultIter`] (implemented in Part 2).
#[derive(Clone, Debug)]
pub struct SelectResultRow {
    pub channel_index: isize,
    pub row: RowHandle,
}

/// SelectResultIter is an iterator used to iterate the rows from [`SelectResult`] (Part 2).
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

/// Concrete [`SelectResult`] wrapping a `kv::Response`.
///
/// Wraps `Option<SelectResultInner>` so Part 2's `into_iter` can move the inner state cleanly
/// and reject subsequent calls with `"selectResult is invalid after IntoIter()"` (D07.3
/// spec §4.5).
pub struct SelectResultImpl {
    inner: Option<SelectResultInner>,
}

struct SelectResultInner {
    label: &'static str,
    resp: Box<dyn Response>,

    row_len: usize,
    field_types: Vec<Arc<FieldType>>,
    intermediate_output_types: Vec<Vec<Arc<FieldType>>>,
    ctx: Arc<DistSQLContext>,

    select_resp: Option<tipb::SelectResponse>,
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
            let _ = self.resp.close();
        }
    }
}

impl SelectResultInner {
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
                let Ok(()) = tidb_runtime::checkpoint(&()).await;
                break;
            }

            let intermediates = sr.get_intermediate_outputs();
            if !intermediates.is_empty() {
                for output in intermediates {
                    if !output.get_chunks().is_empty() {
                        let Ok(()) = tidb_runtime::checkpoint(&()).await;
                        return Ok(());
                    }
                }
            }
            let Ok(()) = tidb_runtime::checkpoint(&()).await;
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
            let Ok(()) = tidb_runtime::checkpoint(&()).await;
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
                    log::warn!(
                        "invalid cop task execution summaries length, expected: {}, received: {}",
                        self.cop_plan_ids.len(),
                        summaries.len()
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

    /// Temporary Part 1 stub; implemented in D07.3 Part 2 (`SelectResultIter`).
    // glue: stub of selectResult.IntoIter, blocked-by:D07.3 (D07.3 Part 2: push-down IndexLookUp SelectResultIter)
    fn into_iter(
        &mut self,
        _intermediate_field_types: Vec<Vec<Arc<FieldType>>>,
    ) -> Result<Box<dyn SelectResultIter>, Error> {
        let _ = self.inner_mut()?;
        Err(blocked("D07.3", "SelectResult::into_iter (D07.3 Part 2)"))
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
