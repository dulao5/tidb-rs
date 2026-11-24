//! go-file: pkg/executor/sortexec/sort_util.go
//!
//! Shared pieces of the sort executors: the spill states, `rowWithError`, the panic helper,
//! the failpoint helpers (failpoints are not ported: they do nothing) and the cursor over
//! spilled chunks.
//!
//! The cursor reads chunks back from disk (`chunk.DataInDiskByChunks`, the disk container of
//! D01.10, not ported): [`DataInDiskByChunks`] has no values, and [`reload_cursor`] is a
//! boundary stub. The in-memory paths never reach them.

// Go identifiers for package constants (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use std::any::Any;
use std::collections::VecDeque;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_chunk::chunk::Chunk;
use tidb_chunk::list::RowPtr;
use tidb_chunk::row::Row;

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

/// `rowWithError`: a result row or an error, as the result queues carry them.
///
/// Go's `chunk.Row` points into a chunk the producer keeps; here the row is a [`RowPtr`] into
/// the producer's chunk list (mapping rule M-ROW), resolved by the consumer that owns the
/// list.
#[derive(Debug, Default)]
pub struct RowWithError {
    pub row: Option<RowPtr>,
    pub err: Option<Error>,
}

/// processPanicAndLog: the error of a recovered panic, sent to the result queue and logged.
///
/// No caller recovers a panic (mapping rule M-PANIC: the goroutines of the in-memory paths
/// run on the executor's task, and a panic there unwinds as in any Rust code); kept for the
/// worker tasks of the parallel path.
// go: processPanicAndLog L2 (the payload of a caught unwind; the channel is the result queue [M-PANIC])
pub fn process_panic_and_log(err_output_chan: &mut VecDeque<RowWithError>, r: Box<dyn Any + Send>) {
    let err = get_recover_error(r);
    err_output_chan.push_back(RowWithError {
        row: None,
        err: Some(err.clone()),
    });
    log::warn!("executor panicked, error: {err}");
}

/// `util.GetRecoverError(r)`: an error panic value is the error, another value is formatted
/// with `%v`.
// glue: recover() value to error (Go util.GetRecoverError)
fn get_recover_error(r: Box<dyn Any + Send>) -> Error {
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

/// injectErrorForIssue59655: the failpoint `Issue59655` is not ported (no failpoints), so no
/// error is injected.
// go: injectErrorForIssue59655 L2 (failpoint Issue59655 not ported: no failpoints)
pub fn inject_error_for_issue59655(_trigger_factor: i32) -> Result<(), Error> {
    Ok(())
}

/// injectPanicForIssue63216: the failpoint `Issue63216` is not ported (no failpoints).
// go: injectPanicForIssue63216 L2 (failpoint Issue63216 not ported: no failpoints)
pub fn inject_panic_for_issue63216() {}

/// `chunk.DataInDiskByChunks`, the chunks a partition spilled to disk: the disk container
/// (D01.10) is not ported, so no value of this type exists.
#[derive(Debug)]
pub enum DataInDiskByChunks {}

impl DataInDiskByChunks {
    /// `NumChunks`.
    // glue: method of the unported chunk.DataInDiskByChunks (no value exists)
    pub fn num_chunks(&self) -> usize {
        match *self {}
    }
    /// `Close`.
    // glue: method of the unported chunk.DataInDiskByChunks (no value exists)
    pub fn close(&self) {
        match *self {}
    }
}

/// dataCursor: it's used only when spill is triggered.
///
/// Go's `chunk.Iterator4Chunk` borrows its chunk; here the cursor owns the chunk it reads
/// back and keeps the iterator's position.
#[derive(Debug)]
pub struct DataCursor {
    chk_id: isize,
    /// The chunk of `chunkIter` (`nil` at first).
    chk: Option<Chunk>,
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

    /// The row `begin` / `next` returned last (Go keeps the returned row; here those calls
    /// borrow the cursor mutably, so the caller takes the row again).
    // glue: re-borrow of the row a dataCursor call returned
    pub fn last(&self) -> Row<'_> {
        match &self.chk {
            Some(chk) if self.cursor >= 1 && self.cursor <= self.num_rows => {
                chk.get_row(self.cursor - 1)
            }
            _ => Row::default(),
        }
    }

    /// `chunkIter.ResetChunk(chk)`: the cursor reads `chk` from its start.
    // go: dataCursor.setChunk L2 (Iterator4Chunk.ResetChunk inline; the cursor takes the chunk)
    pub fn set_chunk(&mut self, chk: Chunk, chk_id: isize) {
        self.chk_id = chk_id;
        self.num_rows = chk.num_rows();
        self.cursor = 0;
        self.chk = Some(chk);
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
        assert_eq!(c.last().get_int64(0), 2);
        assert!(c.next().is_empty());
        assert!(c.next().is_empty());
    }

    #[test]
    fn recovered_panic_goes_to_the_queue() {
        let mut q = VecDeque::new();
        process_panic_and_log(&mut q, Box::new("boom"));
        let r = q.pop_front().unwrap();
        assert!(r.row.is_none());
        assert_eq!(r.err.unwrap().to_string(), "boom");
    }
}
