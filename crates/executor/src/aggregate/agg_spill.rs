//! go-file: pkg/executor/aggregate/agg_spill.go
//!
//! Spill actions and spill-trigger state machine for `HashAggExec` (D12.3 part B).
//!
//! Part B ports:
//! - Unparallel: `hasEnoughDataToSpill`, `AggSpillDiskAction.{Action, GetPriority}`.
//! - Parallel: `newSpillHelper`, `parallelHashAggSpillHelper.{close, isSpilledChunksIOEmpty,
//!   isNoSpill, checkNeedSpill, setNeedSpill, waitForTheEndOfSpill, checkError, setError}`,
//!   `ParallelAggSpillDiskAction.{Action, actionImpl, GetPriority}`.
//!
//! Part C (blocked on D01.10) will add the disk restore/partition methods (`getNextPartition`,
//! `addListInDisks`, `getListInDisks`, `setInSpilling`, `setSpillTriggered`,
//! `restoreOnePartition`, `restoreFromOneSpillFile`, `processRow`).

// Go identifiers for package constants (M-GONAME).
#![allow(non_upper_case_globals)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use parking_lot::{Condvar, Mutex};
use tidb_base::Error;
use tidb_chunk::chunk::Chunk;
use tidb_chunk::chunk_in_disk::DataInDiskByChunks;
use tidb_memory::action::{ActionOnExceed, ActionRef, BaseOOMAction, DefSpillPriority};
use tidb_memory::tracker::{QueryForceDisk, Tracker};
use tidb_parser::types::FieldType;

use crate::aggfuncs::AggFunc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub(crate) enum SpillStatus {
    NoSpill = 0,
    NeedSpill = 1,
    InSpilling = 2,
    #[allow(dead_code)] // Used by Part C setSpillTriggered.
    SpillTriggered = 3,
}

/// maxSpillTimes indicates how many times the data can spill at most.
pub(crate) const maxSpillTimes: u32 = 10;

/// Number of disk partitions for parallel HashAgg spill.
pub(crate) const spilledPartitionNum: usize = 256;

pub(crate) const spillLogInfo: &str = "memory exceeds quota, set aggregate mode to spill-mode";

struct SpillHelperLock {
    #[allow(dead_code)] // Used by Part C getNextPartition.
    next_partition_idx: isize,
    spilled_chunks_io: Vec<Vec<Arc<DataInDiskByChunks>>>,
    status: SpillStatus,
    #[allow(dead_code)] // Logged by Part C setInSpilling.
    memory_consumption: i64,
    #[allow(dead_code)] // Logged by Part C setInSpilling.
    memory_quota: i64,
}

/// State machine coordinating parallel HashAgg spill triggers and spilled disk partitions.
pub struct ParallelHashAggSpillHelper {
    lock: Mutex<SpillHelperLock>,
    wait_if_in_spilling: Condvar,

    #[allow(dead_code)] // Used by Part C restoreOnePartition.
    pub(crate) mem_tracker: Arc<Tracker>,
    pub(crate) disk_tracker: Mutex<Option<Arc<Tracker>>>,
    has_error: AtomicBool,

    #[allow(dead_code)] // Used by Part C restoreFromOneSpillFile.
    pub(crate) agg_funcs_for_restoring: Vec<Option<Arc<dyn AggFunc>>>,
    #[allow(dead_code)] // Used by Part C processRow.
    pub(crate) final_worker_agg_funcs: Vec<Option<Arc<dyn AggFunc>>>,
    #[allow(dead_code)] // Used by Part C prepareForSpill.
    pub(crate) get_new_spill_chunk_func: Box<dyn Fn() -> Chunk + Send + Sync>,
    #[allow(dead_code)] // Used by Part C prepareForSpill.
    pub(crate) spill_chunk_field_types: Vec<FieldType>,
}

// go: newSpillHelper L1
pub(crate) fn new_spill_helper(
    tracker: Arc<Tracker>,
    agg_funcs_for_restoring: Vec<Option<Arc<dyn AggFunc>>>,
    final_worker_agg_funcs: Vec<Option<Arc<dyn AggFunc>>>,
    get_new_spill_chunk_func: Box<dyn Fn() -> Chunk + Send + Sync>,
    spill_chunk_field_types: Vec<FieldType>,
) -> Result<ParallelHashAggSpillHelper, Error> {
    if agg_funcs_for_restoring.len() != final_worker_agg_funcs.len() {
        return Err(Error::plain(
            "len(aggFuncsForRestoring) != len(finalWorkerAggFuncs)",
        ));
    }

    Ok(ParallelHashAggSpillHelper {
        lock: Mutex::new(SpillHelperLock {
            next_partition_idx: (spilledPartitionNum as isize) - 1,
            spilled_chunks_io: (0..spilledPartitionNum).map(|_| Vec::new()).collect(),
            status: SpillStatus::NoSpill,
            memory_consumption: 0,
            memory_quota: 0,
        }),
        wait_if_in_spilling: Condvar::new(),
        mem_tracker: tracker,
        disk_tracker: Mutex::new(None),
        has_error: AtomicBool::new(false),
        agg_funcs_for_restoring,
        final_worker_agg_funcs,
        get_new_spill_chunk_func,
        spill_chunk_field_types,
    })
}

