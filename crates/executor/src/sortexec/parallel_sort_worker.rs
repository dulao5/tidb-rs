//! go-file: pkg/executor/sortexec/parallel_sort_worker.go
//!
//! A worker of the parallel sort: it receives chunks from the fetcher through the chunk
//! channel, sorts every `maxSortedRowsLimit` rows into a sorted run, and when the channel is
//! closed merges its runs into one sorted sequence that the result generator reads.
//!
//! Mapping (N3-A1, the concurrency exception):
//! - The goroutine is a task (M-GO); the chunk channel is a tokio channel shared by the
//!   workers behind an async lock (M-CH, one receiver for several consumers, as the
//!   coprocessor client does); `finishCh` is a watch channel ([`FinishCh`]).
//! - Rows are [`RowHandle`]s that keep their chunks (M-ROW).
//! - Go panics with the kill error in the comparison (`keyColumnsLess`) and recovers it in
//!   `run`; here the comparison returns the error, the rest of that sort returns false at
//!   once (as the unparallel sort partition does), and `run` sends it as the recovered panic.
//!   A real panic of the task is caught in `run` (M-PANIC: at the task boundary).
//! - Failpoints are not ported (no failpoints).

// Go identifiers for package constants (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use std::any::Any;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use futures::FutureExt;
use tidb_base::Error;
use tidb_base::gostd::sort::sort;
use tidb_chunk::chunk::Chunk;
use tidb_chunk::row::RowHandle;
use tidb_memory::tracker::Tracker;
use tidb_runtime::CheckpointStride;
use tokio::sync::mpsc;

use super::multi_way_merge::{MemorySource, MultiWayMerger, new_multi_way_merger};
use super::parallel_sort_spill_helper::ParallelSortSpillHelper;
use super::sort::SortKeys;
use super::sort_util::{
    ChunkWithMemoryUsage, DoneGuard, FinishCh, RowWithError, SharedSortedRowsIter, WaitGroup,
    inject_parallel_sort_random_fail, new_sorted_rows_iter, process_panic_and_log,
};
use crate::internal::exec::executor::ExecutorKillerHandler;

/// SignalCheckpointForSort indicates the times of row comparation that a signal detection
/// will be triggered.
pub const SignalCheckpointForSort: u64 = 20000;

/// The merged rows between two reads of the clock in the merge loops of the parallel sort
/// (`CheckpointStride`, N3-A1 §3.1 item 5): every 100 rows, where the worker's merge checks
/// the kill signal in Go.
// glue: N3-A1 §3.1 item 5 (per-row loops)
pub const MERGE_CHECKPOINT_STRIDE: u32 = 100;

/// The receiving side of the chunk channel, shared by the workers and `Close` (Go's
/// `chan *chunkWithMemoryUsage` has many receivers; a tokio receiver has one, so it is
/// behind an async lock, M-CH).
pub type ChunkReceiver = Arc<tokio::sync::Mutex<mpsc::Receiver<ChunkWithMemoryUsage>>>;

/// The sending side of the result channel (`chan rowWithError`).
pub type ResultSender = mpsc::Sender<RowWithError<RowHandle>>;

/// parallelSortWorker.
pub struct ParallelSortWorker {
    pub worker_id_for_test: usize,

    chunk_channel: ChunkReceiver,
    fetcher_and_worker_syncer: Arc<WaitGroup>,
    /// `None` once the task has ended: a sender kept here would keep the result channel open
    /// (Go closes the channel explicitly; here it closes when its last sender is dropped).
    err_output_chan: Option<ResultSender>,
    finish_ch: FinishCh,

    less_row_func: SortKeys,
    times_of_row_compare: u64,

    mem_tracker: Arc<Tracker>,
    pub total_memory_usage: i64,

    /// Kept as Go keeps it; only the spill (D01.10) reads it.
    #[allow(dead_code)]
    spill_helper: Arc<ParallelSortSpillHelper>,

    local_sorted_rows: Vec<SharedSortedRowsIter>,
    sorted_rows_iter: Option<SharedSortedRowsIter>,
    max_sorted_rows_limit: usize,
    /// The chunks of `chunkIters` (Go's `[]*chunk.Iterator4Chunk`; the iteration is in
    /// `convertChunksToRows`).
    chunk_iters: Vec<Arc<Chunk>>,
    row_num_in_chunk_iters: usize,
    merger: Option<MultiWayMerger<MemorySource>>,

    sql_killer: Option<ExecutorKillerHandler>,
}

