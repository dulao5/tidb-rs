//! # SPIKE — D09.7 design pilot for N1. Not for merging, not counted in coverage.
//!
//! This crate is the design pilot of `docs/porting/methodology.md` §8 and ROADMAP P0 item 3:
//! port column pruning and (logical) projection elimination of Go TiDB v8.5.8 onto the plan
//! representation of `specs/exceptions/N1-plan-representation/spec.md`, to find out whether
//! that representation keeps rule code line-by-line comparable to Go. The findings are in
//! `specs/D09.7-column-pruning/pilot-report.md`.
//!
//! What is real and what is a stand-in:
//! - The rule code (`PruneColumns` of the operators, `ColumnPruner`, `ProjectionEliminator`,
//!   `eliminatePhysicalProjection`, `rebuildRange`'s shape) is ported function by function
//!   with `// go:` provenance, on top of the real expression crate (`tidb-expression`: `Arc<Expr>`,
//!   `Column`, `Schema`, `GetUsedList`, `ExtractColumnsFromExpressions`, `FoldConstant`).
//! - Operators carry only the fields the two rules read. Types of unported blocks are
//!   boundary stubs marked `blocked-by:<block>` (aggregation descriptors D08.11, handle
//!   columns, the optimizer trace D09.13, ranger D10.4, stats).
//! - There is no plan builder: tests build the logical plans the Go builder would build
//!   (same operators, same order of plan ids), and compare against Go's golden outputs
//!   (`planner/core/testdata/plan_suite_unexported_out.json`).
#![deny(unsafe_code)]

pub mod aggregation;
pub mod arena;
pub mod base;
pub mod logicalop;
pub mod optimizetrace;
pub mod physicalop;
pub mod plan_cache_rebuild;
pub mod planctx;
pub mod rule_column_pruning;
pub mod rule_eliminate_projection;
pub mod rule_util;
pub mod util;

pub use arena::{PlanArena, PlanId};
pub use base::{LogicalKind, LogicalOptRule, LogicalPlan};
pub use planctx::PlanCtx;
