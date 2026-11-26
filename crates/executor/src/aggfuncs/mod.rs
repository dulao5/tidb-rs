//! The aggregate functions (D12.4, the implementations of the aggregate functions), Go's
//! `pkg/executor/aggfuncs`, one Rust file per Go file (spec `specs/D12.4-aggfuncs`, §5.1).
//!
//! Parts A (spec §3.1), B1 (spec §3.2), and B2 (spec §3.3) are ported: the [`AggFunc`]
//! interface, [`build`], COUNT (including DISTINCT), SUM, AVG, FIRST_ROW, MAX / MIN (not
//! sliding), BIT_OR / BIT_XOR / BIT_AND, GROUP_CONCAT, VAR_POP / VAR_SAMP, STDDEV_POP /
//! STDDEV_SAMP, JSON_ARRAYAGG, JSON_OBJECTAGG, APPROX_COUNT_DISTINCT, and APPROX_PERCENTILE.
//! The window functions (part W) and the spill serialization (part S) come later and have no
//! entry point yet.
//!
//! Callers: the aggregation executors (D12.3) and the window executor (D12.9).

#[allow(clippy::module_inception)]
pub mod aggfuncs;
pub mod builder;
pub mod func_avg;
pub mod func_bitfuncs;
pub mod func_count;
pub mod func_count_distinct;
pub mod func_first_row;
pub mod func_group_concat;
pub mod func_json_arrayagg;
pub mod func_json_objectagg;
pub mod func_max_min;
pub mod func_percentile;
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
