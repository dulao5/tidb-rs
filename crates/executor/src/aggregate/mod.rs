//! The aggregation executors (D12.3), Go's `pkg/executor/aggregate`.
//!
//! - Part A (spec `specs/D12.3-agg-executors/spec.md` §2.1): the stream aggregation
//!   ([`agg_stream_executor`]) and `closeBaseExecutor` ([`agg_util`]).
//! - Part B (§2.2): the hash aggregation ([`agg_hash_executor`], [`agg_spill`], [`agg_util`],
//!   and the parallel workers). The builders (`buildStreamAggFromChildExec`,
//!   `buildHashAggFromChildExec`) are in [`crate::builder`].

pub mod agg_hash_executor;
pub mod agg_spill;
pub mod agg_stream_executor;
pub mod agg_util;

pub use agg_hash_executor::{HashAggExec, new_hash_agg_exec};
pub use agg_stream_executor::StreamAggExec;
pub use agg_util::get_group_key;

#[cfg(test)]
mod tests;
