//! go-file: pkg/executor/aggregate/agg_hash_partial_worker.go
//!
//! Partial worker of parallel `HashAggExec` (D12.3 part B2).
//!
//! Mapping differences from Go (spec §4):
//! - `giveBackCh` carries `HashAggInput { chk, worker_idx }` back to the fetcher (worker
//!   index instead of a channel sender so dropping `partial_input_txs` in the fetcher closes
//!   every partial worker's input channel, spec §4.3).
//! - `partialResultsBuffer` stores each row's `final_worker_idx` (`Vec<usize>`) instead of
//!   raw pointers into `partialResultsMap`; `updatePartialResult` indexes
//!   `partialResultsMap[final_worker_idx]` with the group key bytes without re-hashing.
//! - Memory accounting uses A1 `TrackedAlloc` + `mem_tracker.checkpoint()?` (spec §4.6).
//! - Failpoint hooks (`intestDuringPartialWorkerRun`, `intestBeforePartialWorkerRun`, spec
//!   §2.4) are not ported; spill-to-disk methods (`prepareForSpill`, `spillDataToDisk`,
//!   `spillDataToDiskImpl`, `spillRemainingDataToDisk`) belong to Part C (D01.10).

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::AtomicI64;
use std::time::Instant;

use allocator_api2::vec::Vec as AllocVec;
use futures::FutureExt;
use tidb_base::Error;
use tidb_chunk::ColAlloc;
use tidb_chunk::chunk::Chunk;
use tidb_chunk::chunk_in_disk::DataInDiskByChunks;
use tidb_expression::ExprRef;
use tidb_expression::eval::RpnState;
use tidb_expression::expression::EvalState;
use tidb_runtime::{DoneGuard, WaitGroup};
use tidb_sessionctx::exprctx::EvalContextRef;
use tokio::sync::mpsc;

use crate::aggfuncs::{AggPartialResultMapper, PartialResult};
use crate::exec::ExecCtx;

use super::agg_hash_base_worker::{BaseHashAggWorker, wait_finish};
use super::agg_hash_executor::{HashAggInput, nil_agg_func, tracked_key_box};
use super::agg_hash_final_worker::AfFinalResult;
use super::agg_spill::ParallelHashAggSpillHelper;
use super::agg_util::{
    get_group_key, get_group_key_mem_usage, murmur3_sum32, recovery_hash_agg, try_recycle_buffer,
    update_exec_time, update_wait_time, update_worker_time,
};

/// `HashAggPartialWorker` indicates the partial workers of parallel hash agg execution,
/// the number of which is controlled by `tidb_hashagg_partial_concurrency`.
pub struct HashAggPartialWorker {
    pub(crate) base: BaseHashAggWorker,
    pub(crate) worker_id: usize,
    pub(crate) eval_ctx: EvalContextRef,
    pub(crate) enable_vectorized_expression: bool,

    pub(crate) input_rx: mpsc::Receiver<Chunk>,
    pub(crate) output_txs: Vec<mpsc::Sender<AggPartialResultMapper<ColAlloc>>>,
    pub(crate) global_output_tx: mpsc::Sender<AfFinalResult>,

    /// Transmits `HashAggInput` back to the data fetcher.
    pub(crate) give_back_tx: mpsc::Sender<HashAggInput>,

    /// Per-row target final-worker index computed by `get_partial_results_of_each_row`.
    pub(crate) partial_results_buffer: Vec<usize>,
    pub(crate) partial_result_num_in_row: usize,

    /// One `AggPartialResultMapper` per final worker.
    pub(crate) partial_results_map: Vec<AggPartialResultMapper<ColAlloc>>,
    #[allow(dead_code)] // Retained for Part C spill.
    pub(crate) partial_results_map_mem: AtomicI64,

    pub(crate) group_by_items: Vec<ExprRef>,
    pub(crate) group_key_buf: Vec<Vec<u8>>,
    /// Input chunk swapped with chunks received from the fetcher.
    pub(crate) chk: Chunk,

    pub(crate) spill_helper: Arc<ParallelHashAggSpillHelper>,
    pub(crate) spilled_chunks_io: Vec<Arc<DataInDiskByChunks>>,

    pub(crate) inflight_chunk_sync: Arc<WaitGroup>,
    pub(crate) eval_state: EvalState,
    pub(crate) rpn: RpnState,
}

