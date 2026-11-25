//! go-file: pkg/executor/aggregate/agg_hash_final_worker.go
//!
//! Final worker of parallel `HashAggExec` (D12.3 part B2).
//!
//! Mapping differences from Go (spec §4):
//! - Per-row loops (`mergeInputIntoResultMap` and `generateResultAndSend`) include
//!   `CheckpointStride` (N3-A1 §3.1 item 5, spec §4.7) so long merges and emissions yield
//!   cooperatively.
//! - Memory accounting uses A1 `TrackedAlloc` + `mem_tracker.checkpoint()?` (spec §4.6).
//! - Disk-restore branches (`!isSpilledChunksIOEmpty()`) return `blocked-by:D01.10` until
//!   Part C (`getInputFromDisk`, `restoreDataFromDisk`).
//! - Failpoint hooks (`intestBeforeFinalWorkerStart`, `intestDuringFinalWorkerRun`, spec
//!   §2.4) are not ported.

use std::any::Any;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Instant;

use futures::FutureExt;
use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_chunk::ColAlloc;
use tidb_chunk::chunk::Chunk;
use tidb_runtime::{CheckpointHook, CheckpointStride, DoneGuard, WaitGroup};
use tidb_sessionctx::exprctx::EvalContextRef;
use tokio::sync::mpsc;

use crate::aggfuncs::AggPartialResultMapper;
use crate::internal::exec::executor::ExecutorKillerHandler;

use super::agg_hash_base_worker::{BaseHashAggWorker, wait_finish};
use super::agg_hash_executor::nil_agg_func;
use super::agg_spill::ParallelHashAggSpillHelper;
use super::agg_util::{
    AggWorkerStat, recovery_hash_agg, update_exec_time, update_wait_time, update_worker_time,
};

/// Checkpoint stride for per-group loops in the final worker (spec §4.7, N3-A1 §3.1 item 5).
const FINAL_WORKER_CHECKPOINT_STRIDE: u32 = 100;

/// `AfFinalResult` carries either a finalized result chunk (with its give-back channel) or
/// an error from a worker/fetcher to the main `parallelExec` loop.
pub struct AfFinalResult {
    pub(crate) chk: Option<Chunk>,
    pub(crate) err: Option<Error>,
    pub(crate) give_back_tx: Option<mpsc::Sender<Chunk>>,
}

impl AfFinalResult {
    // glue: Go composite literal &AfFinalResult{err: err}
    pub(crate) fn err(err: Error) -> Self {
        Self {
            chk: None,
            err: Some(err),
            give_back_tx: None,
        }
    }

    // glue: Go composite literal &AfFinalResult{chk: chk, giveBackCh: giveBackCh}
    pub(crate) fn chunk(chk: Chunk, give_back_tx: mpsc::Sender<Chunk>) -> Self {
        Self {
            chk: Some(chk),
            err: None,
            give_back_tx: Some(give_back_tx),
        }
    }
}

/// Drop guard recording wait time on every exit path of `getPartialInput` (spec §4.5).
struct WaitTimeGuard<'a> {
    stats: Option<&'a AggWorkerStat>,
    start: Instant,
}

impl Drop for WaitTimeGuard<'_> {
    // glue: defer updateWaitTime(w.stats, waitStart) (M-DEFER, spec §4.5)
    fn drop(&mut self) {
        update_wait_time(self.stats, self.start);
    }
}

/// `CheckpointHook` checking the statement's `SqlKiller` inside final-worker loops (spec §4.7).
struct KillerHook<'a>(&'a ExecutorKillerHandler);

impl CheckpointHook for KillerHook<'_> {
    type Error = Error;

    // glue: M-KILL CheckpointHook checking ExecutorKillerHandler
    fn check(&self) -> Result<(), Error> {
        self.0.handle_sql_killer_signal()
    }
}

/// `HashAggFinalWorker` indicates the final workers of parallel hash agg execution,
/// the number of which is controlled by `tidb_hashagg_final_concurrency`.
pub struct HashAggFinalWorker {
    pub(crate) base: BaseHashAggWorker,

    pub(crate) partial_result_map: AggPartialResultMapper<ColAlloc>,
    pub(crate) input_rx: mpsc::Receiver<AggPartialResultMapper<ColAlloc>>,
    pub(crate) output_tx: mpsc::Sender<AfFinalResult>,
    pub(crate) final_result_holder_rx: mpsc::Receiver<Chunk>,
    pub(crate) final_result_holder_tx: mpsc::Sender<Chunk>,

