//! go-file: pkg/executor/aggregate/agg_hash_base_worker.go
//!
//! Common attributes and helpers of `HashAggPartialWorker` and `HashAggFinalWorker` (D12.3
//! part B2).

use std::sync::Arc;

use tidb_memory::TrackedAlloc;
use tidb_memory::tracker::Tracker;
use tokio::sync::watch;

use crate::aggfuncs::AggFunc;

use super::agg_util::AggWorkerStat;

/// `baseHashAggWorker` stores the common attributes of `HashAggFinalWorker` and
/// `HashAggPartialWorker`.
pub(crate) struct BaseHashAggWorker {
    pub(crate) finish_ch: watch::Receiver<bool>,
    pub(crate) agg_funcs: Vec<Option<Arc<dyn AggFunc>>>,
    #[allow(dead_code)] // Retained for parity with Go struct and Part C spill.
    pub(crate) max_chunk_size: usize,
    pub(crate) stats: Option<Arc<AggWorkerStat>>,
    pub(crate) mem_tracker: Arc<Tracker>,
    pub(crate) alloc: TrackedAlloc,
}

// go: newBaseHashAggWorker L2 (carries TrackedAlloc for A1 memory accounting; watch::Receiver for finishCh)
pub(crate) fn new_base_hash_agg_worker(
    finish_ch: watch::Receiver<bool>,
    agg_funcs: Vec<Option<Arc<dyn AggFunc>>>,
    max_chunk_size: usize,
    mem_tracker: Arc<Tracker>,
    alloc: TrackedAlloc,
) -> BaseHashAggWorker {
    BaseHashAggWorker {
        finish_ch,
        agg_funcs,
        max_chunk_size,
        stats: None,
        mem_tracker,
        alloc,
    }
}

impl BaseHashAggWorker {
    // go: baseHashAggWorker.getPartialResultSliceLenConsiderByteAlign L1
    pub(crate) fn get_partial_result_slice_len_consider_byte_align(&self) -> usize {
        let length = self.agg_funcs.len();
        if length == 1 {
            return 1;
        }
        length + (length & 1)
    }

    // go: baseHashAggWorker.checkFinishChClosed L2 (watch::Receiver instead of Go chan)
    pub(crate) fn check_finish_ch_closed(&self) -> bool {
        *self.finish_ch.borrow() || self.finish_ch.has_changed().is_err()
    }
}

/// Awaits until `finish_ch` is closed (receives `true` or its sender is dropped).
// glue: `<-finishCh` on watch::Receiver<bool> (M-CH)
pub(crate) async fn wait_finish(finish_ch: &mut watch::Receiver<bool>) {
    let _ = finish_ch.wait_for(|closed| *closed).await;
}
