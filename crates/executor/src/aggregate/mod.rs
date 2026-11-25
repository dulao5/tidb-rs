//! The aggregation executors (D12.3), Go's `pkg/executor/aggregate`.
//!
//! - Part A (spec `specs/D12.3-agg-executors/spec.md` §2.1): the stream aggregation
//!   ([`agg_stream_executor`]) and `closeBaseExecutor` ([`agg_util`]).
//! - Part B (§2.2): the hash aggregation (single-threaded and parallel execution in
//!   [`agg_hash_executor`], [`agg_hash_base_worker`], [`agg_hash_partial_worker`],
//!   [`agg_hash_final_worker`], [`agg_spill`], and [`agg_util`]). The builders
//!   (`buildStreamAggFromChildExec`, `buildHashAggFromChildExec`) are in [`crate::builder`].

pub mod agg_hash_base_worker;
pub mod agg_hash_executor;
pub mod agg_hash_final_worker;
pub mod agg_hash_partial_worker;
pub mod agg_spill;
pub mod agg_stream_executor;
pub mod agg_util;

pub use agg_hash_executor::{HashAggExec, HashAggInput, new_hash_agg_exec};
pub use agg_hash_final_worker::{AfFinalResult, HashAggFinalWorker};
pub use agg_hash_partial_worker::HashAggPartialWorker;
pub use agg_stream_executor::StreamAggExec;
pub use agg_util::{AggWorkerInfo, AggWorkerStat, HashAggRuntimeStats, get_group_key};

#[cfg(test)]
mod tests;
