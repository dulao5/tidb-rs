//! `pkg/executor/internal/vecgroupchecker`: the vectorized group checker of the stream
//! aggregation (and, later, of the window and merge join executors). Counted in the executor
//! framework block (D12.0); ported with the stream aggregation (the aggregation executors,
//! D12.3, part A; spec `specs/D12.3-agg-executors/spec.md` §2.5).

pub mod vec_group_checker;

pub use vec_group_checker::VecGroupChecker;

#[cfg(test)]
mod tests;
