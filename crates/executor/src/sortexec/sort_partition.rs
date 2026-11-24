//! go-file: pkg/executor/sortexec/sort_partition.go
//!
//! `sortPartition`: the rows of one partition of the unparallel sort, kept in memory and
//! sorted there.
//!
//! - Go keeps `[]chunk.Row` pointing into the chunks it was given; here the partition keeps
//!   the chunks (`Arc<Chunk>`, mapping rule M-ROW) and a [`RowPtr`] per row.
//! - Go guards the partition with `syncLock` and the spill state with `cond`, because the
//!   spill action runs on another goroutine. The partition has one owner here and the spill
//!   action is never triggered (it is not registered: the session memory tracker of the memory exception A1), so
//!   both locks are dropped.
//! - Spilling (`spillToDisk`, `spillToDiskImpl`) is not in this part: a partition never
//!   reaches the spilled state, and reading spilled rows back stops at the cursor's boundary
//!   stub (D01.10).

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::gostd::sort::sort;
use tidb_chunk::chunk::Chunk;
use tidb_chunk::compare::CompareFunc;
use tidb_chunk::list::RowPtr;
use tidb_chunk::row::{Row, RowSize};
use tidb_memory::tracker::{LabelForSortPartition, Tracker, new_tracker};
use tidb_parser::types::FieldType;

use super::sort_spill::SortPartitionSpillDiskAction;
use super::sort_util::{
    DataCursor, DataInDiskByChunks, new_data_cursor, notSpilled, reload_cursor,
    signalCheckpointForSort, spillTriggered,
};

/// sortPartition: one partition of the unparallel sort.
pub struct SortPartition {
    spill_status: i32,

    /// The chunks the rows of `savedRows` are in.
    chunks: Vec<Arc<Chunk>>,
    /// Data are stored in savedRows.
    saved_rows: Vec<RowPtr>,
    /// `sliceIter`: the position of the next row of `savedRows` to return (`nil` until the
    /// rows are sorted).
    slice_iter: Option<usize>,
    is_sorted: bool,

    /// cursor iterates the spilled chunks.
    cursor: DataCursor,
    in_disk: Option<DataInDiskByChunks>,

    spill_error: Option<Error>,
    closed: bool,

    field_types: Vec<Arc<FieldType>>,

    mem_tracker: Arc<Tracker>,
    disk_tracker: Arc<Tracker>,
    spill_action: Option<Arc<SortPartitionSpillDiskAction>>,

    /// We can't spill if size of data is lower than the limit.
    spill_limit: i64,

    by_items_desc: Arc<[bool]>,
    /// keyColumns is the column index of the by items.
    key_columns: Arc<[usize]>,
    /// keyCmpFuncs is used to compare each ByItem.
    key_cmp_funcs: Arc<[CompareFunc]>,

    /// Sort is a time-consuming operation, we need to set a checkpoint to detect the outside
    /// signal periodically.
    times_of_row_compare: u64,

    file_name_prefix_for_test: String,
}

/// Creates a new SortPartition in memory.
// go: newSortPartition L2 (no locks: one owner, see the file comment)
pub fn new_sort_partition(
    field_types: Vec<Arc<FieldType>>,
    by_items_desc: Arc<[bool]>,
    key_columns: Arc<[usize]>,
    key_cmp_funcs: Arc<[CompareFunc]>,
    spill_limit: i64,
    file_name_prefix_for_test: String,
) -> SortPartition {
    SortPartition {
        spill_error: None,
        spill_status: notSpilled,
        field_types,
        chunks: Vec::new(),
        saved_rows: Vec::new(),
        slice_iter: None,
        is_sorted: false,
        in_disk: None, // It's initialized only when spill is triggered
        mem_tracker: new_tracker(LabelForSortPartition, -1),
        disk_tracker: new_tracker(LabelForSortPartition, -1),
        spill_action: None, // It's set in `actionSpill` function
        spill_limit,
        by_items_desc,
        key_columns,
        key_cmp_funcs,
        cursor: new_data_cursor(),
        closed: false,
        times_of_row_compare: 0,
        file_name_prefix_for_test,
    }
}

