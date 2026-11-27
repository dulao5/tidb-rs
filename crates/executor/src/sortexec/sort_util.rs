//! go-file: pkg/executor/sortexec/sort_util.go
//!
//! Shared pieces of the sort executors: the spill states, `rowWithError`,
//! `chunkWithMemoryUsage`, `rowWithPartition`, the panic helper, the failpoint helpers
//! (failpoints are not ported: they do nothing) and the cursor over spilled chunks.
//!
//! The cursor reads chunks back from disk (`chunk.DataInDiskByChunks`, the disk container of
//! D01.10, not ported): [`DataInDiskByChunks`] has no values, and [`reload_cursor`] is a
//! boundary stub. The in-memory paths never reach them.

// Go identifiers for package constants (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use std::any::Any;
use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_chunk::chunk::Chunk;
use tidb_chunk::list::RowPtr;
use tidb_chunk::row::{Row, RowHandle};
use tokio::sync::mpsc;

/// `errFailToAddChunk`.
// glue: Go package-level error variable (errors.New, built on use)
pub fn err_fail_to_add_chunk() -> Error {
    tidb_base::errors::new("fail to add chunk")
}

/// signalCheckpointForSort indicates the times of row comparation that a signal detection
/// will be triggered.
pub const signalCheckpointForSort: u64 = 10240;

/// The spill states (`notSpilled`, `needSpill`, `inSpilling`, `spillTriggered`).
pub const notSpilled: i32 = 0;
pub const needSpill: i32 = 1;
pub const inSpilling: i32 = 2;
pub const spillTriggered: i32 = 3;

/// `rowWithPartition`: a row of a multi-way merge and the partition it came from.
///
/// Go's `chunk.Row` points into a chunk the source keeps; here the row is a [`RowHandle`]
/// (mapping rule M-ROW), `None` for Go's empty `Row{}`.
#[derive(Clone, Debug)]
pub struct RowWithPartition {
    pub row: Option<RowHandle>,
    pub partition_id: usize,
}

/// `*chunk.Iterator4Slice` over the rows of the parallel sort: the sorted rows of a worker.
///
/// Go's iterator holds `chunk.Row`s; here they are [`RowHandle`]s, which keep their chunks
/// alive while the rows cross from the worker task to the result generator (mapping rule
/// M-ROW). `None` is Go's empty `Row{}`.
#[derive(Debug, Default)]
pub struct SortedRowsIter {
    rows: Vec<RowHandle>,
    cursor: usize,
}

/// `chunk.NewIterator4Slice(rows)`.
// glue: chunk.NewIterator4Slice over row handles (M-ROW)
pub fn new_sorted_rows_iter(rows: Vec<RowHandle>) -> SortedRowsIter {
    SortedRowsIter { rows, cursor: 0 }
}

impl SortedRowsIter {
    /// `Begin`: resets the cursor and returns the first row.
    // glue: chunk.Iterator4Slice.Begin over row handles (M-ROW)
    pub fn begin(&mut self) -> Option<RowHandle> {
        if self.rows.is_empty() {
            return None;
        }
        self.cursor = 1;
        Some(self.rows[0].clone())
    }

    /// `Next`: the next row, `None` at the end.
    // glue: chunk.Iterator4Slice.Next over row handles (M-ROW)
    #[allow(clippy::should_implement_trait)] // Go's method name; Begin restarts it
    pub fn next(&mut self) -> Option<RowHandle> {
        let l = self.rows.len();
        if self.cursor >= l {
            self.cursor = l + 1;
            return None;
        }
        let row = self.rows[self.cursor].clone();
        self.cursor += 1;
        Some(row)
    }

    /// `Len`.
    // glue: chunk.Iterator4Slice.Len over row handles (M-ROW)
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether there are no rows.
    // glue: Rust is_empty convention
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// `Reset`: the iterator reads `rows` from the start.
    // glue: chunk.Iterator4Slice.Reset over row handles (M-ROW)
    pub fn reset(&mut self, rows: Vec<RowHandle>) {
        self.rows = rows;
        self.cursor = 0;
    }
}

/// The sorted rows of one worker, shared by the worker that fills them and the merge of the
/// result generator that reads them after the worker has finished (Go shares the
/// `*chunk.Iterator4Slice`; mapping rule M-LOCK, never held across an await).
pub type SharedSortedRowsIter = Arc<parking_lot::Mutex<SortedRowsIter>>;

pub use tidb_runtime::wait_group::{DoneGuard, WaitGroup};

