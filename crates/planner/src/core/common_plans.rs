//! go-file: pkg/planner/core/common_plans.go
//!
//! So far the two point-get helpers of VS1-19 (the point get arm with VS1-11) and the
//! `Execute` plan of a prepared statement's execution (D13.1); the other common plans (SET,
//! SIMPLE, EXPLAIN, ...) come with their blocks.

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_sessionctx::variable::session::SessionVars;

use tidb_model::TableCacheStatusDisable;

use tidb_types::datum::Datum;
use tidb_types::field_name::NameSlice;

use crate::PhysicalPlanRef;
use crate::base::{PhysicalKind, PhysicalPlan};
use crate::core::plan_cache_utils::PlanCacheStmtRef;
use crate::core::point_get_plan::PointGetPlan;

/// Execute represents prepare plan.
///
/// - `Params`: Go's `[]expression.Expression` of the arguments; the new stack's arguments are
///   the binary protocol's constants (the text protocol's `USING @var` needs the expression
///   rewriter, D09.2), so their values are kept, as `SetParameterValuesIntoSCtx` reads them.
/// - `Stmt` (Go: the prepared statement's AST, `PrepStmt.PreparedAst.Stmt`) is reached
///   through `prep_stmt`: the AST is owned by the `PlanCacheStmt`.
/// - `baseSchemaProducer`: only the output names are kept (the schema comes with the
///   physical plan, `plan`).
pub struct Execute {
    pub name: String,
    pub params: Vec<Datum>,
    pub prep_stmt: PlanCacheStmtRef,
    pub plan: Option<PhysicalPlanRef>,
    pub output_names: NameSlice,
}

/// IsPointGetWithPKOrUniqueKeyByAutoCommit returns true when meets following conditions:
///  1. ctx is auto commit tagged
///  2. session is not InTxn
///  3. plan is point get by pk, or point get by unique index (no double read)
///
/// The `PointGetPlan` arm is ported (VS1-11); `PhysicalProjection`, `PhysicalIndexReader` /
/// `PhysicalIndexScan` and `PhysicalTableReader` / `PhysicalTableScan` (D10.1, VS1-27) are
/// not, so those plans are reported as blocked and the result is a `Result` for now.
// go: IsPointGetWithPKOrUniqueKeyByAutoCommit L2 (the reader and projection arms are blocked-by:D10.1)
pub fn is_point_get_with_pk_or_unique_key_by_auto_commit(
    vars: &SessionVars,
    p: &dyn PhysicalPlan,
) -> Result<bool, Error> {
    if !is_auto_commit_txn(vars) {
        return Ok(false);
    }

    // check plan
    match p.kind() {
        PhysicalKind::Projection | PhysicalKind::TableReader => {
            // `p = proj.Children()[0]`; the reader arms read the scan's ranges.
            // glue: blocked-by:D10.1 (VS1-27: PhysicalProjection / PhysicalIndexReader / PhysicalTableReader)
            Err(blocked(
                "D10.1",
                "IsPointGetWithPKOrUniqueKeyByAutoCommit (physical operators)",
            ))
        }
        PhysicalKind::PointGet => {
            let Some(v) = p.as_any().downcast_ref::<PointGetPlan>() else {
                return Ok(false);
            };
            // If the PointGetPlan needs to read data using unique index (double read), we
            // can't use max uint64, because using math.MaxUint64 can't guarantee repeatable-read
            // and the data and index would be inconsistent!
            // If the PointGetPlan needs to read data from Cache Table, we can't use max uint64,
            // because math.MaxUint64 always make cacheData invalid.
            let no_second_read = v
                .index_info
                .as_ref()
                .is_none_or(|i| i.primary && v.tbl_info.is_common_handle);
            if !no_second_read {
                return Ok(false);
            }
            if v.tbl_info.table_cache_status_type != TableCacheStatusDisable {
                return Ok(false);
            }
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// IsAutoCommitTxn checks if session is in autocommit mode and not InTxn
/// used for fast plan like point get
// go: IsAutoCommitTxn L1
pub fn is_auto_commit_txn(vars: &SessionVars) -> bool {
    vars.is_autocommit() && !vars.in_txn()
}
