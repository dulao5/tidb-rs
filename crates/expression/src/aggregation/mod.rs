//! Port of Go TiDB v8.5.8 `pkg/expression/aggregation` (block D08.11): the descriptors of
//! aggregate and window functions the planner builds and the executors read (spec
//! `specs/D08.11-aggregation`).
//!
//! So far part A: `baseFuncDesc` with the return type inference, `AggFuncDesc`, the modes and
//! the name helpers. Part B (the tipb conversion and push-down checks) lives in
//! tidb-expr-pushdown (`tidb_expr_pushdown::aggregation`, spec §4.8). Part C (the window
//! function descriptor's functions) and part D (`ExplainAggFunc`) come later; the legacy
//! evaluators only the mock store uses are not ported (spec §2.3).

// The Go package `aggregation` has a file `aggregation.go` (file-for-file, constitution §2).
#[allow(clippy::module_inception)]
pub mod aggregation;
pub mod base_func;
pub mod descriptor;
pub mod window_func;

pub use aggregation::{AggFunctionMode, is_all_first_row, need_count, need_value};
pub use base_func::BaseFuncDesc;
pub use descriptor::{AggFuncDesc, new_agg_func_desc, new_agg_func_desc_for_window_func};
pub use window_func::WindowFuncDesc;

#[cfg(test)]
mod tests;
