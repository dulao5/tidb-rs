//! go-file: pkg/executor/sortexec/topn_spill.go
//!
//! The spill helper of the top-n executor, reduced to its spill state: the in-memory path
//! asks it whether a spill is needed (`isSpillNeeded`) or has happened (`isSpillTriggered`).
//!
//! The state only changes through the spill action (`topNSpillAction.Action` sets
//! `needSpill`), which Go registers on the session memory tracker; the Rust session has none
//! (the memory exception A1), so the state stays `notSpilled`. The temporary spill chunks, the workers and the
//! spilled data of the helper serve the spill itself (the disk container, D01.10) and are
//! not kept.

use parking_lot::Mutex;

use super::sort_util::{DataInDiskByChunks, needSpill, notSpilled};

/// topNSpillHelper, its spill state.
pub struct TopNSpillHelper {
    /// `cond.L` with the state it protects (`spillStatus`, `sortedRowsInDisk`).
    cond: Mutex<SpillState>,
}

/// The fields of `topNSpillHelper` its lock protects.
struct SpillState {
    spill_status: i32,
    sorted_rows_in_disk: Vec<DataInDiskByChunks>,
}

/// The spill helper of a top-n executor, in the state `notSpilled`.
// go: newTopNSpillerHelper L2 (the spill state only: the temporary spill chunks, the workers and the trackers serve the spill, D01.10)
pub fn new_top_n_spiller_helper() -> TopNSpillHelper {
    TopNSpillHelper {
        cond: Mutex::new(SpillState {
            spill_status: notSpilled,
            sorted_rows_in_disk: Vec::new(),
        }),
    }
}

impl TopNSpillHelper {
    // go: topNSpillHelper.close L1
    pub fn close(&self) {
        for in_disk in &self.cond.lock().sorted_rows_in_disk {
            in_disk.close();
        }
    }

    // go: topNSpillHelper.isSpillNeeded L1
    pub fn is_spill_needed(&self) -> bool {
        self.cond.lock().spill_status == needSpill
    }

    // go: topNSpillHelper.isSpillTriggered L1
    pub fn is_spill_triggered(&self) -> bool {
        !self.cond.lock().sorted_rows_in_disk.is_empty()
    }
}