#[allow(clippy::too_many_arguments)] // Go's parameter list
// go: newParallelSortWorker L1
pub fn new_parallel_sort_worker(
    worker_id_for_test: usize,
    less_row_func: SortKeys,
    chunk_channel: ChunkReceiver,
    fetcher_and_worker_syncer: Arc<WaitGroup>,
    err_output_chan: ResultSender,
    finish_ch: FinishCh,
    mem_tracker: Arc<Tracker>,
    sorted_rows_iter: SharedSortedRowsIter,
    max_chunk_size: usize,
    spill_helper: Arc<ParallelSortSpillHelper>,
    sql_killer: Option<ExecutorKillerHandler>,
) -> ParallelSortWorker {
    ParallelSortWorker {
        worker_id_for_test,
        less_row_func,
        chunk_channel,
        fetcher_and_worker_syncer,
        err_output_chan: Some(err_output_chan),
        finish_ch,
        times_of_row_compare: 0,
        mem_tracker,
        total_memory_usage: 0,
        sorted_rows_iter: Some(sorted_rows_iter),
        max_sorted_rows_limit: max_chunk_size * 30,
        spill_helper,
        local_sorted_rows: Vec::new(),
        chunk_iters: Vec::new(),
        row_num_in_chunk_iters: 0,
        merger: None,
        sql_killer,
    }
}

impl ParallelSortWorker {
    // go: parallelSortWorker.reset L1
    pub fn reset(&mut self) -> Result<(), Error> {
        self.local_sorted_rows = Vec::new();
        self.sorted_rows_iter = None;
        self.merger = None;
        self.mem_tracker.replace_bytes_used(0)
    }

    /// The task of the worker has ended: its sender of the result channel is dropped (see
    /// `err_output_chan`).
    // glue: the goroutine's references to the result channel end with it (M-CH)
    pub fn exit(&mut self) {
        self.err_output_chan = None;
    }

    // glue: p.errOutputChan
    fn err_output_chan(&self) -> &ResultSender {
        self.err_output_chan
            .as_ref()
            .expect("the worker's task has not ended")
    }

    // go: parallelSortWorker.injectFailPointForParallelSortWorker L2 (failpoint SlowSomeWorkers not ported: no failpoints)
    fn inject_fail_point_for_parallel_sort_worker(&self, trigger_factor: i32) {
        inject_parallel_sort_random_fail(trigger_factor);
    }

    /// The merge yields at a time-sliced checkpoint (N3-A1 §3.1 item 5) where Go checks the
    /// kill signal, every 100 rows: Rust has no preemption, and a worker merges all its rows
    /// at the end in one loop.
    // go: parallelSortWorker.multiWayMergeLocalSortedRows L2 (async: a time-sliced checkpoint where Go checks the kill signal; failpoint ParallelSortRandomFail not ported: no failpoints)
    async fn multi_way_merge_local_sorted_rows(&mut self) -> Result<Vec<RowHandle>, Error> {
        let mut total_row_num = 0;
        for rows in &self.local_sorted_rows {
            total_row_num += rows.lock().len();
        }
        let mut result_sorted_rows = Vec::with_capacity(total_row_num);
        let source = MemorySource {
            sorted_rows_iters: self.local_sorted_rows.clone(),
        };
        let merger = self
            .merger
            .insert(new_multi_way_merger(source, self.less_row_func.clone()));
        merger.init()?;

        let mut loop_cnt: u64 = 0;
        let mut stride = CheckpointStride::new(MERGE_CHECKPOINT_STRIDE);

        loop {
            if loop_cnt.is_multiple_of(100)
                && let Some(sql_killer) = &self.sql_killer
            {
                sql_killer.handle_sql_killer_signal()?;
            }
            // glue: N3-A1 §3.1 item 5 (a time slice in a per-row loop, beside Go's kill check)
            let Ok(()) = stride.checkpoint(&()).await;

            loop_cnt += 1;

            // It's impossible to return error here as rows are in memory
            let row = merger.next().unwrap_or_default();
            let Some(row) = row else {
                break;
            };
            result_sorted_rows.push(row);
        }
        self.local_sorted_rows = Vec::new();
        Ok(result_sorted_rows)
    }