impl HashAggPartialWorker {
    // go: HashAggPartialWorker.getChildInput L2 (async tokio::select! on watch finishCh + mpsc, M-CH)
    async fn get_child_input(&mut self) -> Option<Chunk> {
        tokio::select! {
            _ = wait_finish(&mut self.base.finish_ch) => None,
            chk = self.input_rx.recv() => chk,
        }
    }

    // go: HashAggPartialWorker.fetchChunkAndProcess L2 (async; DoneGuard for inflightChunkSync; no failpoints)
    async fn fetch_chunk_and_process(
        &mut self,
        ctx: &ExecCtx,
        has_error: &mut bool,
        need_shuffle: &mut bool,
    ) -> bool {
        if self.spill_helper.check_error() {
            *has_error = true;
            return false;
        }

        let wait_start = Instant::now();
        let Some(mut chk) = self.get_child_input().await else {
            return false;
        };

        let _inflight_done = DoneGuard(Arc::clone(&self.inflight_chunk_sync));
        update_wait_time(self.base.stats.as_deref(), wait_start);

        self.chk.swap_columns(&mut chk);
        let give_back_mem = chk.memory_usage();
        let input = HashAggInput {
            chk,
            worker_idx: self.worker_id,
        };
        let sent = tokio::select! {
            _ = wait_finish(&mut self.base.finish_ch) => false,
            res = self.give_back_tx.send(input) => res.is_ok(),
        };
        if !sent {
            // Fetcher will not receive this chunk; release its tracked bytes now.
            let _ = self.base.mem_tracker.consume(-give_back_mem);
            return false;
        }

        let exec_start = Instant::now();
        let final_concurrency = self.partial_results_map.len();
        if let Err(err) = self.update_partial_result(ctx, final_concurrency) {
            *has_error = true;
            self.process_error(err).await;
            return false;
        }
        update_exec_time(self.base.stats.as_deref(), exec_start);

        // The intermData is guaranteed non-empty if reaching here.
        *need_shuffle = true;
        true
    }

    // go: HashAggPartialWorker.finalizeWorkerProcess L2 (async drain & shuffle; releases drained chunk memory)
    async fn finalize_worker_process(
        &mut self,
        need_shuffle: bool,
        final_concurrency: usize,
        has_error: bool,
    ) {
        // Consume all remaining chunks to avoid hanging the fetcher.
        while let Some(chk) = self.input_rx.recv().await {
            let _ = self.base.mem_tracker.consume(-chk.memory_usage());
            self.inflight_chunk_sync.done();
        }

        if self.base.check_finish_ch_closed() {
            return;
        }

        if has_error {
            return;
        }

        if need_shuffle && self.spill_helper.is_spilled_chunks_io_empty() {
            self.shuffle_interm_data(final_concurrency).await;
        }
    }

    // go: HashAggPartialWorker.run L2 (async task; catch_unwind + ordered cleanup matching Go defer, spec §4.5)
    pub(crate) async fn run(
        mut self,
        ctx: ExecCtx,
        wait_group: Arc<WaitGroup>,
        final_concurrency: usize,
    ) {
        let start = Instant::now();
        // Ensures wait_group.done() runs even if the task is aborted on Drop.
        let wg_guard = DoneGuard(wait_group);
        let mut has_error = false;
        let mut need_shuffle = false;

        let res = AssertUnwindSafe(async {
            while self
                .fetch_chunk_and_process(&ctx, &mut has_error, &mut need_shuffle)
                .await
            {}
        })
        .catch_unwind()
        .await;

        if let Err(payload) = res {
            recovery_hash_agg(&self.global_output_tx, payload).await;
        }

        self.finalize_worker_process(need_shuffle, final_concurrency, has_error)
            .await;

        // Close any spilled disk handles (empty in Part B) and drop any unshuffled maps so
        // TrackedAlloc returns their bytes before signalling Done.
        for disk in self.spilled_chunks_io.drain(..) {
            disk.close();
        }
        self.partial_results_map.clear();
        self.base.alloc.flush();

        let _ = self.base.mem_tracker.consume(-self.chk.memory_usage());
        update_worker_time(self.base.stats.as_deref(), start);

        // Drop output senders before Done so partialOutputChs close once the last worker exits.
        self.output_txs.clear();
        drop(wg_guard);

        try_recycle_buffer(&mut self.partial_results_buffer, &mut self.group_key_buf);
    }

