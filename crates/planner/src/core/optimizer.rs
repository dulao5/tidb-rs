//! go-file: pkg/planner/core/optimizer.go
//!
//! So far the two helpers the point get fast path calls (VS1-11): `CheckTableLock` and
//! `checkStableResultMode`. `DoOptimize` and the rest of the driver are VS1-24's (D09.13).

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_base::config;
use tidb_sessionctx::variable::session::SessionVars;

use crate::core::planbuilder::VisitInfo;

/// CheckTableLock checks the table lock.
///
/// The lock checker (`pkg/lock`) is block X (LOCK TABLES): with `enable-table-lock` on
/// (off by default) the check is a boundary stub.
// go: CheckTableLock L2 (the lock checker of an enabled table lock is blocked-by:X)
pub fn check_table_lock(vs: &[VisitInfo]) -> Result<(), Error> {
    if !config::table_lock_enabled() {
        return Ok(());
    }
    // checker := lock.NewChecker(ctx, is); checker.CheckTableLock(...) for each visit info.
    let _ = vs;
    Err(blocked("X", "the table lock check (LOCK TABLES)"))
}

// go: checkStableResultMode L1
pub fn check_stable_result_mode(s: &SessionVars) -> bool {
    let st = s.stmt_ctx();
    s.enable_stable_result_mode
        && (!st.in_insert_stmt && !st.in_update_stmt && !st.in_delete_stmt && !st.in_load_data_stmt)
}
