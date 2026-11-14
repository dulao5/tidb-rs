//! go-file: pkg/sessiontxn/staleread/util.go
//!
//! `CalculateAsOfTsExpr`, `CalculateTsWithReadStaleness`, `GetExternalTimestamp` are part 2
//! (they evaluate `AS OF` expressions and read the store's safe ts / external ts).

use crate::sctx::SessionCtx;

/// IsStmtStaleness indicates whether the current statement is staleness or not
// go: IsStmtStaleness L1
pub fn is_stmt_staleness(sctx: &dyn SessionCtx) -> bool {
    sctx.get_session_vars().stmt_ctx().is_staleness
}
