//! Go package `pkg/util/hint` (block D13.3, the hint skeleton of the optimizer driver).
//!
//! `StmtHints` and its data-only methods live in `tidb_sessionctx::hint` (the statement
//! context embeds them); this module re-exports them and holds the hint processing the
//! planner calls. The Rust parser does not parse the hint grammar (`ast::TableOptimizerHint`
//! is opaque), so a statement with hints stops at a `blocked-by:D02.2` boundary wherever Go
//! reads a hint; statements without hints get Go's results.

#[allow(clippy::module_inception)]
pub mod hint;
pub mod hint_processor;
pub mod hint_query_block;

pub use hint::{
    HintedIndex, HintedTable, IndexJoinHints, PlanHints, parse_plan_hints, parse_stmt_hints,
};
pub use hint_processor::extract_table_hints_from_stmt_node;
pub use hint_query_block::{HintWarnHandler, QBHintBuildState, QBHintHandler, new_qb_hint_handler};
pub use tidb_sessionctx::hint::StmtHints;