    // go: parallelSortWorker.convertChunksToRows L2 (Iterator4Chunk inline; the rows are handles [M-ROW])
    fn convert_chunks_to_rows(&mut self) -> Vec<RowHandle> {
        let mut rows = Vec::with_capacity(self.row_num_in_chunk_iters);
        for chk in &self.chunk_iters {
            for i in 0..chk.num_rows() {
                let idx = chk.get_row(i).idx();
                rows.push(RowHandle::new(Arc::clone(chk), idx));
            }
        }
        self.chunk_iters.clear();
        self.row_num_in_chunk_iters = 0;
        rows
    }

    /// The kill error of a comparison (Go's panic) is returned; the rows are dropped, as Go
    /// drops them when it panics out of `slices.SortFunc`.
    // go: parallelSortWorker.sortBatchRows L2 (the kill error short-circuits the sort instead of a panic [M-PANIC])
    fn sort_batch_rows(&mut self) -> Result<(), Error> {
        let mut rows = self.convert_chunks_to_rows();
        // slices.SortFunc(rows, p.keyColumnsLess): the same pattern-defeating quicksort as
        // sort.Sort, with less(a, b) = cmp(a, b) < 0.
        let mut ret: Option<Error> = None;
        sort(&mut rows, |a, b| {
            if ret.is_some() {
                return false;
            }
            match self.key_columns_less(a, b) {
                Ok(cmp) => cmp < 0,
                Err(err) => {
                    ret = Some(err);
                    false
                }
            }
        });
        if let Some(err) = ret {
            return Err(err);
        }
        self.local_sorted_rows
            .push(Arc::new(parking_lot::Mutex::new(new_sorted_rows_iter(
                rows,
            ))));
        Ok(())
    }

    // go: parallelSortWorker.sortLocalRows L2 (async: the merge has a time-sliced checkpoint)
    async fn sort_local_rows(&mut self) -> Result<Vec<RowHandle>, Error> {
        // Handle Remaining batchRows whose row number is not over the `maxSortedRowsLimit`
        if self.row_num_in_chunk_iters > 0 {
            self.sort_batch_rows()?;
        }

        self.multi_way_merge_local_sorted_rows().await
    }

    // go: parallelSortWorker.saveChunk L2 (Iterator4Chunk inline: the chunk is kept)
    fn save_chunk(&mut self, chk: Arc<Chunk>) {
        self.row_num_in_chunk_iters += chk.num_rows();
        self.chunk_iters.push(chk);
    }

    /// Fetching a bunch of chunks from chunkChannel and sort them. After receiving all
    /// chunks, we will get several sorted rows slices and we use k-way merge to sort them.
    ///
    /// The error is the one Go panics with (see `fetchChunksAndSortImpl`).
    // go: parallelSortWorker.fetchChunksAndSort L2 (the kill error of a comparison is returned [M-PANIC])
    async fn fetch_chunks_and_sort(&mut self) -> Result<(), Error> {
        while self.fetch_chunks_and_sort_impl().await? {}
        Ok(())
    }

    /// Returns the kill error a comparison of a full batch raises (Go panics with it and
    /// `run` recovers it). The other errors are sent here, as Go sends them.
    ///
    /// The kill error of the last batch's sort (sorted after the channel is closed) is sent
    /// with the errors of the merge, without the warning Go logs for a recovered panic.
    // go: parallelSortWorker.fetchChunksAndSortImpl L2 (the kill error of a full batch's sort is returned, see the comment [M-PANIC, M-CH])
    async fn fetch_chunks_and_sort_impl(&mut self) -> Result<bool, Error> {
        let received = tokio::select! {
            _ = self.finish_ch.closed() => return Ok(false),
            chk = recv_chunk(&self.chunk_channel) => chk,
        };
        // Memory usage of the chunk has been consumed at the chunk fetcher
        let Some(chk) = received else {
            self.inject_fail_point_for_parallel_sort_worker(100);
            // Put local sorted rows into this iter who will be read by sort executor
            let sorted_rows = match self.sort_local_rows().await {
                Ok(rows) => rows,
                Err(err) => {
                    let _ = self
                        .err_output_chan()
                        .send(RowWithError {
                            row: None,
                            err: Some(err),
                        })
                        .await;
                    return Ok(false);
                }
            };
            self.sorted_rows_iter
                .as_ref()
                .expect("invalid memory address or nil pointer dereference")
                .lock()
                .reset(sorted_rows);
            return Ok(false);
        };
        let _done = DoneGuard(Arc::clone(&self.fetcher_and_worker_syncer));
        self.total_memory_usage += chk.memory_usage;

        self.save_chunk(chk.chk);

        if self.row_num_in_chunk_iters >= self.max_sorted_rows_limit {
            self.sort_batch_rows()?;
        }

        self.inject_fail_point_for_parallel_sort_worker(3);
        Ok(true)
    }

