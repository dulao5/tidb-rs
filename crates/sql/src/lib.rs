//! SQL layer of the read-only node: statement lowering ([`ir`]), name resolution and physical
//! planning ([`plan`]), handle ranges ([`ranges`]) and result metadata ([`types`]).
//!
//! See `docs/adr/0004-prototype-sql-layer.md` for the supported subset and plan shapes.

#![forbid(unsafe_code)]

pub mod ir;
pub mod plan;
pub mod ranges;
pub mod types;

pub use crate::ir::Lowered;
pub use crate::ir::Statement;
pub use crate::ir::parse_and_lower;
pub use crate::plan::Plan;
pub use crate::plan::PlanContext;
pub use crate::plan::plan_select;
