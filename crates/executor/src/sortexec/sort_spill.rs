//! go-file: pkg/executor/sortexec/sort_spill.go
//!
//! The spill action of a sort partition (`sortPartitionSpillDiskAction`). Go registers it on
//! the session memory tracker; the Rust session has none yet (the memory exception A1), so
//! it is never triggered. Its `Action` spills the partition to disk (the disk container,
//! D01.10) and is a boundary stub.

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_memory::action::{ActionOnExceed, ActionRef, BaseOOMAction, DefSpillPriority};
use tidb_memory::tracker::Tracker;

/// sortPartitionSpillDiskAction implements memory.ActionOnExceed for chunk.List. If the
/// memory quota of a query is exceeded, sortPartitionSpillDiskAction.Action is triggered.
///
/// Go keeps the partition it spills (`partition`) and a `sync.Once`; both serve the spill,
/// which is not ported, so the action keeps only its `BaseOOMAction`.
#[derive(Default)]
pub struct SortPartitionSpillDiskAction {
    base: BaseOOMAction,
}

impl ActionOnExceed for SortPartitionSpillDiskAction {
    // glue: stub of sortPartitionSpillDiskAction.Action, blocked-by:D01.10 (spilling a sort partition to disk)
    fn action(&self, _t: &Arc<Tracker>) -> Result<(), Error> {
        Err(blocked("D01.10", "spilling a sort partition to disk"))
    }
    // glue: Go embeds BaseOOMAction; forwarded to its SetFallback
    fn set_fallback(&self, a: Option<ActionRef>) {
        self.base.set_fallback(a)
    }
    // glue: Go embeds BaseOOMAction; forwarded to its GetFallback
    fn get_fallback(&self) -> Option<ActionRef> {
        self.base.get_fallback()
    }
    /// GetPriority get the priority of the Action.
    // go: sortPartitionSpillDiskAction.GetPriority L1
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
