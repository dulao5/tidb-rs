//! Go package `pkg/planner/util` and its sub-packages `costusage` and `optimizetrace`
//! (block D10.10; `optimizetrace` is D09.13): access paths, handle columns, ORDER BY items,
//! clone helpers and the cost-model-v2 bookkeeping.

pub mod column;
pub mod coreusage;
pub mod costusage;
pub mod explain_misc;
pub mod fixcontrol;
pub mod handle_cols;
pub mod logicaltrace;
pub mod misc;
pub mod optimizetrace;
pub mod path;

pub use byitem::ByItems;
pub use handle_cols::{CommonHandleCols, HandleCols, IntHandleCols};
pub use path::{AccessPath, Col2Len};
/// `pkg/planner/util/byitem.go` lives in tidb-expression (D08.11 spec §4.7); re-exported here so
/// the planner's callers keep `util::byitem`.
pub use tidb_expression::byitem;