impl SortPartition {
    // go: sortPartition.close L2 (no lock: one owner)
    pub fn close(&mut self) -> Result<(), Error> {
        self.closed = true;
        if let Some(in_disk) = &self.in_disk {
            in_disk.close();
        }
        self.get_mem_tracker().replace_bytes_used(0)
    }

    /// Return false if the spill is triggered in this partition.
    ///
    /// The memory tracker returns the kill or quota error Go panics with (A1).
    // go: sortPartition.add L2 (no lock: one owner; the tracker's error is returned [M-ROW, M-MEM])
    pub fn add(&mut self, chk: &Arc<Chunk>) -> Result<bool, Error> {
        let row_num = chk.num_rows();
        let consumed_bytes_num = RowSize
            .wrapping_mul(row_num as i64)
            .wrapping_add(chk.memory_usage());

        if self.is_spill_triggered() {
            return Ok(false);
        }

        // Convert chunk to rows
        let chk_idx = self.chunks.len() as u32;
        self.chunks.push(Arc::clone(chk));
        for i in 0..row_num {
            self.saved_rows.push(RowPtr {
                chk_idx,
                row_idx: i as u32,
            });
        }

        self.get_mem_tracker().consume(consumed_bytes_num)?;
        Ok(true)
    }

    // go: sortPartition.sort L2 (no lock: one owner)
    pub fn sort(&mut self) -> Result<(), Error> {
        self.sort_no_lock()
    }

    /// Go recovers the panic the kill check raises during the sort (`HandleKillSignal`) and
    /// returns it; here the check returns the error, the remaining comparisons of the sort
    /// return false at once, and the error is returned. The rows are left unsorted, as Go
    /// leaves them when it panics out of `sort.Slice`.
    // go: sortPartition.sortNoLock L2 (the kill error short-circuits the sort instead of a panic; failpoint errorDuringSortRowContainer not ported: no failpoints [M-PANIC])
    pub fn sort_no_lock(&mut self) -> Result<(), Error> {
        if self.is_sorted {
            return Ok(());
        }

        let mut rows = std::mem::take(&mut self.saved_rows);
        let mut ret: Option<Error> = None;
        sort(&mut rows, |&i, &j| {
            if ret.is_some() {
                return false;
            }
            match self.key_columns_less(i, j) {
                Ok(less) => less,
                Err(err) => {
                    ret = Some(err);
                    false
                }
            }
        });
        self.saved_rows = rows;
        if let Some(err) = ret {
            return Err(err);
        }
        self.is_sorted = true;
        self.slice_iter = Some(0);
        Ok(())
    }

    /// The next sorted row; the empty row once all rows have been returned.
    // go: sortPartition.getNextSortedRow L2 (no lock: one owner; Iterator4Slice.Next inline)
    pub fn get_next_sorted_row(&mut self) -> Result<Row<'_>, Error> {
        if self.is_spill_triggered() {
            if self.cursor.next().is_empty() {
                let success = reload_cursor(&mut self.cursor, self.in_disk.as_ref())?;
                if !success {
                    // All data has been consumed
                    return Ok(Row::default());
                }

                if self.cursor.begin().is_empty() {
                    return Err(tidb_base::errors::new("Get an empty row"));
                }
            }
            return Ok(self.cursor.last());
        }

