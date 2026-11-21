//! Go package `pkg/planner/core`: the plan builder, the optimizer driver and the physical
//! plan helpers, ported file by file with the VS1 work items.

pub mod common_plans;
pub mod constraint;
pub mod core_init;
pub mod exhaust_physical_plans;
pub mod explain;
pub mod expression_codec_fn;
pub mod expression_rewriter;
pub mod find_best_task;
pub mod initialize;
pub mod logical_plan_builder;
pub mod optimizer;
pub mod physical_plans;
#[cfg(test)]
mod physical_plans_tests;
pub mod plan;
pub mod plan_cache;
#[cfg(test)]
mod plan_cache_tests;
pub mod plan_cache_utils;
pub mod plan_cacheable_checker;
pub mod plan_cost_ver1;
pub mod planbuilder;
pub mod point_get_plan;
pub mod preprocess;
pub mod resolve;
pub mod rule;
pub mod rule_column_pruning;
pub mod rule_eliminate_projection;
pub mod rule_predicate_push_down;
pub mod rule_predicate_simplification;
pub mod rule_topn_push_down;
pub mod rule_util;
pub mod task;
pub mod task_base;
pub mod util;
