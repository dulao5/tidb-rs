//! go-file: pkg/executor/aggregate/agg_util.go
//!
//! The helpers of the aggregation executors (D12.3). Part A (the stream aggregation, spec
//! `specs/D12.3-agg-executors/spec.md` §2.1) ports `closeBaseExecutor`; the hash
//! aggregation's helpers of this file (`recoveryHashAgg`, `getGroupKey`, the partial results
//! map, the worker structs, ...) come with parts B and C.

use tidb_base::Error;
use tidb_base::terror;

use crate::internal::exec::executor::BaseExecutor;

/// Go's deferred `closeBaseExecutor` recovers a panic of `OpenSelf`, closes the base
/// executor (the children) and panics again. A Go panic is an error here (M-PANIC), so the
/// caller calls it on the error path of `OpenSelf` and returns what it returns: `err`
/// itself, after the children were closed (spec §4.5).
// go: closeBaseExecutor L2 (recover → the error of OpenSelf, M-PANIC; async close)
pub(crate) async fn close_base_executor(b: &mut BaseExecutor, err: Error) -> Error {
    // Release the resource, but throw the panic again and let the top level handle it.
    terror::log(b.base.close().await.err().as_ref());
    log::warn!("panic in Open(), close base executor and throw exception again");
    err
}