    pub(crate) spill_helper: Arc<ParallelHashAggSpillHelper>,
    pub(crate) eval_ctx: EvalContextRef,
    pub(crate) sql_killer: ExecutorKillerHandler,
}

impl HashAggFinalWorker {
    // go: HashAggFinalWorker.getPartialInput L2 (async tokio::select!; WaitTimeGuard for defer updateWaitTime)
    async fn get_partial_input(&mut self) -> Option<AggPartialResultMapper<ColAlloc>> {
        let _wait_guard = WaitTimeGuard {
            stats: self.base.stats.as_deref(),
            start: Instant::now(),
        };
        tokio::select! {
            _ = wait_finish(&mut self.base.finish_ch) => None,
            input = self.input_rx.recv() => input,
        }
    }

    // go: HashAggFinalWorker.mergeInputIntoResultMap L2 (async: CheckpointStride in merge loop per spec §4.7; A1 TrackedAlloc checkpoint)
    async fn merge_input_into_result_map(
        &mut self,
        input: AggPartialResultMapper<ColAlloc>,
    ) -> Result<(), Error> {
        // As w.partialResultMap is empty when we get the first input, directly assign it.
        if self.partial_result_map.m.is_empty() {
            self.partial_result_map = input;
            return Ok(());
        }

        let exec_start = Instant::now();
        let eval_ctx = &*self.eval_ctx;
        let killer = self.sql_killer.clone();
        let kill_hook = KillerHook(&killer);
        let mut stride = CheckpointStride::new(FINAL_WORKER_CHECKPOINT_STRIDE);

        for (key, mut value) in input.m {
            stride.checkpoint(&kill_hook).await?;
            if let Some(dst_val) = self.partial_result_map.m.get_mut(&key) {
                for (j, af) in self.base.agg_funcs.iter().enumerate() {
                    nil_agg_func(af)?.merge_partial_result(
                        eval_ctx,
                        &mut value[j],
                        &mut dst_val[j],
                    )?;
                }
            } else {
                self.partial_result_map.set(key, value);
            }
        }

        self.base.alloc.flush();
        self.base.mem_tracker.checkpoint()?;
        update_exec_time(self.base.stats.as_deref(), exec_start);
        Ok(())
    }

    // go: HashAggFinalWorker.consumeIntermData L2 (async; no failpoint)
    async fn consume_interm_data(&mut self) -> Result<(), Error> {
        while let Some(input) = self.get_partial_input().await {
            self.merge_input_into_result_map(input).await?;
        }
        Ok(())
    }

    /// Appends finalized results to `result`, flushing whenever `result.is_full()`. Returns
    /// `Some(result)` with the remaining unflushed chunk, or `None` if `finish_ch` closed
    /// while waiting for a fresh result holder.
    // go: HashAggFinalWorker.generateResultAndSend L2 (async: CheckpointStride with KillerHook M-KILL per spec §4.7; finishCh-aware sends; R21 / #384: propagates AppendFinalResult2Chunk errors instead of swallowing them)
    async fn generate_result_and_send(
        &mut self,
        mut result: Chunk,
    ) -> Result<Option<Chunk>, Error> {
        let mut stride = CheckpointStride::new(FINAL_WORKER_CHECKPOINT_STRIDE);

        // Drain keys/values out of the map so partial results are freed as rows are emitted.
        let mut map = std::mem::replace(
            &mut self.partial_result_map,
            crate::aggfuncs::new_agg_partial_result_mapper_in(
                ColAlloc::Tracked(self.base.alloc.clone()),
                0,
            ),
        );
        let killer = self.sql_killer.clone();
        let kill_hook = KillerHook(&killer);
        for (_, mut results) in map.m.drain() {
            stride.checkpoint(&kill_hook).await?;
            let eval_ctx = &*self.eval_ctx;
            for (j, af) in self.base.agg_funcs.iter().enumerate() {
                let f = nil_agg_func(af)?;
                // Ruling R21 (#384, owner 2026-11-25): propagate AppendFinalResult2Chunk errors
                // instead of Go v8.5.8's log-and-continue (which misaligns chunk columns or
                // panics on out-of-range reads), matching unparallelExec and StreamAggExec.
                f.append_final_result2_chunk(eval_ctx, &mut results[j], &mut result)?;
            }

            if self.base.agg_funcs.is_empty() {
                result.set_num_virtual_rows(result.num_rows() + 1);
            }

            if result.is_full() {
                let send_msg = AfFinalResult::chunk(result, self.final_result_holder_tx.clone());
                let sent = tokio::select! {
                    _ = wait_finish(&mut self.base.finish_ch) => false,
                    res = self.output_tx.send(send_msg) => res.is_ok(),
                };
                if !sent {
                    return Ok(None);
                }
                let Some(next_result) = self.receive_final_result_holder().await else {
                    return Ok(None);
                };
                result = next_result;
            }
        }
        Ok(Some(result))
    }

