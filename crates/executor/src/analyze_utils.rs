//! go-file: pkg/executor/analyze_utils.go
//!
//! Context error normalization helper (block D12.14, carried with D12.1 Part 2A).

use tidb_base::Error;

/// normalizeCtxErrWithCause returns the context's cancellation cause when `err` is
/// `context.Canceled` or `context.DeadlineExceeded`.
///
/// Under mapping rule `M-GOCTX`, `tidb_kv::Context` carries only the request source label
/// (cancellation is performed by dropping the future), so there is no separate context cause
/// and `err` is returned unchanged.
// go: normalizeCtxErrWithCause L2 (M-GOCTX: tidb_kv::Context carries no cancellation cause)
pub fn normalize_ctx_err_with_cause(_ctx: &tidb_kv::Context, err: Error) -> Error {
    err
}
