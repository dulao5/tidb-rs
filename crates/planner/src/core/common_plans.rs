//! go-file: pkg/planner/core/common_plans.go
//!
//! So far the two point-get helpers of VS1-19; the other common plans (EXECUTE, SET,
//! SIMPLE, EXPLAIN, ...) come with their blocks.

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_sessionctx::variable::session::SessionVars;

use crate::base::PhysicalPlan;

/// IsPointGetWithPKOrUniqueKeyByAutoCommit returns true when meets following conditions:
///  1. ctx is auto commit tagged
///  2. session is not InTxn
///  3. plan is point get by pk, or point get by unique index (no double read)
///
/// The plan checks need `PhysicalProjection`, `PhysicalIndexReader` / `PhysicalIndexScan`,
/// `PhysicalTableReader` / `PhysicalTableScan` (D10.1, VS1-27) and `PointGetPlan` (D10.8,
/// VS1-11); until they exist the function answers the session part and reports the rest as
/// blocked, so the result is a `Result` for now.
// go: IsPointGetWithPKOrUniqueKeyByAutoCommit L1
pub fn is_point_get_with_pk_or_unique_key_by_auto_commit(
    vars: &SessionVars,
    p: &dyn PhysicalPlan,
) -> Result<bool, Error> {
    if !is_auto_commit_txn(vars) {
        return Ok(false);
    }

    // check plan
    // glue: blocked-by:D10.1 (VS1-27: PhysicalProjection / PhysicalIndexReader / PhysicalTableReader)
    // glue: blocked-by:D10.8 (VS1-11: PointGetPlan)
    let _ = p;
    Err(blocked(
        "D10.1",
        "IsPointGetWithPKOrUniqueKeyByAutoCommit (physical operators)",
    ))
}

/// IsAutoCommitTxn checks if session is in autocommit mode and not InTxn
/// used for fast plan like point get
// go: IsAutoCommitTxn L1
pub fn is_auto_commit_txn(vars: &SessionVars) -> bool {
    vars.is_autocommit() && !vars.in_txn()
}
