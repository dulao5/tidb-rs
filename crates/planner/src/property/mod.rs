//! Go package `pkg/planner/property` (block D10.10): the physical property a parent requires
//! of its children, the statistics of a plan's output, and the task types.

pub mod physical_property;
pub mod stats_info;
pub mod task_type;

pub use physical_property::*;
pub use stats_info::{GroupNDV, StatsInfo};
pub use task_type::*;