    /// Returns the comparison of the two rows, or the kill error Go panics with.
    // go: parallelSortWorker.keyColumnsLess L2 (the kill error is returned; failpoint SignalCheckpointForSort not ported: no failpoints [M-PANIC])
    fn key_columns_less(&mut self, i: &RowHandle, j: &RowHandle) -> Result<isize, Error> {
        if self.times_of_row_compare >= SignalCheckpointForSort
            && let Some(sql_killer) = &self.sql_killer
        {
            sql_killer.handle_sql_killer_signal()?;

            self.times_of_row_compare = 0;
        }

        self.times_of_row_compare += 1;

        Ok(self.less_row_func.less_row(i.row(), j.row()))
    }

    /// The body of the worker's task. A panic of the task is caught here (M-PANIC, the task
    /// boundary) and sent with the kill error the sort returns.
    // go: parallelSortWorker.run L2 (the panic is a caught unwind or the returned kill error [M-PANIC])
    pub async fn run(&mut self) {
        let ret = AssertUnwindSafe(self.fetch_chunks_and_sort())
            .catch_unwind()
            .await;
        let r: Box<dyn Any + Send> = match ret {
            Ok(Ok(())) => return,
            Ok(Err(err)) => Box::new(err),
            Err(payload) => payload,
        };
        process_panic_and_log(self.err_output_chan(), r).await;
    }
}