/// The receiving side of Go's `finishCh` (`chan struct{}` that is only ever closed): a
/// watch channel whose value turns true when `Close` closes it. A dropped sender (the
/// executor dropped without `Close`) also counts as closed.
#[derive(Clone, Debug)]
pub struct FinishCh(tokio::sync::watch::Receiver<bool>);

/// A new `finishCh`: the sender `Close` closes, and the receiver the tasks wait on.
// glue: make(chan struct{}, 1) as a watch channel (M-CH)
pub fn new_finish_ch() -> (tokio::sync::watch::Sender<bool>, FinishCh) {
    let (tx, rx) = tokio::sync::watch::channel(false);
    (tx, FinishCh(rx))
}

impl FinishCh {
    /// Another receiver of the `finishCh` whose sender is `tx`.
    // glue: a copy of the channel value finishCh (M-CH)
    pub fn from_sender(tx: &tokio::sync::watch::Sender<bool>) -> FinishCh {
        FinishCh(tx.subscribe())
    }

    /// `<-finishCh`: returns once the channel is closed.
    // glue: <-finishCh (M-CH)
    pub async fn closed(&mut self) {
        let _ = self.0.wait_for(|closed| *closed).await;
    }

    /// `select { case <-finishCh: ...; default: }`: whether the channel is closed.
    // glue: non-blocking receive from finishCh (M-CH)
    pub fn is_closed(&self) -> bool {
        *self.0.borrow() || self.0.has_changed().is_err()
    }
}

/// processPanicAndLog: the error of a recovered panic, sent to the result channel and
/// logged.
///
/// `r` is the payload of a panic caught at a task boundary (mapping rule M-PANIC), or the
/// error Go panics with where the Rust code returns it instead (the kill check of a
/// comparison, a memory quota), boxed. The send waits for room in the channel, as Go's does;
/// it returns at once when the receiver is gone (the executor was dropped).
// go: processPanicAndLog L2 (the payload of a caught unwind or a boxed error + captured stack; the send is awaited [M-PANIC, M-CH])
pub async fn process_panic_and_log<R: Send>(
    err_output_chan: &mpsc::Sender<RowWithError<R>>,
    r: Box<dyn Any + Send>,
    bt: Option<String>,
) {
    let err = get_recover_error(r);
    let _ = err_output_chan
        .send(RowWithError {
            row: None,
            err: Some(err.clone()),
        })
        .await;
    let stack = bt.unwrap_or_else(|| {
        tidb_base::errors::format_backtrace_string(&std::backtrace::Backtrace::force_capture())
    });
    tidb_base::log_warn!("executor panicked", "error" => &err, "stack" => &stack);
}

/// `util.GetRecoverError(r)`: an error panic value is the error, another value is formatted
/// with `%v`.
// glue: recover() value to error (Go util.GetRecoverError)
pub fn get_recover_error(r: Box<dyn Any + Send>) -> Error {
    match r.downcast::<Error>() {
        Ok(err) => *err,
        Err(r) => {
            let msg = r
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| r.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "panic".to_string());
            Error::plain(msg)
        }
    }
}

/// chunkWithMemoryUsage contains chunk and memory usage. However, some of memory usage may
/// also come from other place, not only the chunk's memory usage.
///
/// The chunk is shared (`Arc`, mapping rule M-ROW): the worker that receives it keeps rows
/// pointing into it.
pub struct ChunkWithMemoryUsage {
    pub chk: Arc<Chunk>,
    pub memory_usage: i64,
}

/// `rowWithError`: a result row or an error, as the result channels carry them.
///
/// Go's `chunk.Row` points into a chunk the producer keeps; here the row is a [`RowPtr`] into
/// the producer's chunk list, resolved by the consumer that owns the list (the top-n), or a
/// [`RowHandle`] that holds its chunk (the parallel sort, whose rows cross tasks) (mapping
/// rule M-ROW).
#[derive(Debug)]
pub struct RowWithError<R = RowPtr> {
    pub row: Option<R>,
    pub err: Option<Error>,
}

impl<R> Default for RowWithError<R> {
    // glue: Go zero value rowWithError{}
    fn default() -> RowWithError<R> {
        RowWithError {
            row: None,
            err: None,
        }
    }
}

/// injectParallelSortRandomFail: the failpoint `ParallelSortRandomFail` is not ported (no
/// failpoints), so no panic is injected.
// go: injectParallelSortRandomFail L2 (failpoint ParallelSortRandomFail not ported: no failpoints)
pub fn inject_parallel_sort_random_fail(_trigger_factor: i32) {}