        // s.sliceIter.Next()
        let it = self
            .slice_iter
            .as_mut()
            .expect("invalid memory address or nil pointer dereference");
        let l = self.saved_rows.len();
        if *it >= l {
            *it = l + 1;
            return Ok(Row::default());
        }
        let ptr = self.saved_rows[*it];
        *it += 1;
        Ok(self.chunks[ptr.chk_idx as usize].get_row(ptr.row_idx as usize))
    }

    // go: sortPartition.actionSpill L1
    pub fn action_spill(&mut self) -> Arc<SortPartitionSpillDiskAction> {
        Arc::clone(
            self.spill_action
                .get_or_insert_with(|| Arc::new(SortPartitionSpillDiskAction::default())),
        )
    }

    // go: sortPartition.getMemTracker L1
    pub fn get_mem_tracker(&self) -> &Arc<Tracker> {
        &self.mem_tracker
    }

    // go: sortPartition.getDiskTracker L1
    pub fn get_disk_tracker(&self) -> &Arc<Tracker> {
        &self.disk_tracker
    }

    // go: sortPartition.lessRow L2 (every compare func is set: initCompareFuncs fails on a nil one)
    pub fn less_row(&self, row_i: Row<'_>, row_j: Row<'_>) -> bool {
        for (i, &col_idx) in self.key_columns.iter().enumerate() {
            let cmp_func = &self.key_cmp_funcs[i];
            let mut cmp = cmp_func(row_i, col_idx, row_j, col_idx);
            if self.by_items_desc[i] {
                cmp = cmp.wrapping_neg();
            }
            if cmp < 0 {
                return true;
            } else if cmp > 0 {
                return false;
            }
        }
        false
    }

    /// keyColumnsLess is the less function for key columns.
    ///
    /// Takes the two rows (Go takes their indexes in `savedRows`, which the sort permutes).
    /// The kill check returns the error Go panics with.
    // go: sortPartition.keyColumnsLess L2 (rows instead of slice indexes; the kill error is returned; failpoint SignalCheckpointForSort not ported: no failpoints [M-PANIC])
    pub fn key_columns_less(&mut self, i: RowPtr, j: RowPtr) -> Result<bool, Error> {
        if self.times_of_row_compare >= signalCheckpointForSort {
            // Trigger Consume for checking the NeedKill signal
            self.mem_tracker.handle_kill_signal()?;
            self.times_of_row_compare = 0;
        }

        self.times_of_row_compare += 1;
        let row_i = self.chunks[i.chk_idx as usize].get_row(i.row_idx as usize);
        let row_j = self.chunks[j.chk_idx as usize].get_row(j.row_idx as usize);
        Ok(self.less_row(row_i, row_j))
    }

    // go: sortPartition.isSpillTriggered L2 (no lock: one owner)
    pub fn is_spill_triggered(&self) -> bool {
        self.spill_status == spillTriggered
    }

    // go: sortPartition.setError L2 (no lock: one owner)
    pub fn set_error(&mut self, err: Option<Error>) {
        self.spill_error = err;
    }

    // go: sortPartition.checkError L2 (no lock: one owner)
    pub fn check_error(&self) -> Result<(), Error> {
        match &self.spill_error {
            Some(err) => Err(err.clone()),
            None => Ok(()),
        }
    }

    /// `numRowInMemoryForTest`. Go panics when the slice iterator's length differs from
    /// `savedRows`; here the iterator is a position in `savedRows`, so they never differ.
    // go: sortPartition.numRowInMemoryForTest L2 (the slice iterator is a position in savedRows)
    pub fn num_row_in_memory_for_test(&self) -> i64 {
        self.saved_rows.len() as i64
    }

    /// The field types of the partition's rows (Go's `fieldTypes`, used by the spill).
    // glue: accessor
    pub fn field_types(&self) -> &[Arc<FieldType>] {
        &self.field_types
    }

    /// Whether `close` has been called.
    // glue: accessor
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// The spill limit (Go's `spillLimit`, used by the spill action).
    // glue: accessor
    pub fn spill_limit(&self) -> i64 {
        self.spill_limit
    }

    /// The file name prefix of the spill files in tests.
    // glue: accessor
    pub fn file_name_prefix_for_test(&self) -> &str {
        &self.file_name_prefix_for_test
    }
}