/// `<-p.chunkChannel`: `None` once the channel is closed.
// glue: receive from the shared chunk channel (M-CH)
async fn recv_chunk(ch: &ChunkReceiver) -> Option<ChunkWithMemoryUsage> {
    ch.lock().await.recv().await
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tidb_memory::tracker::new_tracker;
    use tokio::sync::mpsc;

    use super::*;
    use crate::sortexec::parallel_sort_spill_helper::new_parallel_sort_spill_helper;
    use crate::sortexec::sort_util::new_finish_ch;
    use crate::sortexec::sort_util::yield_test::{PollCount, int_keys, runtime, sorted_runs};

    /// The final merge of a worker's sorted runs yields at its time-sliced checkpoints
    /// (N3-A1 §3.1 item 5) when it runs in an instrumented task: 8 runs of 40000 rows take
    /// far more than one time slice, and nothing else in the merge awaits. The task runs
    /// unconstrained (no tokio budget), so every extra poll comes from a checkpoint.
    #[test]
    fn merge_of_local_sorted_rows_yields() {
        let rt = runtime();
        let pool = rt.pool().clone();
        let (_chunk_tx, chunk_rx) = mpsc::channel(1);
        let (result_tx, _result_rx) = mpsc::channel(1);
        let (_finish_tx, finish_ch) = new_finish_ch();
        let mut worker = new_parallel_sort_worker(
            0,
            int_keys(),
            Arc::new(tokio::sync::Mutex::new(chunk_rx)),
            Arc::new(WaitGroup::default()),
            result_tx,
            finish_ch,
            new_tracker(-1, -1),
            Arc::new(parking_lot::Mutex::new(new_sorted_rows_iter(Vec::new()))),
            32,
            Arc::new(new_parallel_sort_spill_helper()),
            None,
        );
        worker.local_sorted_rows = sorted_runs(8, 40_000);
        let polls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&polls);
        let rows = rt.block_on(async move {
            pool.spawn(
                "test-merge",
                tokio::task::unconstrained(PollCount {
                    inner: Box::pin(async move {
                        worker.multi_way_merge_local_sorted_rows().await.unwrap()
                    }),
                    polls: counted,
                }),
            )
            .await
            .unwrap()
        });
        assert_eq!(rows.len(), 320_000);
        assert!(
            rows.iter()
                .enumerate()
                .all(|(i, r)| r.row().get_int64(0) == i as i64)
        );
        assert!(polls.load(Ordering::SeqCst) > 1, "the merge never yielded");
    }

    /// The time of one batch sort (`sortBatchRows`, which cannot yield partway through):
    /// 30 * 1024 rows, two BIGINT keys. A measurement for spec D12.5 §7.8, run by hand in a
    /// release build: `cargo test --release -p tidb-executor --lib batch_sort_time --
    /// --ignored --nocapture`.
    #[test]
    #[ignore]
    fn batch_sort_time() {
        let ft = tidb_types::field_type::new_field_type(tidb_base::mysql::TypeLonglong);
        let cmp = || tidb_chunk::compare::get_compare_func(&ft).unwrap();
        let keys = SortKeys {
            key_columns: Arc::from(vec![0usize, 1]),
            key_cmp_funcs: Arc::from(vec![cmp(), cmp()]),
            descs: Arc::from(vec![false, true]),
        };
        let mut times = Vec::new();
        for round in 0..7u64 {
            let (_chunk_tx, chunk_rx) = mpsc::channel(1);
            let (result_tx, _result_rx) = mpsc::channel(1);
            let (_finish_tx, finish_ch) = new_finish_ch();
            let mut worker = new_parallel_sort_worker(
                0,
                keys.clone(),
                Arc::new(tokio::sync::Mutex::new(chunk_rx)),
                Arc::new(WaitGroup::default()),
                result_tx,
                finish_ch,
                new_tracker(-1, -1),
                Arc::new(parking_lot::Mutex::new(new_sorted_rows_iter(Vec::new()))),
                1024,
                Arc::new(new_parallel_sort_spill_helper()),
                None,
            );
            let mut x: u64 = 0x2545_f491_4f6c_dd1d ^ round;
            for _ in 0..30 {
                let mut chk = tidb_chunk::chunk::new(&[&ft, &ft], 1024, 1024);
                for _ in 0..1024 {
                    x = x
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407);
                    chk.append_int64(0, ((x >> 33) % 100) as i64);
                    chk.append_int64(1, ((x >> 13) % 1_000_000) as i64);
                }
                worker.save_chunk(Arc::new(chk));
            }
            assert_eq!(worker.row_num_in_chunk_iters, worker.max_sorted_rows_limit);
            let start = std::time::Instant::now();
            worker.sort_batch_rows().unwrap();
            times.push(start.elapsed());
        }
        times.sort();
        eprintln!(
            "batch sort of 30720 rows, 2 BIGINT keys: min {:?}, median {:?}, max {:?}",
            times[0],
            times[times.len() / 2],
            times[times.len() - 1]
        );
    }

    /// A sort key function that panics inside a worker's task, end to end: the worker's
    /// `run` catches the panic at the task boundary and sends it to the result channel as
    /// an error (Go's recover and `processPanicAndLog`), the counter of the chunk it was
    /// sorting is released by its drop guard during the unwind (Go's deferred `Done`), and
    /// the task ends normally.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn panic_of_a_sort_key_function_goes_to_the_result_channel() {
        let panicking: tidb_chunk::compare::CompareFunc =
            Box::new(|_, _, _, _| panic!("sort key function panicked"));
        let keys = SortKeys {
            key_columns: Arc::from(vec![0usize]),
            key_cmp_funcs: Arc::from(vec![panicking]),
            descs: Arc::from(vec![false]),
        };
        let (chunk_tx, chunk_rx) = mpsc::channel(1);
        let (result_tx, mut result_rx) = mpsc::channel(10);
        let (_finish_tx, finish_ch) = new_finish_ch();
        let syncer = Arc::new(WaitGroup::default());
        // Max chunk size 1: the worker sorts a batch every 30 rows, while it holds the chunk.
        let mut worker = new_parallel_sort_worker(
            0,
            keys,
            Arc::new(tokio::sync::Mutex::new(chunk_rx)),
            Arc::clone(&syncer),
            result_tx,
            finish_ch,
            new_tracker(-1, -1),
            Arc::new(parking_lot::Mutex::new(new_sorted_rows_iter(Vec::new()))),
            1,
            Arc::new(new_parallel_sort_spill_helper()),
            None,
        );
        let task = tokio::spawn(async move {
            worker.run().await;
            worker.exit();
        });
        let ft = tidb_types::field_type::new_field_type(tidb_base::mysql::TypeLonglong);
        let mut chk = tidb_chunk::chunk::new(&[&ft], 40, 40);
        for i in 0..40 {
            chk.append_int64(0, 40 - i);
        }
        syncer.add(1);
        chunk_tx
            .send(ChunkWithMemoryUsage {
                chk: Arc::new(chk),
                memory_usage: 0,
            })
            .await
            .unwrap();
        let got = result_rx.recv().await.expect("the panic is sent");
        assert!(got.row.is_none());
        assert_eq!(
            got.err.expect("an error").to_string(),
            "sort key function panicked"
        );
        task.await.expect("the task ends normally");
        assert_eq!(syncer.count(), 0);
        // The worker's sender is gone with its task, so the channel is closed.
        assert!(result_rx.recv().await.is_none());
    }
}
