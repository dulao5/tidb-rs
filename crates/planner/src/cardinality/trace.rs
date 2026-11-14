//! go-file: pkg/planner/cardinality/trace.go
//!
//! The estimation traces (`TRACE PLAN TARGET='estimation'`, the optimizer debug trace) and the
//! record of used statistics are not in D10.6 part 1 (VS1-29; specs/D10.6-cardinality §1.2):
//! the entry points of the estimation return a boundary error when either trace is on, and
//! the used-statistics record waits for `StatementContext.UsedStatsInfo` (owned by D11.2,
//! specs/D11.2-stats-load tasks P1-1).

// Go package-level names (pseudoEqualRate, TypeLonglong, ...), mapping rule M-GONAME.
#![allow(non_upper_case_globals)]

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_statistics::{Column, Index};

use crate::PlanCtx;

/// The statistics item of `recordUsedItemStatsStatus` (Go's `stats any`: a `*Column` or an
/// `*Index`, possibly nil).
#[allow(dead_code)] // read when recordUsedItemStatsStatus is ported (D11.2)
pub(crate) enum StatsItem<'a> {
    Column(Option<&'a Column>),
    Index(Option<&'a Index>),
}

/// The optimizer debug trace and the CE trace are not ported (module doc): a statement with
/// either on gets a boundary error instead of an estimate without its trace.
// glue: boundary of the trace functions of trace.go / debugtrace (D10.6 part 2)
pub(crate) fn check_no_trace(sctx: &PlanCtx<'_>) -> Result<(), Error> {
    let sc = sctx.vars.stmt_ctx();
    if sc.enable_optimizer_debug_trace || sc.enable_optimizer_ce_trace {
        return Err(blocked(
            "D10.6",
            "optimizer debug trace / CE trace of the estimation",
        ));
    }
    Ok(())
}

/// recordUsedItemStatsStatus records the status of the statistics a statement uses (slow log
/// `Stats:` field, `EXPLAIN ANALYZE`). `StatementContext.GetUsedStatsInfo` is not ported yet,
/// so nothing is recorded; the estimate does not depend on it.
// glue: stub of recordUsedItemStatsStatus, blocked-by:D11.2 (StatementContext.UsedStatsInfo, D11.2 tasks P1-1)
pub(crate) fn record_used_item_stats_status(
    _sctx: &PlanCtx<'_>,
    _stats: StatsItem<'_>,
    _table_id: i64,
    _id: i64,
) {
}
