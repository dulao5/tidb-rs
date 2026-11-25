//! go-file: pkg/executor/aggregate/agg_spill.go
//!
//! Spill actions and spill-trigger state machine for `HashAggExec` (D12.3 part B).
//! Part B1 implements `hasEnoughDataToSpill` and `AggSpillDiskAction` (unparallel spill
//! action); part B2 adds `parallelHashAggSpillHelper` and `ParallelAggSpillDiskAction`.

// Go identifiers for package constants (M-GONAME).
#![allow(non_upper_case_globals)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use parking_lot::Mutex;
use tidb_base::Error;
use tidb_memory::action::{ActionOnExceed, ActionRef, BaseOOMAction, DefSpillPriority};
use tidb_memory::tracker::{QueryForceDisk, Tracker};

/// maxSpillTimes indicates how many times the data can spill at most.
pub(crate) const maxSpillTimes: u32 = 10;

/// Number of disk partitions for parallel HashAgg spill.
#[allow(dead_code)] // Used by B2 parallelHashAggSpillHelper.
pub(crate) const spilledPartitionNum: usize = 256;

pub(crate) const spillLogInfo: &str = "memory exceeds quota, set aggregate mode to spill-mode";

/// Guarantee that processed data is at least 20% of the threshold, to avoid spilling too
/// frequently.
// go: hasEnoughDataToSpill L1
pub(crate) fn has_enough_data_to_spill(agg_tracker: &Tracker, passed_in_tracker: &Tracker) -> bool {
    agg_tracker.bytes_consumed() >= passed_in_tracker.get_bytes_limit() / 5
}

/// AggSpillDiskAction implements `memory.ActionOnExceed` for unparallel HashAgg.
/// If the memory quota of a query is exceeded, `AggSpillDiskAction.Action` is triggered.
///
/// Instead of holding `*HashAggExec` (which is exclusively owned by `&mut self` in Rust),
/// the action holds shared handles to the two fields it reads/writes: `in_spill_mode` and
/// `mem_tracker` (spec §4.1).
pub struct AggSpillDiskAction {
    base: BaseOOMAction,
    in_spill_mode: Arc<AtomicU32>,
    mem_tracker: Arc<Tracker>,
    spill_times: Mutex<u32>,
}

impl AggSpillDiskAction {
    // glue: Go composite literal &AggSpillDiskAction{e: e}
    pub(crate) fn new(in_spill_mode: Arc<AtomicU32>, mem_tracker: Arc<Tracker>) -> Self {
        Self {
            base: BaseOOMAction::default(),
            in_spill_mode,
            mem_tracker,
            spill_times: Mutex::new(0),
        }
    }
}

impl ActionOnExceed for AggSpillDiskAction {
    /// Action sets HashAggExec spill mode.
    // go: AggSpillDiskAction.Action L1
    fn action(&self, t: &Arc<Tracker>) -> Result<(), Error> {
        let mut spill_times = self.spill_times.lock();
        if self.in_spill_mode.load(Ordering::SeqCst) == 0
            && has_enough_data_to_spill(&self.mem_tracker, t)
            && *spill_times < maxSpillTimes
        {
            *spill_times += 1;
            log::info!(
                "{spillLogInfo}, spillTimes={}, consumed={}, quota={}",
                *spill_times,
                t.bytes_consumed(),
                t.get_bytes_limit()
            );
            self.in_spill_mode.store(1, Ordering::SeqCst);
            QueryForceDisk.fetch_add(1, Ordering::SeqCst);
            return Ok(());
        }
        drop(spill_times);
        if let Some(fallback) = self.get_fallback() {
            fallback.action(t)?;
        }
        Ok(())
    }

    // glue: Go embeds BaseOOMAction; forwarded to its SetFallback
    fn set_fallback(&self, a: Option<ActionRef>) {
        self.base.set_fallback(a)
    }

    // glue: Go embeds BaseOOMAction; forwarded to its GetFallback
    fn get_fallback(&self) -> Option<ActionRef> {
        self.base.get_fallback()
    }

    /// GetPriority gets the priority of the Action.
    // go: AggSpillDiskAction.GetPriority L1
    fn get_priority(&self) -> i64 {
        DefSpillPriority
    }

    // glue: Go embeds BaseOOMAction; forwarded to its SetFinished
    fn set_finished(&self) {
        self.base.set_finished()
    }

    // glue: Go embeds BaseOOMAction; forwarded to its IsFinished
    fn is_finished(&self) -> bool {
        self.base.is_finished()
    }
}