/// injectErrorForIssue59655: the failpoint `Issue59655` is not ported (no failpoints), so no
/// error is injected.
// go: injectErrorForIssue59655 L2 (failpoint Issue59655 not ported: no failpoints)
pub fn inject_error_for_issue59655(_trigger_factor: i32) -> Result<(), Error> {
    Ok(())
}

/// injectPanicForIssue63216: the failpoint `Issue63216` is not ported (no failpoints).
// go: injectPanicForIssue63216 L2 (failpoint Issue63216 not ported: no failpoints)
pub fn inject_panic_for_issue63216() {}

pub use tidb_chunk::chunk_in_disk::DataInDiskByChunks;

/// dataCursor: it's used only when spill is triggered.
///
/// Go's `chunk.Iterator4Chunk` borrows its chunk; here the cursor owns the chunk it reads
/// back (`Arc`, so that a row it returned can be kept: mapping rule M-ROW) and keeps the
/// iterator's position.
#[derive(Debug)]
pub struct DataCursor {
    chk_id: isize,
    /// The chunk of `chunkIter` (`nil` at first).
    chk: Option<Arc<Chunk>>,
    /// `chunkIter.cursor`.
    cursor: usize,
    /// `chunkIter.numRows`.
    num_rows: usize,
}

/// NewDataCursor creates a new dataCursor.
// go: NewDataCursor L2 (the chunk iterator is inline: the cursor owns its chunk)
pub fn new_data_cursor() -> DataCursor {
    DataCursor {
        chk_id: -1,
        chk: None,
        cursor: 0,
        num_rows: 0,
    }
}

impl DataCursor {
    // go: dataCursor.getChkID L1
    pub fn get_chk_id(&self) -> isize {
        self.chk_id
    }

    /// `chunkIter.Begin()`: the first row, or the empty row when the chunk has none.
    // go: dataCursor.begin L2 (Iterator4Chunk.Begin inline)
    pub fn begin(&mut self) -> Row<'_> {
        let Some(chk) = &self.chk else {
            return Row::default();
        };
        self.num_rows = chk.num_rows();
        if self.num_rows == 0 {
            return Row::default();
        }
        self.cursor = 1;
        chk.get_row(0)
    }

    /// `chunkIter.Next()`: the next row, or the empty row at the end.
    // go: dataCursor.next L2 (Iterator4Chunk.Next inline)
    #[allow(clippy::should_implement_trait)] // Go's method name; it lends rows, not an Iterator
    pub fn next(&mut self) -> Row<'_> {
        if self.cursor >= self.num_rows {
            self.cursor = self.num_rows + 1;
            return Row::default();
        }
        let chk = self.chk.as_ref().expect("a cursor with rows has a chunk");
        let row = chk.get_row(self.cursor);
        self.cursor += 1;
        row
    }

    /// The row `begin` / `next` returned last, kept (Go keeps the returned row; here those
    /// calls borrow the cursor mutably, so the caller takes the row again); `None` for the
    /// empty row.
    // glue: re-borrow of the row a dataCursor call returned
    pub fn last(&self) -> Option<RowHandle> {
        match &self.chk {
            Some(chk) if self.cursor >= 1 && self.cursor <= self.num_rows => {
                Some(RowHandle::new(Arc::clone(chk), self.cursor - 1))
            }
            _ => None,
        }
    }

    /// `chunkIter.ResetChunk(chk)`: the cursor reads `chk` from its start.
    // go: dataCursor.setChunk L2 (Iterator4Chunk.ResetChunk inline; the cursor takes the chunk)
    pub fn set_chunk(&mut self, chk: Chunk, chk_id: isize) {
        self.chk_id = chk_id;
        self.num_rows = chk.num_rows();
        self.cursor = 0;
        self.chk = Some(Arc::new(chk));
    }
}

/// reloadCursor: loads the next spilled chunk into the cursor; false when all have been read.
// glue: stub of reloadCursor, blocked-by:D01.10 (reading spilled chunks back needs the disk container)
pub fn reload_cursor(
    _cursor: &mut DataCursor,
    _in_disk: Option<&DataInDiskByChunks>,
) -> Result<bool, Error> {
    Err(blocked(
        "D01.10",
        "reading the chunks a sort partition spilled to disk",
    ))
}

