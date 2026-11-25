//! The aggregation executors (D12.3), Go's `pkg/executor/aggregate`.
//!
//! Part A (spec `specs/D12.3-agg-executors/spec.md` §2.1): the stream aggregation
//! ([`agg_stream_executor`]) and `closeBaseExecutor` ([`agg_util`]); the builder's
//! `buildStreamAggFromChildExec` is in [`crate::builder`]. The hash aggregation (parts B and
//! C) comes later.

pub mod agg_stream_executor;
pub mod agg_util;

pub use agg_stream_executor::StreamAggExec;

#[cfg(test)]
mod tests;
