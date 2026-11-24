//! The sort and top-n executors (D12.5, part VS1-36: the in-memory paths), Go's
//! `pkg/executor/sortexec`.
//!
//! - [`sort`]: `SortExec` (sort.go); the unparallel path is ported, the parallel path (the
//!   one Go's `Open` always takes, sort.go:170) stops at `Open` with `Blocked`.
//! - [`sort_partition`]: `sortPartition`, the rows of one partition, sorted in memory.
//! - [`topn`] / [`topn_chunk_heap`]: `TopNExec` and its row heap.
//! - [`sort_util`]: shared helpers and the cursor over spilled chunks.
//! - [`sort_spill`] / [`topn_spill`]: the parts of the spill machinery the in-memory paths
//!   touch (the spill status and the spill action of a partition); spilling itself needs the
//!   disk container (D01.10) and returns `Blocked`.
//! - [`heap`]: Go's `container/heap`.
//!
//! Spill cannot start here: the spill actions are registered on the session memory tracker
//! in Go, and the Rust session has none yet (the memory exception A1). The spill branches of
//! the in-memory paths are kept and stop with `Blocked` (D01.10).

pub mod heap;
pub mod sort;
pub mod sort_partition;
pub mod sort_spill;
pub mod sort_util;
pub mod topn;
pub mod topn_chunk_heap;
pub mod topn_spill;

pub use sort::{SortExec, SortKeys};
pub use topn::TopNExec;

#[cfg(test)]
mod tests;