    /// If the group key has appeared before, reuse its partial result; otherwise allocate
    /// empty partial results in the target final worker's map.
    // go: HashAggPartialWorker.getPartialResultsOfEachRow L2 (A1 TrackedAlloc checkpoint; returns Result for nil AggFunc/quota; stores target final-worker indices)
    fn get_partial_results_of_each_row(&mut self, final_concurrency: usize) -> Result<(), Error> {
        let num_rows = self.group_key_buf.len();
        self.partial_results_buffer.clear();
        let alloc = ColAlloc::Tracked(self.base.alloc.clone());

        let mut any_new = false;
        for i in 0..num_rows {
            let key = self.group_key_buf[i].as_slice();
            let final_worker_idx =
                (murmur3_sum32(key) as isize % final_concurrency as isize) as usize;
            self.partial_results_buffer.push(final_worker_idx);

            if self.partial_results_map[final_worker_idx]
                .m
                .contains_key(key)
            {
                continue;
            }

            any_new = true;
            let mut prs: AllocVec<PartialResult, ColAlloc> =
                AllocVec::with_capacity_in(self.partial_result_num_in_row, alloc.clone());
            for af in &self.base.agg_funcs {
                prs.push(nil_agg_func(af)?.alloc_partial_result());
            }
            let key_box = tracked_key_box(key, alloc.clone());
            self.partial_results_map[final_worker_idx].set(key_box, prs);
        }

        if any_new {
            self.base.alloc.flush();
            self.base.mem_tracker.checkpoint()?;
        }
        Ok(())
    }

    // go: HashAggPartialWorker.updatePartialResult L2 (A1 TrackedAlloc checkpoint; no failpoints)
    fn update_partial_result(
        &mut self,
        ctx: &ExecCtx,
        final_concurrency: usize,
    ) -> Result<(), Error> {
        let _old_mem = get_group_key_mem_usage(&self.group_key_buf);
        let buf = std::mem::take(&mut self.group_key_buf);
        let loc = ctx.stmt.time_zone();
        let err_ctx = ctx.stmt.err_ctx();
        let gk_res = get_group_key(
            &*self.eval_ctx,
            &err_ctx,
            &loc,
            self.enable_vectorized_expression,
            &self.eval_state,
            &mut self.rpn,
            &self.chk,
            buf,
            &self.group_by_items,
        );
        match gk_res {
            Ok(gk) => {
                self.group_key_buf = gk;
                let _new_mem = get_group_key_mem_usage(&self.group_key_buf);
                self.base.mem_tracker.checkpoint()?;
            }
            Err(err) => {
                self.base.mem_tracker.checkpoint()?;
                return Err(err);
            }
        }

        self.get_partial_results_of_each_row(final_concurrency)?;

        let num_rows = self.chk.num_rows();
        let eval_ctx = &*self.eval_ctx;
        for i in 0..num_rows {
            let final_worker_idx = self.partial_results_buffer[i];
            let key = self.group_key_buf[i].as_slice();
            let partial_result = self.partial_results_map[final_worker_idx]
                .m
                .get_mut(key)
                .expect("get_partial_results_of_each_row inserted key");
            let rows = [self.chk.get_row(i)];
            for (j, af) in self.base.agg_funcs.iter().enumerate() {
                nil_agg_func(af)?.update_partial_result(
                    eval_ctx,
                    &self.eval_state,
                    &rows,
                    &mut partial_result[j],
                )?;
            }
        }

        self.base.alloc.flush();
        self.base.mem_tracker.checkpoint()
    }

    // go: HashAggPartialWorker.shuffleIntermData L2 (async mpsc send with finishCh select, M-CH)
    async fn shuffle_interm_data(&mut self, final_concurrency: usize) {
        let maps = std::mem::take(&mut self.partial_results_map);
        for (i, map) in maps.into_iter().take(final_concurrency).enumerate() {
            tokio::select! {
                _ = wait_finish(&mut self.base.finish_ch) => return,
                res = self.output_txs[i].send(map) => {
                    if res.is_err() {
                        return;
                    }
                }
            }
        }
    }

    // go: HashAggPartialWorker.processError L2 (async mpsc send with finishCh select, M-CH)
    pub(crate) async fn process_error(&mut self, err: Error) {
        self.spill_helper.set_error();
        tokio::select! {
            _ = wait_finish(&mut self.base.finish_ch) => {}
            _ = self.global_output_tx.send(AfFinalResult::err(err)) => {}
        }
    }
}