    // go: HashAggFinalWorker.sendFinalResult L2 (async; disk restore branch is blocked-by:D01.10; no failpoint)
    async fn send_final_result(&mut self) {
        let wait_start = Instant::now();
        let Some(result) = self.receive_final_result_holder().await else {
            update_wait_time(self.base.stats.as_deref(), wait_start);
            return;
        };
        update_wait_time(self.base.stats.as_deref(), wait_start);

        let exec_start = Instant::now();
        update_exec_time(self.base.stats.as_deref(), exec_start);

        if self.spill_helper.is_spilled_chunks_io_empty() {
            match self.generate_result_and_send(result).await {
                Ok(Some(result)) => {
                    let send_msg =
                        AfFinalResult::chunk(result, self.final_result_holder_tx.clone());
                    tokio::select! {
                        _ = wait_finish(&mut self.base.finish_ch) => {}
                        _ = self.output_tx.send(send_msg) => {}
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    tokio::select! {
                        _ = wait_finish(&mut self.base.finish_ch) => {}
                        _ = self.output_tx.send(AfFinalResult::err(err)) => {}
                    }
                }
            }
        } else {
            // glue: stub of HashAggFinalWorker.sendFinalResult disk restore, blocked-by:D01.10 (restoring spilled partitions from disk)
            let err = blocked(
                "D01.10",
                "restoring spilled parallel hash aggregation partitions from disk",
            );
            tokio::select! {
                _ = wait_finish(&mut self.base.finish_ch) => {}
                _ = self.output_tx.send(AfFinalResult::err(err)) => {}
            }
        }
    }

    // go: HashAggFinalWorker.receiveFinalResultHolder L2 (async tokio::select! on watch finishCh + mpsc, M-CH)
    async fn receive_final_result_holder(&mut self) -> Option<Chunk> {
        tokio::select! {
            _ = wait_finish(&mut self.base.finish_ch) => None,
            result = self.final_result_holder_rx.recv() => result,
        }
    }

    // go: HashAggFinalWorker.run L2 (async task; catch_unwind + cleanup matching Go defer; disk restore branch blocked-by:D01.10)
    pub(crate) async fn run(
        mut self,
        wait_group: Arc<WaitGroup>,
        partial_worker_waiter: Arc<WaitGroup>,
    ) {
        let start = Instant::now();
        let wg_guard = DoneGuard(wait_group);

        let res = AssertUnwindSafe(async {
            partial_worker_waiter.wait().await;

            if self.spill_helper.is_spilled_chunks_io_empty() {
                if let Err(err) = self.consume_interm_data().await {
                    tokio::select! {
                        _ = wait_finish(&mut self.base.finish_ch) => {}
                        _ = self.output_tx.send(AfFinalResult::err(err)) => {}
                    }
                    return;
                }
            } else {
                if self.spill_helper.check_error() {
                    return;
                }
                // glue: stub of HashAggFinalWorker.run disk restore branch, blocked-by:D01.10 (reading spilled partitions from disk)
                let err = blocked(
                    "D01.10",
                    "reading spilled parallel hash aggregation partitions from disk",
                );
                tokio::select! {
                    _ = wait_finish(&mut self.base.finish_ch) => {}
                    _ = self.output_tx.send(AfFinalResult::err(err)) => {}
                }
                return;
            }
            self.send_final_result().await;
        })
        .catch_unwind()
        .await;

        self.cleanup(start, res.err(), wg_guard).await;
    }

    // go: HashAggFinalWorker.cleanup L2 (async panic recovery send; drops tracked map before Done)
    async fn cleanup(
        mut self,
        start: Instant,
        panic_payload: Option<Box<dyn Any + Send>>,
        wg_guard: DoneGuard,
    ) {
        if let Some(r) = panic_payload {
            recovery_hash_agg(&self.output_tx, r).await;
        }
        // Release partialResultMap bytes into TrackedAlloc before Done.
        self.partial_result_map.m.clear();
        self.partial_result_map.m.shrink_to_fit();
        self.base.alloc.flush();
        update_worker_time(self.base.stats.as_deref(), start);
        drop(wg_guard);
    }
}
