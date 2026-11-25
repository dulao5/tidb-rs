//! The aggregate functions (D12.4, the implementations of the aggregate functions), Go's
//! `pkg/executor/aggfuncs`, one Rust file per Go file (spec `specs/D12.4-aggfuncs`, §5.1).
//!
//! Part A (spec §3.1) is ported: the [`AggFunc`] interface, [`build`], and COUNT (without
//! DISTINCT), SUM, AVG, FIRST_ROW, MAX / MIN (not sliding) and BIT_OR / BIT_XOR / BIT_AND.
//! The functions of the parts B1 and B2 (GROUP_CONCAT, VAR / STDDEV, JSON_*AGG,
//! APPROX_COUNT_DISTINCT, APPROX_PERCENTILE and COUNT(DISTINCT)) return `blocked` from
//! [`build`] until they are ported (spec §4); the window functions (part W) and the spill
//! serialization (part S) come later and have no entry point yet.
//!
//! Callers: the aggregation executors (D12.3) and the window executor (D12.9), neither of
//! which is ported yet, so nothing in the default execution path builds these functions.

#[allow(clippy::module_inception)]
pub mod aggfuncs;
pub mod builder;
pub mod func_avg;
pub mod func_bitfuncs;
pub mod func_count;
pub mod func_first_row;
pub mod func_json_arrayagg;
pub mod func_json_objectagg;
pub mod func_max_min;
pub mod func_stddevpop;
pub mod func_stddevsamp;
pub mod func_sum;
pub mod func_varpop;
pub mod func_varsamp;

pub use aggfuncs::{
    AggFunc, AggFuncUpdateContext, AggPartialResultMapper, BaseAggFunc, PartialResult,
    new_agg_partial_result_mapper, new_agg_partial_result_mapper_in,
    new_agg_partial_result_mapper_with_cap,
};
pub use builder::{AggFuncBuildContext, build};

#[cfg(test)]
mod tests;
