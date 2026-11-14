//! Go package `pkg/planner/core`: the plan builder, the optimizer driver and the physical
//! plan helpers, ported file by file with the VS1 work items.

pub mod common_plans;
pub mod core_init;
pub mod initialize;
pub mod plan;
pub mod planbuilder;
pub mod preprocess;
pub mod resolve;
pub mod util;