/// Helpers of the tests of the merge loops' time-sliced checkpoints (N3-A1 §3.1 item 5).
#[cfg(test)]
pub(crate) mod yield_test {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll};
    use std::time::Duration;

    use tidb_base::mysql::TypeLonglong;
    use tidb_chunk::compare::get_compare_func;
    use tidb_chunk::row::RowHandle;
    use tidb_runtime::{Runtime, RuntimeConfig, SliceConfig};
    use tidb_types::field_type::new_field_type;

    use super::{SharedSortedRowsIter, new_sorted_rows_iter};
    use crate::sortexec::sort::SortKeys;

    /// Counts the polls of the future it wraps.
    pub struct PollCount<F> {
        pub inner: Pin<Box<F>>,
        pub polls: Arc<AtomicUsize>,
    }

    impl<F: Future> Future for PollCount<F> {
        type Output = F::Output;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
            self.polls.fetch_add(1, Ordering::SeqCst);
            self.inner.as_mut().poll(cx)
        }
    }

    /// An instrumented runtime (`ExecPool`) with one thread and a short time slice.
    pub fn runtime() -> Runtime {
        Runtime::new(RuntimeConfig {
            worker_threads: 1,
            slice: SliceConfig {
                quantum: Duration::from_micros(200),
                ..SliceConfig::default()
            },
            ..RuntimeConfig::default()
        })
        .unwrap()
    }

    /// The sort keys of one BIGINT column, ascending.
    pub fn int_keys() -> SortKeys {
        SortKeys {
            key_columns: Arc::from(vec![0usize]),
            key_cmp_funcs: Arc::from(vec![
                get_compare_func(&new_field_type(TypeLonglong)).unwrap(),
            ]),
            descs: Arc::from(vec![false]),
        }
    }

    /// Sysbench's `c` value from the pseudo-random `x`: ten groups of 11 digits joined by
    /// `-`.
    pub fn sysbench_c(mut x: u64) -> String {
        let mut s = String::with_capacity(119);
        for g in 0..10 {
            if g > 0 {
                s.push('-');
            }
            for _ in 0..11 {
                x = x
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                s.push(char::from(b'0' + ((x >> 33) % 10) as u8));
            }
        }
        s
    }

    /// A word of 8 to 23 letters in mixed case from the pseudo-random `x`.
    pub fn mixed_case_word(mut x: u64) -> String {
        let len = 8 + (x >> 59) as usize;
        let mut s = String::with_capacity(len);
        for _ in 0..len {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let c = b'a' + ((x >> 33) % 26) as u8;
            s.push(char::from(if (x >> 40) & 1 == 1 {
                c.to_ascii_uppercase()
            } else {
                c
            }));
        }
        s
    }

    /// `runs` sorted runs of `len` BIGINT rows each, interleaved (run r holds r, r + runs, ...).
    pub fn sorted_runs(runs: usize, len: usize) -> Vec<SharedSortedRowsIter> {
        let ft = new_field_type(TypeLonglong);
        (0..runs)
            .map(|r| {
                let mut chk = tidb_chunk::chunk::new(&[&ft], len, len.max(1));
                for i in 0..len {
                    chk.append_int64(0, (i * runs + r) as i64);
                }
                let chk = Arc::new(chk);
                let rows = (0..len)
                    .map(|i| RowHandle::new(Arc::clone(&chk), i))
                    .collect();
                Arc::new(parking_lot::Mutex::new(new_sorted_rows_iter(rows)))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tidb_base::mysql::TypeLonglong;
    use tidb_types::field_type::new_field_type;

    #[test]
    fn data_cursor_iterates_like_iterator4chunk() {
        let mut c = new_data_cursor();
        assert_eq!(c.get_chk_id(), -1);
        assert!(c.next().is_empty());
        assert!(c.begin().is_empty());
        let ft = new_field_type(TypeLonglong);
        let mut chk = tidb_chunk::chunk::new(&[&ft], 4, 4);
        for i in 0..3 {
            chk.append_int64(0, i);
        }
        c.set_chunk(chk, 0);
        assert_eq!(c.get_chk_id(), 0);
        assert_eq!(c.begin().get_int64(0), 0);
        assert_eq!(c.next().get_int64(0), 1);
        assert_eq!(c.next().get_int64(0), 2);
        assert_eq!(c.last().unwrap().row().get_int64(0), 2);
        assert!(c.next().is_empty());
        assert!(c.next().is_empty());
    }

    #[tokio::test]
    async fn recovered_panic_goes_to_the_channel() {
        let (tx, mut rx) = mpsc::channel::<RowWithError<RowHandle>>(1);
        process_panic_and_log(&tx, Box::new(Error::plain("boom")), None).await;
        let r = rx.recv().await.unwrap();
        assert!(r.row.is_none());
        assert_eq!(r.err.unwrap().to_string(), "boom");
        process_panic_and_log(&tx, Box::new("index out of range"), None).await;
        let r = rx.recv().await.unwrap();
        assert_eq!(r.err.unwrap().to_string(), "index out of range");
    }
}
