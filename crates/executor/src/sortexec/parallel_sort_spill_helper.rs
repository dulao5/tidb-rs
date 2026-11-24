//! go-file: pkg/executor/sortexec/parallel_sort_spill_helper.go
//!
//! The spill helper of the parallel sort, reduced to its spill state: the fetcher, the
//! workers and the result generator ask it whether a spill is needed (`isSpillNeeded`) or
//! has happened (`isSpillTriggered`).
//!
//! The state only changes through the spill action (`parallelSortSpillAction`, which sets
//! `needSpill`), which Go registers on the session memory tracker; the Rust session has none
//! (the memory exception A1), so the state stays `notSpilled`. Spilling itself
//! (`spill`, `spillImpl`) writes to the disk container (D01.10) and is a boundary stub.

use std::sync::Arc;

use parking_lot::Mutex;
use tidb_base::Error;
use tidb_base::blocked::blocked;

use super::multi_way_merge::{MemorySource, MultiWayMerger};
use super::sort_util::{DataInDiskByChunks, needSpill, notSpilled};

/// parallelSortSpillHelper, its spill state.
pub struct ParallelSortSpillHelper {
    /// `cond.L` with the state it protects (`spillStatus`, `sortedRowsInDisk`).
    cond: Mutex<SpillState>,
}

/// The fields of `parallelSortSpillHelper` its lock protects.
struct SpillState {
    spill_status: i32,
    /// Shared with the disk source of the result generator (Go shares the pointers).
    sorted_rows_in_disk: Vec<Arc<DataInDiskByChunks>>,
}

/// The spill helper of a parallel sort, in the state `notSpilled`.
///
/// Go also keeps the executor, its `lessRow`, the result channel, `finishCh`, the field
/// types, the byte counters and the file name prefix: they serve the spill (D01.10), and the
/// result channel is never read (a copy of the sender here would keep the channel from
/// closing).
// go: newParallelSortSpillHelper L2 (the spill state only: the other fields serve the spill, D01.10)
pub fn new_parallel_sort_spill_helper() -> ParallelSortSpillHelper {
    ParallelSortSpillHelper {
        cond: Mutex::new(SpillState {
            spill_status: notSpilled,
            sorted_rows_in_disk: Vec::new(),
        }),
    }
}

impl ParallelSortSpillHelper {
    /// The temporary spill chunk is never made (`initForSpill` is part of the spill, D01.10),
    /// so only the spilled data is closed.
    // go: parallelSortSpillHelper.close L2 (no temporary spill chunk to destroy, see the comment)
    pub fn close(&self) {
        for in_disk in &self.cond.lock().sorted_rows_in_disk {
            in_disk.close();
        }
    }

    // go: parallelSortSpillHelper.isSpillNeeded L1
    pub fn is_spill_needed(&self) -> bool {
        self.cond.lock().spill_status == needSpill
    }

    // go: parallelSortSpillHelper.isSpillTriggered L1
    pub fn is_spill_triggered(&self) -> bool {
        !self.cond.lock().sorted_rows_in_disk.is_empty()
    }

    /// The containers of the spilled rows (Go reads `sortedRowsInDisk` directly).
    // glue: e.Parallel.spillHelper.sortedRowsInDisk
    pub fn sorted_rows_in_disk(&self) -> Vec<Arc<DataInDiskByChunks>> {
        self.cond.lock().sorted_rows_in_disk.clone()
    }

    // glue: stub of parallelSortSpillHelper.spill, blocked-by:D01.10 (spilling the parallel sort's rows to disk needs the disk container)
    pub fn spill(&self) -> Result<(), Error> {
        Err(blocked(
            "D01.10",
            "spilling the parallel sort's rows to disk",
        ))
    }

    // glue: stub of parallelSortSpillHelper.spillImpl, blocked-by:D01.10 (spilling the parallel sort's rows to disk needs the disk container)
    pub fn spill_impl(&self, _merger: &mut MultiWayMerger<MemorySource>) -> Result<(), Error> {
        Err(blocked(
            "D01.10",
            "spilling the parallel sort's rows to disk",
        ))
    }
}