impl ParallelHashAggSpillHelper {
    // go: parallelHashAggSpillHelper.close L1
    pub(crate) fn close(&self) {
        let lock = self.lock.lock();
        for ios in &lock.spilled_chunks_io {
            for io in ios {
                io.close();
            }
        }
    }

    // go: parallelHashAggSpillHelper.isSpilledChunksIOEmpty L1
    pub(crate) fn is_spilled_chunks_io_empty(&self) -> bool {
        let lock = self.lock.lock();
        for ios in &lock.spilled_chunks_io {
            if !ios.is_empty() {
                return false;
            }
        }
        true
    }

    // go: parallelHashAggSpillHelper.isNoSpill L1
    pub(crate) fn is_no_spill(&self) -> bool {
        let lock = self.lock.lock();
        lock.status == SpillStatus::NoSpill
    }

    // go: parallelHashAggSpillHelper.checkNeedSpill L1
    pub(crate) fn check_need_spill(&self) -> bool {
        let lock = self.lock.lock();
        lock.status == SpillStatus::NeedSpill
    }

    /// Returns true if the `NeedSpill` flag was successfully set.
    // go: parallelHashAggSpillHelper.setNeedSpill L1
    pub(crate) fn set_need_spill(
        &self,
        executor_tracker: &Tracker,
        triggered_tracker: &Tracker,
    ) -> bool {
        let mut lock = self.lock.lock();
        if has_enough_data_to_spill(executor_tracker, triggered_tracker) {
            lock.status = SpillStatus::NeedSpill;
            lock.memory_consumption = triggered_tracker.bytes_consumed();
            lock.memory_quota = triggered_tracker.get_bytes_limit();
            return true;
        }
        false
    }

    // go: parallelHashAggSpillHelper.waitForTheEndOfSpill L1
    pub(crate) fn wait_for_the_end_of_spill(&self) {
        let mut lock = self.lock.lock();
        while lock.status == SpillStatus::InSpilling {
            self.wait_if_in_spilling.wait(&mut lock);
        }
    }

    /// Checks the error flag atomically as multiple partial workers may access it.
    // go: parallelHashAggSpillHelper.checkError L1
    pub(crate) fn check_error(&self) -> bool {
        self.has_error.load(Ordering::SeqCst)
    }

    /// Sets the error flag atomically as multiple partial workers may access it.
    // go: parallelHashAggSpillHelper.setError L1
    pub(crate) fn set_error(&self) {
        self.has_error.store(true, Ordering::SeqCst);
    }

    /// Stores the disk tracker for this spill helper.
    // glue: field write e.spillHelper.diskTracker = e.diskTracker
    pub(crate) fn set_disk_tracker(&self, disk_tracker: Arc<Tracker>) {
        *self.disk_tracker.lock() = Some(disk_tracker);
    }
}

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

/// ParallelAggSpillDiskAction implements `memory.ActionOnExceed` for parallel HashAgg.
pub struct ParallelAggSpillDiskAction {
    base: BaseOOMAction,
    mem_tracker: Arc<Tracker>,
    spill_helper: Arc<ParallelHashAggSpillHelper>,
}

impl ParallelAggSpillDiskAction {
    // glue: Go composite literal &ParallelAggSpillDiskAction{e: e, spillHelper: e.spillHelper}
    pub(crate) fn new(
        mem_tracker: Arc<Tracker>,
        spill_helper: Arc<ParallelHashAggSpillHelper>,
    ) -> Self {
        Self {
            base: BaseOOMAction::default(),
            mem_tracker,
            spill_helper,
        }
    }

    /// Returns true if the spill flag was successfully set.
    // go: ParallelAggSpillDiskAction.actionImpl L1
    fn action_impl(&self, t: &Arc<Tracker>) -> bool {
        self.spill_helper.wait_for_the_end_of_spill();
        self.spill_helper.set_need_spill(&self.mem_tracker, t)
    }
}

impl ActionOnExceed for ParallelAggSpillDiskAction {
    /// Action sets parallel HashAgg spill mode or triggers the fallback action.
    // go: ParallelAggSpillDiskAction.Action L1
    fn action(&self, t: &Arc<Tracker>) -> Result<(), Error> {
        if self.action_impl(t) {
            return Ok(());
        }
        self.base.trigger_fall_back_action(t)
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
    // go: ParallelAggSpillDiskAction.GetPriority L1
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
