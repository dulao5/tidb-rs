//! Go package `pkg/planner/core`: the plan builder, the optimizer driver and the physical
//! plan helpers, ported file by file with the VS1 work items.

pub mod access_object;
pub mod collect_column_stats_usage;
pub mod common_plans;
pub mod constraint;
pub mod core_init;
pub mod cost;
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
pub mod plan_cache_heap_size;
pub mod plan_cache_instance;
#[cfg(test)]
mod plan_cache_instance_tests;
pub mod plan_cache_rebuild;
#[cfg(test)]
mod plan_cache_tests;
pub mod plan_cache_utils;
pub mod plan_cacheable_checker;
pub mod plan_cost_ver1;
pub mod plan_cost_ver2;
pub mod plan_to_pb;
pub mod planbuilder;
pub mod point_get_plan;
pub mod preprocess;
pub mod property_cols_prune;
pub mod resolve;
pub mod resolve_indices;
pub mod rule;
pub mod rule_collect_plan_stats;
pub mod rule_column_pruning;
pub mod rule_derive_topn_from_window;
pub mod rule_eliminate_projection;
pub mod rule_inject_extra_projection;
pub mod rule_predicate_push_down;
pub mod rule_predicate_simplification;
pub mod rule_result_reorder;
pub mod rule_topn_push_down;
pub mod rule_util;
pub mod stats;
pub mod task;
pub mod task_base;
pub mod util;
