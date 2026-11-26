//! The aggregate functions (D12.4, the implementations of the aggregate functions), Go's
//! `pkg/executor/aggfuncs`, one Rust file per Go file (spec `specs/D12.4-aggfuncs`, §5.1).
//!
//! Parts A (spec §3.1), B1 (spec §3.2), B2 (spec §3.3), and W (spec §3.4) are ported:
//! the [`AggFunc`], [`SlidingWindowAggFunc`], and [`MaxMinSlidingWindowAggFunc`] interfaces,
//! [`build`], [`build_window_functions`], COUNT (including DISTINCT), SUM, AVG, FIRST_ROW,
//! MAX / MIN (including sliding), BIT_OR / BIT_XOR / BIT_AND, GROUP_CONCAT, VAR_POP /
//! VAR_SAMP, STDDEV_POP / STDDEV_SAMP, JSON_ARRAYAGG, JSON_OBJECTAGG,
//! APPROX_COUNT_DISTINCT, APPROX_PERCENTILE, and the window functions (ROW_NUMBER, RANK,
//! DENSE_RANK, PERCENT_RANK, CUME_DIST, NTILE, FIRST_VALUE, LAST_VALUE, NTH_VALUE, LEAD,
//! LAG). The spill serialization (part S) comes later and has no entry point yet.
//!
//! Callers: the aggregation executors (D12.3) and the window executor (D12.9).

#[allow(clippy::module_inception)]
pub mod aggfuncs;
pub mod builder;
pub mod func_avg;
pub mod func_bitfuncs;
pub mod func_count;
pub mod func_count_distinct;
pub mod func_cume_dist;
pub mod func_first_row;
pub mod func_group_concat;
pub mod func_json_arrayagg;
pub mod func_json_objectagg;
pub mod func_lead_lag;
pub mod func_max_min;
pub mod func_ntile;
pub mod func_percent_rank;
pub mod func_percentile;
pub mod func_rank;
pub mod func_stddevpop;
pub mod func_stddevsamp;
pub mod func_sum;
pub mod func_value;
pub mod func_varpop;
pub mod func_varsamp;
pub mod row_number;

pub use aggfuncs::{
    AggFunc, AggFuncUpdateContext, AggPartialResultMapper, BaseAggFunc, MaxMinSlidingWindowAggFunc,
    PartialResult, SlidingWindowAggFunc, new_agg_partial_result_mapper,
    new_agg_partial_result_mapper_in, new_agg_partial_result_mapper_with_cap,
};
pub use builder::{AggFuncBuildContext, build, build_window_functions};
pub use func_max_min::{MinMaxDeque, Pair, new_deque, new_deque_in};

#[cfg(test)]
mod tests;
