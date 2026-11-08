//! go-file: pkg/sessionctx/variable/sysvar.go
//! go-file: pkg/sessionctx/variable/noop.go
//!
//! The session hooks of the system variable table (block D05.2): `SetSession`,
//! `SetGlobal`, `GetSession`, `GetGlobal` and `GetStateValue`. Go writes them as closures
//! inside the table literals (sysvar.go, noop.go); here they are functions named after the
//! variable (`ss_` SetSession, `sg_` SetGlobal, `gs_` GetSession, `gg_` GetGlobal, `gv_`
//! GetStateValue), each with the Go position of its closure. The checklist is tracker
//! `data/sysvar_hooks.tsv`; a Go hook that is not ported yet is listed in the `PENDING_*`
//! tables below, and the registry checks at construction (and in a unit test) that every
//! Go hook is either here or pending, and that no hook is here that Go does not have.

// Go names for constants (M-GONAME).
#![allow(non_upper_case_globals)]

use std::collections::HashSet;
use std::sync::LazyLock;

use tidb_base::Error;
use tidb_base::gostd;

use super::session::SessionVars;
use super::tidb_defs::*;
use super::tidb_vars::*;
use super::variable::{GetGlobalFn, GetSessionFn, GetStateValueFn, SetGlobalFn, SetSessionFn};
use super::varsutil::*;

/// The session hooks of one variable.
#[derive(Clone, Copy, Default)]
pub(crate) struct SessionHooks {
    pub(crate) set_session: Option<SetSessionFn>,
    pub(crate) set_global: Option<SetGlobalFn>,
    pub(crate) get_session: Option<GetSessionFn>,
    pub(crate) get_global: Option<GetGlobalFn>,
    pub(crate) get_state_value: Option<GetStateValueFn>,
}

/// The session hooks of a variable (the `SetSession`, `SetGlobal`, `GetSession`,
/// `GetGlobal` and `GetStateValue` fields of its Go table entry).
// glue: the hook fields of the Go table literals, keyed by variable name
pub(crate) fn hooks_of(name: &str) -> SessionHooks {
    SessionHooks {
        set_session: set_session(name),
        get_session: get_session(name),
        get_state_value: get_state_value(name),
        ..SessionHooks::default()
    }
}

// glue: the SetSession fields of the Go table literals, keyed by variable name
fn set_session(name: &str) -> Option<SetSessionFn> {
    Some(match name {
        "tidb_enforce_mpp" => ss_tidb_enforce_mpp,
        "tidb_max_tiflash_threads" => ss_tidb_max_tiflash_threads,
        "tidb_max_bytes_before_tiflash_external_join" => {
            ss_tidb_max_bytes_before_tiflash_external_join
        }
        "tidb_max_bytes_before_tiflash_external_group_by" => {
            ss_tidb_max_bytes_before_tiflash_external_group_by
        }
        "tidb_max_bytes_before_tiflash_external_sort" => {
            ss_tidb_max_bytes_before_tiflash_external_sort
        }
        "tiflash_mem_quota_query_per_node" => ss_tiflash_mem_quota_query_per_node,
        "tiflash_query_spill_ratio" => ss_tiflash_query_spill_ratio,
        "tidb_opt_projection_push_down" => ss_tidb_opt_projection_push_down,
        "tidb_opt_derive_topn" => ss_tidb_opt_derive_topn,
        "tidb_opt_agg_push_down" => ss_tidb_opt_agg_push_down,
        "tidb_opt_distinct_agg_push_down" => ss_tidb_opt_distinct_agg_push_down,
        "tidb_opt_skew_distinct_agg" => ss_tidb_opt_skew_distinct_agg,
        "tidb_opt_three_stage_distinct_agg" => ss_tidb_opt_three_stage_distinct_agg,
        "tidb_opt_enable_three_stage_multi_distinct_agg" => {
            ss_tidb_opt_enable_three_stage_multi_distinct_agg
        }
        "tidb_opt_enable_non_eval_scalar_subquery" => ss_tidb_opt_enable_non_eval_scalar_subquery,
        "tidb_opt_write_row_id" => ss_tidb_opt_write_row_id,
        "tidb_enable_chunk_rpc" => ss_tidb_enable_chunk_rpc,
        "tidb_optimizer_selectivity_level" => ss_tidb_optimizer_selectivity_level,
        "tidb_opt_index_prune_threshold" => ss_tidb_opt_index_prune_threshold,
        "tidb_enable_outer_join_reorder" => ss_tidb_enable_outer_join_reorder,
        "tidb_enable_null_aware_anti_join" => ss_tidb_enable_null_aware_anti_join,
        "tidb_opt_enable_semi_join_rewrite" => ss_tidb_opt_enable_semi_join_rewrite,
        "tidb_slow_query_file" => ss_tidb_slow_query_file,
        "tidb_wait_split_region_finish" => ss_tidb_wait_split_region_finish,
        "tidb_wait_split_region_timeout" => ss_tidb_wait_split_region_timeout,
        "tidb_low_resolution_tso" => ss_tidb_low_resolution_tso,
        "tidb_metric_query_step" => ss_tidb_metric_query_step,
        "tidb_cdc_write_source" => ss_tidb_cdc_write_source,
        "tidb_metric_query_range_duration" => ss_tidb_metric_query_range_duration,
        "tidb_request_source_type" => ss_tidb_request_source_type,
        "tidb_slow_txn_log_threshold" => ss_tidb_slow_txn_log_threshold,
        "tidb_enable_prepared_plan_cache" => ss_tidb_enable_prepared_plan_cache,
        "tidb_enable_prepared_plan_cache_memory_monitor" => {
            ss_tidb_enable_prepared_plan_cache_memory_monitor
        }
        "tidb_enable_non_prepared_plan_cache" => ss_tidb_enable_non_prepared_plan_cache,
        "tidb_enable_non_prepared_plan_cache_for_dml" => {
            ss_tidb_enable_non_prepared_plan_cache_for_dml
        }
        "tidb_opt_enable_fuzzy_binding" => ss_tidb_opt_enable_fuzzy_binding,
        "tidb_enable_plan_replayer_capture" => ss_tidb_enable_plan_replayer_capture,
        "max_execution_time" => ss_max_execution_time,
        "tikv_client_read_timeout" => ss_tikv_client_read_timeout,
        "default_collation_for_utf8mb4" => ss_default_collation_for_utf8mb4,
        "tidb_foreign_key_check_in_shared_lock" => ss_tidb_foreign_key_check_in_shared_lock,
        "auto_increment_increment" => ss_auto_increment_increment,
        "auto_increment_offset" => ss_auto_increment_offset,
        "windowing_use_high_precision" => ss_windowing_use_high_precision,
        "tidb_allow_mpp" => ss_tidb_allow_mpp,
        "tidb_allow_tiflash_cop" => ss_tidb_allow_tiflash_cop,
        "tiflash_fastscan" => ss_tiflash_fastscan,
        "tidb_mpp_store_fail_ttl" => ss_tidb_mpp_store_fail_ttl,
        "tidb_hash_exchange_with_new_collation" => ss_tidb_hash_exchange_with_new_collation,
        "tidb_broadcast_join_threshold_count" => ss_tidb_broadcast_join_threshold_count,
        "tidb_broadcast_join_threshold_size" => ss_tidb_broadcast_join_threshold_size,
        "tidb_prefer_broadcast_join_by_exchange_data_size" => {
            ss_tidb_prefer_broadcast_join_by_exchange_data_size
        }
        "tidb_opt_broadcast_cartesian_join" => ss_tidb_opt_broadcast_cartesian_join,
        "tidb_opt_mpp_outer_join_fixed_build_side" => ss_tidb_opt_mpp_outer_join_fixed_build_side,
        "tidb_executor_concurrency" => ss_tidb_executor_concurrency,
        "tidb_distsql_scan_concurrency" => ss_tidb_distsql_scan_concurrency,
        "tidb_analyze_distsql_scan_concurrency" => ss_tidb_analyze_distsql_scan_concurrency,
        "tidb_opt_limit_push_down_threshold" => ss_tidb_opt_limit_push_down_threshold,
        "tidb_opt_correlation_threshold" => ss_tidb_opt_correlation_threshold,
        "tidb_opt_enable_correlation_adjustment" => ss_tidb_opt_enable_correlation_adjustment,
        "tidb_opt_correlation_exp_factor" => ss_tidb_opt_correlation_exp_factor,
        "tidb_opt_cpu_factor" => ss_tidb_opt_cpu_factor,
        "tidb_opt_tiflash_concurrency_factor" => ss_tidb_opt_tiflash_concurrency_factor,
        "tidb_opt_copcpu_factor" => ss_tidb_opt_copcpu_factor,
        "tidb_opt_network_factor" => ss_tidb_opt_network_factor,
        "tidb_opt_scan_factor" => ss_tidb_opt_scan_factor,
        "tidb_opt_desc_factor" => ss_tidb_opt_desc_factor,
        "tidb_opt_seek_factor" => ss_tidb_opt_seek_factor,
        "tidb_opt_memory_factor" => ss_tidb_opt_memory_factor,
        "tidb_opt_disk_factor" => ss_tidb_opt_disk_factor,
        "tidb_opt_index_scan_cost_factor" => ss_tidb_opt_index_scan_cost_factor,
        "tidb_opt_index_reader_cost_factor" => ss_tidb_opt_index_reader_cost_factor,
        "tidb_opt_table_reader_cost_factor" => ss_tidb_opt_table_reader_cost_factor,
        "tidb_opt_table_full_scan_cost_factor" => ss_tidb_opt_table_full_scan_cost_factor,
        "tidb_opt_table_range_scan_cost_factor" => ss_tidb_opt_table_range_scan_cost_factor,
        "tidb_opt_table_rowid_scan_cost_factor" => ss_tidb_opt_table_rowid_scan_cost_factor,
        "tidb_opt_table_tiflash_scan_cost_factor" => ss_tidb_opt_table_tiflash_scan_cost_factor,
        "tidb_opt_index_lookup_cost_factor" => ss_tidb_opt_index_lookup_cost_factor,
        "tidb_opt_index_merge_cost_factor" => ss_tidb_opt_index_merge_cost_factor,
        "tidb_opt_sort_cost_factor" => ss_tidb_opt_sort_cost_factor,
        "tidb_opt_topn_cost_factor" => ss_tidb_opt_topn_cost_factor,
        "tidb_opt_limit_cost_factor" => ss_tidb_opt_limit_cost_factor,
        "tidb_opt_stream_agg_cost_factor" => ss_tidb_opt_stream_agg_cost_factor,
        "tidb_opt_hash_agg_cost_factor" => ss_tidb_opt_hash_agg_cost_factor,
        "tidb_opt_merge_join_cost_factor" => ss_tidb_opt_merge_join_cost_factor,
        "tidb_opt_hash_join_cost_factor" => ss_tidb_opt_hash_join_cost_factor,
        "tidb_opt_index_join_cost_factor" => ss_tidb_opt_index_join_cost_factor,
        "tidb_opt_index_join_max_scan_rows_ratio" => ss_tidb_opt_index_join_max_scan_rows_ratio,
        "tidb_enable_new_only_full_group_by_check" => ss_tidb_enable_new_only_full_group_by_check,
        "tidb_opt_concurrency_factor" => ss_tidb_opt_concurrency_factor,
        "tidb_opt_force_inline_cte" => ss_tidb_opt_force_inline_cte,
        "tidb_index_join_batch_size" => ss_tidb_index_join_batch_size,
        "tidb_index_lookup_size" => ss_tidb_index_lookup_size,
        "tidb_index_lookup_concurrency" => ss_tidb_index_lookup_concurrency,
        "tidb_index_lookup_join_concurrency" => ss_tidb_index_lookup_join_concurrency,
        "tidb_index_serial_scan_concurrency" => ss_tidb_index_serial_scan_concurrency,
        "tidb_skip_utf8_check" => ss_tidb_skip_utf8_check,
        "tidb_skip_ascii_check" => ss_tidb_skip_ascii_check,
        "tidb_max_chunk_size" => ss_tidb_max_chunk_size,
        "tidb_allow_batch_cop" => ss_tidb_allow_batch_cop,
        "tidb_shard_row_id_bits" => ss_tidb_shard_row_id_bits,
        "tidb_pre_split_regions" => ss_tidb_pre_split_regions,
        "tidb_init_chunk_size" => ss_tidb_init_chunk_size,
        "tidb_hash_join_concurrency" => ss_tidb_hash_join_concurrency,
        "tidb_projection_concurrency" => ss_tidb_projection_concurrency,
        "tidb_hashagg_partial_concurrency" => ss_tidb_hashagg_partial_concurrency,
        "tidb_hashagg_final_concurrency" => ss_tidb_hashagg_final_concurrency,
        "tidb_window_concurrency" => ss_tidb_window_concurrency,
        "tidb_merge_join_concurrency" => ss_tidb_merge_join_concurrency,
        "tidb_streamagg_concurrency" => ss_tidb_streamagg_concurrency,
        "tidb_index_merge_intersection_concurrency" => ss_tidb_index_merge_intersection_concurrency,
        "tidb_enable_parallel_apply" => ss_tidb_enable_parallel_apply,
        "tidb_mem_quota_apply_cache" => ss_tidb_mem_quota_apply_cache,
        "tidb_txn_entry_size_limit" => ss_tidb_txn_entry_size_limit,
        "tidb_constraint_check_in_place" => ss_tidb_constraint_check_in_place,
        "tidb_txn_mode" => ss_tidb_txn_mode,
        "tidb_enable_window_function" => ss_tidb_enable_window_function,
        "tidb_enable_pipelined_window_function" => ss_tidb_enable_pipelined_window_function,
        "tidb_opt_enable_no_decorrelate_in_select" => ss_tidb_opt_enable_no_decorrelate_in_select,
        "tidb_opt_enable_alternative_logical_plans" => ss_tidb_opt_enable_alternative_logical_plans,
        "tidb_enable_strict_double_type_check" => ss_tidb_enable_strict_double_type_check,
        "tidb_enable_strict_not_null_check" => ss_tidb_enable_strict_not_null_check,
        "tidb_enable_vectorized_expression" => ss_tidb_enable_vectorized_expression,
        "tidb_enable_fast_analyze" => ss_tidb_enable_fast_analyze,
        "tidb_enable_rate_limit_action" => ss_tidb_enable_rate_limit_action,
        "tidb_enable_auto_increment_in_generated" => ss_tidb_enable_auto_increment_in_generated,
        "tidb_placement_mode" => ss_tidb_placement_mode,
        "tidb_opt_join_reorder_threshold" => ss_tidb_opt_join_reorder_threshold,
        "tidb_opt_enable_advanced_join_reorder" => ss_tidb_opt_enable_advanced_join_reorder,
        "tidb_opt_join_reorder_through_proj" => ss_tidb_opt_join_reorder_through_proj,
        "tidb_opt_join_reorder_through_sel" => ss_tidb_opt_join_reorder_through_sel,
        "tidb_enable_noop_functions" => ss_tidb_enable_noop_functions,
        "tidb_adaptive_closest_read_threshold" => ss_tidb_adaptive_closest_read_threshold,
        "tidb_use_plan_baselines" => ss_tidb_use_plan_baselines,
        "tidb_evolve_plan_baselines" => ss_tidb_evolve_plan_baselines,
        "tidb_enable_extended_stats" => ss_tidb_enable_extended_stats,
        "cte_max_recursion_depth" => ss_cte_max_recursion_depth,
        "tidb_enable_async_commit" => ss_tidb_enable_async_commit,
        "tidb_enable_1pc" => ss_tidb_enable_1pc,
        "tidb_guarantee_linearizability" => ss_tidb_guarantee_linearizability,
        "tidb_analyze_version" => ss_tidb_analyze_version,
        "tidb_enable_index_merge_join" => ss_tidb_enable_index_merge_join,
        "tidb_track_aggregate_memory_usage" => ss_tidb_track_aggregate_memory_usage,
        "tidb_multi_statement_mode" => ss_tidb_multi_statement_mode,
        "tidb_enable_exchange_partition" => ss_tidb_enable_exchange_partition,
        "tidb_tmp_table_max_size" => ss_tidb_tmp_table_max_size,
        "tidb_enable_ordered_result_mode" => ss_tidb_enable_ordered_result_mode,
        "tidb_enable_pseudo_for_outdated_stats" => ss_tidb_enable_pseudo_for_outdated_stats,
        "tidb_regard_null_as_point" => ss_tidb_regard_null_as_point,
        "tidb_enable_paging" => ss_tidb_enable_paging,
        "tidb_enable_legacy_instance_scope" => ss_tidb_enable_legacy_instance_scope,
        "tidb_enable_mutation_checker" => ss_tidb_enable_mutation_checker,
        "tidb_rc_write_check_ts" => ss_tidb_rc_write_check_ts,
        "tidb_remove_orderby_in_subquery" => ss_tidb_remove_orderby_in_subquery,
        "tidb_nontransactional_ignore_error" => ss_tidb_nontransactional_ignore_error,
        "tiflash_fine_grained_shuffle_stream_count" => ss_tiflash_fine_grained_shuffle_stream_count,
        "tiflash_fine_grained_shuffle_batch_size" => ss_tiflash_fine_grained_shuffle_batch_size,
        "tidb_min_paging_size" => ss_tidb_min_paging_size,
        "tidb_max_paging_size" => ss_tidb_max_paging_size,
        "tidb_memory_debug_mode_min_heap_inuse" => ss_tidb_memory_debug_mode_min_heap_inuse,
        "tidb_memory_debug_mode_alarm_ratio" => ss_tidb_memory_debug_mode_alarm_ratio,
        "sql_require_primary_key" => ss_sql_require_primary_key,
        "tidb_enable_analyze_snapshot" => ss_tidb_enable_analyze_snapshot,
        "tidb_default_string_match_selectivity" => ss_tidb_default_string_match_selectivity,
        "tidb_enable_tiflash_read_for_write_stmt" => ss_tidb_enable_tiflash_read_for_write_stmt,
        "tidb_enable_unsafe_substitute" => ss_tidb_enable_unsafe_substitute,
        "tidb_opt_range_max_size" => ss_tidb_opt_range_max_size,
        "tidb_opt_advanced_join_hint" => ss_tidb_opt_advanced_join_hint,
        "tidb_opt_use_invisible_indexes" => ss_tidb_opt_use_invisible_indexes,
        "tidb_analyze_partition_concurrency" => ss_tidb_analyze_partition_concurrency,
        "tidb_merge_partition_stats_concurrency" => ss_tidb_merge_partition_stats_concurrency,
        "tidb_enable_async_merge_global_stats" => ss_tidb_enable_async_merge_global_stats,
        "tidb_opt_prefix_index_single_scan" => ss_tidb_opt_prefix_index_single_scan,
        "tidb_opt_partial_ordered_index_for_topn" => ss_tidb_opt_partial_ordered_index_for_topn,
        "tidb_enable_external_ts_read" => ss_tidb_enable_external_ts_read,
        "tidb_enable_reuse_chunk" => ss_tidb_enable_reuse_chunk,
        "tidb_store_batch_size" => ss_tidb_store_batch_size,
        "tidb_pessimistic_txn_fair_locking" => ss_tidb_pessimistic_txn_fair_locking,
        "tidb_enable_plan_cache_for_param_limit" => ss_tidb_enable_plan_cache_for_param_limit,
        "tidb_enable_inl_join_inner_multi_pattern" => ss_tidb_enable_inl_join_inner_multi_pattern,
        "tidb_enable_plan_cache_for_subquery" => ss_tidb_enable_plan_cache_for_subquery,
        "tidb_opt_enable_late_materialization" => ss_tidb_opt_enable_late_materialization,
        "tidb_opt_ordering_index_selectivity_threshold" => {
            ss_tidb_opt_ordering_index_selectivity_threshold
        }
        "tidb_opt_ordering_index_selectivity_ratio" => ss_tidb_opt_ordering_index_selectivity_ratio,
        "tidb_opt_enable_mpp_shared_cte_execution" => ss_tidb_opt_enable_mpp_shared_cte_execution,
        "tidb_plan_cache_invalidation_on_fresh_stats" => {
            ss_tidb_plan_cache_invalidation_on_fresh_stats
        }
        "tidb_enable_fast_table_check" => ss_tidb_enable_fast_table_check,
        "tidb_skip_missing_partition_stats" => ss_tidb_skip_missing_partition_stats,
        "tidb_idle_transaction_timeout" => ss_tidb_idle_transaction_timeout,
        "div_precision_increment" => ss_div_precision_increment,
        "tiflash_hashagg_preaggregation_mode" => ss_tiflash_hashagg_preaggregation_mode,
        "tidb_enable_lazy_cursor_fetch" => ss_tidb_enable_lazy_cursor_fetch,
        "tidb_enable_cache_prepare_stmt" => ss_tidb_enable_cache_prepare_stmt,
        "tidb_batch_insert" => ss_tidb_batch_insert,
        "tidb_batch_delete" => ss_tidb_batch_delete,
        "tidb_batch_commit" => ss_tidb_batch_commit,
        "tidb_allow_remove_auto_inc" => ss_tidb_allow_remove_auto_inc,
        "tidb_dml_batch_size" => ss_tidb_dml_batch_size,
        "tidb_retry_limit" => ss_tidb_retry_limit,
        "tidb_disable_txn_auto_retry" => ss_tidb_disable_txn_auto_retry,
        "allow_auto_random_explicit_insert" => ss_allow_auto_random_explicit_insert,
        "tidb_shard_allocate_step" => ss_tidb_shard_allocate_step,
        "tidb_ddl_reorg_priority" => ss_tidb_ddl_reorg_priority,
        "tidb_read_consistency" => ss_tidb_read_consistency,
        "tidb_prepared_plan_cache_size" => ss_tidb_prepared_plan_cache_size,
        "tidb_non_prepared_plan_cache_size" => ss_tidb_non_prepared_plan_cache_size,
        "tidb_plan_cache_max_plan_size" => ss_tidb_plan_cache_max_plan_size,
        "tidb_session_plan_cache_size" => ss_tidb_session_plan_cache_size,
        "tidb_enable_plan_replayer_continuous_capture" => {
            ss_tidb_enable_plan_replayer_continuous_capture
        }
        "sql_select_limit" => ss_sql_select_limit,
        "collation_server" => ss_collation_server,
        "collation_database" => ss_collation_database,
        "collation_connection" => ss_collation_connection,
        "character_set_database" => ss_character_set_database,
        "innodb_lock_wait_timeout" => ss_innodb_lock_wait_timeout,
        "character_set_connection" => ss_character_set_connection,
        "character_set_server" => ss_character_set_server,
        "max_allowed_packet" => ss_max_allowed_packet,
        "tidb_opt_insubq_to_join_and_agg" => ss_tidb_opt_insubq_to_join_and_agg,
        "tidb_opt_prefer_range_scan" => ss_tidb_opt_prefer_range_scan,
        "tidb_enable_cascades_planner" => ss_tidb_enable_cascades_planner,
        "tidb_enable_index_merge" => ss_tidb_enable_index_merge,
        "tidb_enable_clustered_index" => ss_tidb_enable_clustered_index,
        "tidb_hash_join_version" => ss_tidb_hash_join_version,
        "tidb_opt_enable_hash_join" => ss_tidb_opt_enable_hash_join,
        "tidb_txn_assertion_level" => ss_tidb_txn_assertion_level,
        "tidb_batch_pending_tiflash_count" => ss_tidb_batch_pending_tiflash_count,
        _ => return None,
    })
}

// glue: the GetSession fields of the Go table literals, keyed by variable name
fn get_session(name: &str) -> Option<GetSessionFn> {
    Some(match name {
        "warning_count" => gs_warning_count,
        "error_count" => gs_error_count,
        "tidb_last_txn_info" => gs_tidb_last_txn_info,
        "last_plan_from_cache" => gs_last_plan_from_cache,
        "last_plan_from_binding" => gs_last_plan_from_binding,
        "rand_seed1" => gs_rand_seed1,
        "rand_seed2" => gs_rand_seed2,
        "tidb_last_plan_replayer_token" => gs_tidb_last_plan_replayer_token,
        "last_sql_use_alloc" => gs_last_sql_use_alloc,
        "tidb_request_source_type" => gs_tidb_request_source_type,
        "max_allowed_packet" => gs_max_allowed_packet,
        "tidb_enable_inl_join_inner_multi_pattern" => gs_tidb_enable_inl_join_inner_multi_pattern,
        _ => return None,
    })
}

// glue: the GetStateValue fields of the Go table literals, keyed by variable name
fn get_state_value(name: &str) -> Option<GetStateValueFn> {
    Some(match name {
        "timestamp" => gv_timestamp,
        "last_insert_id" => gv_last_insert_id,
        "identity" => gv_identity,
        _ => return None,
    })
}

/// Whether Go's `kind` hook of `name` is not ported yet.
// glue: the pending part of the D05.2 hook checklist
pub(crate) fn is_pending(name: &str, kind: &str) -> bool {
    static SETS: LazyLock<[(&str, HashSet<&str>); 5]> = LazyLock::new(|| {
        [
            ("SetSession", PENDING_SET_SESSION.iter().copied().collect()),
            ("SetGlobal", PENDING_SET_GLOBAL.iter().copied().collect()),
            ("GetSession", PENDING_GET_SESSION.iter().copied().collect()),
            ("GetGlobal", PENDING_GET_GLOBAL.iter().copied().collect()),
            (
                "GetStateValue",
                PENDING_GET_STATE_VALUE.iter().copied().collect(),
            ),
        ]
    });
    SETS.iter().any(|(k, set)| *k == kind && set.contains(name))
}

/// Variables whose Go entry has a `SetSession` hook that is not ported yet.
const PENDING_SET_SESSION: &[&str] = &[
    "autocommit",
    "group_concat_max_len",
    "mpp_exchange_compression_mode",
    "mpp_version",
    "rand_seed1",
    "rand_seed2",
    "sql_mode",
    "tidb_allow_fallback_to_tikv",
    "tidb_analyze_skip_column_types",
    "tidb_backoff_lock_fast",
    "tidb_backoff_weight",
    "tidb_constraint_check_in_place_pessimistic",
    "tidb_cost_model_version",
    "tidb_dml_type",
    "tidb_enable_new_cost_interface",
    "tidb_enable_parallel_hashagg_spill",
    "tidb_enable_shared_lock_promotion",
    "tidb_ignore_prepared_cache_close_stmt",
    "tidb_index_join_double_read_penalty_cost_rate",
    "tidb_index_lookup_pushdown_policy",
    "tidb_isolation_read_engines",
    "tidb_load_based_replica_read_threshold",
    "tidb_lock_unchanged_keys",
    "tidb_mem_quota_query",
    "tidb_opt_fix_control",
    "tidb_opt_objective",
    "tidb_partition_prune_mode",
    "tidb_read_staleness",
    "tidb_redact_log",
    "tidb_replica_read",
    "tidb_row_format_version",
    "tidb_runtime_filter_mode",
    "tidb_runtime_filter_type",
    "tidb_scatter_region",
    "tidb_session_alias",
    "tidb_slow_log_rules",
    "tidb_snapshot",
    "tidb_stats_load_sync_wait",
    "tidb_stats_update_during_ddl",
    "tidb_sysdate_is_now",
    "tiflash_compute_dispatch_policy",
    "tiflash_replica_read",
    "time_zone",
    "tx_isolation_one_shot",
    "tx_read_ts",
    "txn_scope",
];

/// Variables whose Go entry has a `SetGlobal` hook that is not ported yet.
const PENDING_SET_GLOBAL: &[&str] = &[
    "authentication_ldap_sasl_auth_method_name",
    "authentication_ldap_sasl_bind_base_dn",
    "authentication_ldap_sasl_bind_root_dn",
    "authentication_ldap_sasl_bind_root_pwd",
    "authentication_ldap_sasl_ca_path",
    "authentication_ldap_sasl_init_pool_size",
    "authentication_ldap_sasl_max_pool_size",
    "authentication_ldap_sasl_server_host",
    "authentication_ldap_sasl_server_port",
    "authentication_ldap_sasl_tls",
    "authentication_ldap_sasl_user_search_attr",
    "authentication_ldap_simple_auth_method_name",
    "authentication_ldap_simple_bind_base_dn",
    "authentication_ldap_simple_bind_root_dn",
    "authentication_ldap_simple_bind_root_pwd",
    "authentication_ldap_simple_ca_path",
    "authentication_ldap_simple_init_pool_size",
    "authentication_ldap_simple_max_pool_size",
    "authentication_ldap_simple_server_host",
    "authentication_ldap_simple_server_port",
    "authentication_ldap_simple_tls",
    "authentication_ldap_simple_user_search_attr",
    "ddl_slow_threshold",
    "max_connections",
    "max_prepared_stmt_count",
    "max_user_connections",
    "password_history",
    "password_reuse_interval",
    "pd_enable_follower_handle_region",
    "performance_schema_session_connect_attrs_size",
    "require_secure_transport",
    "tidb_accelerate_user_creation_update",
    "tidb_advancer_check_point_lag_limit",
    "tidb_analyze_column_options",
    "tidb_auto_analyze_concurrency",
    "tidb_auto_analyze_partition_batch_size",
    "tidb_cb_pd_metadata_error_rate_threshold_ratio",
    "tidb_check_mb4_value_in_utf8",
    "tidb_cloud_storage_uri",
    "tidb_committer_concurrency",
    "tidb_ddl_disk_quota",
    "tidb_ddl_enable_fast_reorg",
    "tidb_ddl_error_count_limit",
    "tidb_ddl_flashback_concurrency",
    "tidb_ddl_reorg_batch_size",
    "tidb_ddl_reorg_max_write_speed",
    "tidb_ddl_reorg_worker_cnt",
    "tidb_enable_auto_analyze",
    "tidb_enable_auto_analyze_priority_queue",
    "tidb_enable_batch_dml",
    "tidb_enable_batch_query_region",
    "tidb_enable_check_constraint",
    "tidb_enable_collect_execution_info",
    "tidb_enable_column_tracking",
    "tidb_enable_ddl",
    "tidb_enable_dist_task",
    "tidb_enable_fast_create_table",
    "tidb_enable_foreign_key",
    "tidb_enable_gc_aware_memory_track",
    "tidb_enable_gogc_tuner",
    "tidb_enable_historical_stats_for_capture",
    "tidb_enable_instance_plan_cache",
    "tidb_enable_local_txn",
    "tidb_enable_metadata_lock",
    "tidb_enable_noop_variables",
    "tidb_enable_paging",
    "tidb_enable_resource_control",
    "tidb_enable_row_level_checksum",
    "tidb_enable_slow_log",
    "tidb_enable_stats_owner",
    "tidb_enable_stmt_summary",
    "tidb_enable_tiflash_pipeline_model",
    "tidb_enable_tmp_storage_on_oom",
    "tidb_enable_tso_follower_proxy",
    "tidb_expensive_query_time_threshold",
    "tidb_expensive_txn_time_threshold",
    "tidb_external_ts",
    "tidb_force_priority",
    "tidb_gc_concurrency",
    "tidb_gc_enable",
    "tidb_gc_life_time",
    "tidb_gc_max_wait_time",
    "tidb_gc_run_interval",
    "tidb_gc_scan_lock_mode",
    "tidb_general_log",
    "tidb_generate_binary_plan",
    "tidb_gogc_tuner_max_value",
    "tidb_gogc_tuner_min_value",
    "tidb_gogc_tuner_threshold",
    "tidb_historical_stats_duration",
    "tidb_ignore_inlist_plan_digest",
    "tidb_instance_plan_cache_max_size",
    "tidb_instance_plan_cache_reserved_percentage",
    "tidb_load_binding_timeout",
    "tidb_log_file_max_days",
    "tidb_low_resolution_tso_update_interval",
    "tidb_max_auto_analyze_time",
    "tidb_max_delta_schema_count",
    "tidb_mem_oom_action",
    "tidb_mem_quota_analyze",
    "tidb_mem_quota_binding_cache",
    "tidb_memory_usage_alarm_keep_record_num",
    "tidb_memory_usage_alarm_ratio",
    "tidb_opt_fix_control",
    "tidb_partition_prune_mode",
    "tidb_persist_analyze_options",
    "tidb_pprof_sql_cpu",
    "tidb_prepared_plan_cache_memory_guard_ratio",
    "tidb_query_log_max_len",
    "tidb_rc_read_check_ts",
    "tidb_record_plan_in_slow_log",
    "tidb_resource_control_strict_mode",
    "tidb_restricted_read_only",
    "tidb_row_format_version",
    "tidb_schema_cache_size",
    "tidb_schema_version_cache_limit",
    "tidb_server_memory_limit",
    "tidb_server_memory_limit_gc_trigger",
    "tidb_server_memory_limit_sess_min_size",
    "tidb_service_scope",
    "tidb_simplified_metrics",
    "tidb_slow_log_max_per_sec",
    "tidb_slow_log_rules",
    "tidb_slow_log_threshold",
    "tidb_stats_cache_mem_quota",
    "tidb_stats_load_pseudo_timeout",
    "tidb_stats_load_sync_wait",
    "tidb_stmt_summary_history_size",
    "tidb_stmt_summary_internal_query",
    "tidb_stmt_summary_max_sql_length",
    "tidb_stmt_summary_max_stmt_count",
    "tidb_stmt_summary_refresh_interval",
    "tidb_store_limit",
    "tidb_super_read_only",
    "tidb_table_cache_lease",
    "tidb_top_sql_max_meta_count",
    "tidb_top_sql_max_time_series_count",
    "tidb_tso_client_batch_max_wait_time",
    "tidb_tso_client_rpc_mode",
    "tidb_ttl_delete_batch_size",
    "tidb_ttl_delete_rate_limit",
    "tidb_ttl_delete_worker_count",
    "tidb_ttl_job_enable",
    "tidb_ttl_job_schedule_window_end_time",
    "tidb_ttl_job_schedule_window_start_time",
    "tidb_ttl_running_tasks",
    "tidb_ttl_scan_batch_size",
    "tidb_ttl_scan_worker_count",
    "tidb_txn_commit_batch_size",
    "tidb_txn_entry_size_limit",
    "tiflash_compute_dispatch_policy",
    "validate_password.length",
    "validate_password.mixed_case_count",
    "validate_password.number_count",
    "validate_password.special_char_count",
];

/// Variables whose Go entry has a `GetSession` hook that is not ported yet.
const PENDING_GET_SESSION: &[&str] = &[
    "group_concat_max_len",
    "identity",
    "last_insert_id",
    "tidb_current_ts",
    "tidb_enable_plan_replayer_capture",
    "tidb_enable_plan_replayer_continuous_capture",
    "tidb_last_ddl_info",
    "tidb_last_query_info",
    "tidb_partition_prune_mode",
    "tidb_scatter_region",
    "tidb_session_alias",
    "tidb_slow_log_rules",
    "tiflash_replica_read",
    "timestamp",
    "txn_scope",
];

/// Variables whose Go entry has a `GetGlobal` hook that is not ported yet.
const PENDING_GET_GLOBAL: &[&str] = &[
    "authentication_ldap_sasl_auth_method_name",
    "authentication_ldap_sasl_bind_base_dn",
    "authentication_ldap_sasl_bind_root_dn",
    "authentication_ldap_sasl_bind_root_pwd",
    "authentication_ldap_sasl_ca_path",
    "authentication_ldap_sasl_init_pool_size",
    "authentication_ldap_sasl_max_pool_size",
    "authentication_ldap_sasl_server_host",
    "authentication_ldap_sasl_server_port",
    "authentication_ldap_sasl_tls",
    "authentication_ldap_sasl_user_search_attr",
    "authentication_ldap_simple_bind_base_dn",
    "authentication_ldap_simple_bind_root_dn",
    "authentication_ldap_simple_bind_root_pwd",
    "authentication_ldap_simple_ca_path",
    "authentication_ldap_simple_init_pool_size",
    "authentication_ldap_simple_max_pool_size",
    "authentication_ldap_simple_server_host",
    "authentication_ldap_simple_server_port",
    "authentication_ldap_simple_tls",
    "authentication_ldap_simple_user_search_attr",
    "ddl_slow_threshold",
    "disconnect_on_expired_password",
    "max_connections",
    "max_user_connections",
    "password_history",
    "password_reuse_interval",
    "pd_enable_follower_handle_region",
    "performance_schema_session_connect_attrs_size",
    "plugin_audit_log_buffer_size",
    "plugin_audit_log_flush_interval",
    "plugin_dir",
    "plugin_load",
    "require_secure_transport",
    "tidb_advancer_check_point_lag_limit",
    "tidb_analyze_column_options",
    "tidb_auto_analyze_concurrency",
    "tidb_cb_pd_metadata_error_rate_threshold_ratio",
    "tidb_check_mb4_value_in_utf8",
    "tidb_cloud_storage_uri",
    "tidb_committer_concurrency",
    "tidb_config",
    "tidb_ddl_disk_quota",
    "tidb_ddl_enable_fast_reorg",
    "tidb_ddl_reorg_max_write_speed",
    "tidb_enable_auto_analyze",
    "tidb_enable_auto_analyze_priority_queue",
    "tidb_enable_batch_dml",
    "tidb_enable_batch_query_region",
    "tidb_enable_check_constraint",
    "tidb_enable_collect_execution_info",
    "tidb_enable_column_tracking",
    "tidb_enable_ddl",
    "tidb_enable_dist_task",
    "tidb_enable_fast_create_table",
    "tidb_enable_foreign_key",
    "tidb_enable_gc_aware_memory_track",
    "tidb_enable_historical_stats_for_capture",
    "tidb_enable_instance_plan_cache",
    "tidb_enable_local_txn",
    "tidb_enable_metadata_lock",
    "tidb_enable_noop_variables",
    "tidb_enable_resource_control",
    "tidb_enable_row_level_checksum",
    "tidb_enable_slow_log",
    "tidb_enable_stats_owner",
    "tidb_enable_tiflash_pipeline_model",
    "tidb_enable_tmp_storage_on_oom",
    "tidb_enable_tso_follower_proxy",
    "tidb_expensive_query_time_threshold",
    "tidb_expensive_txn_time_threshold",
    "tidb_external_ts",
    "tidb_force_priority",
    "tidb_gc_concurrency",
    "tidb_gc_enable",
    "tidb_gc_life_time",
    "tidb_gc_run_interval",
    "tidb_gc_scan_lock_mode",
    "tidb_general_log",
    "tidb_gogc_tuner_max_value",
    "tidb_gogc_tuner_min_value",
    "tidb_gogc_tuner_threshold",
    "tidb_historical_stats_duration",
    "tidb_ignore_inlist_plan_digest",
    "tidb_instance_plan_cache_max_size",
    "tidb_instance_plan_cache_reserved_percentage",
    "tidb_log_file_max_days",
    "tidb_max_auto_analyze_time",
    "tidb_mem_oom_action",
    "tidb_mem_quota_analyze",
    "tidb_mem_quota_binding_cache",
    "tidb_memory_usage_alarm_keep_record_num",
    "tidb_memory_usage_alarm_ratio",
    "tidb_persist_analyze_options",
    "tidb_pprof_sql_cpu",
    "tidb_prepared_plan_cache_memory_guard_ratio",
    "tidb_query_log_max_len",
    "tidb_rc_read_check_ts",
    "tidb_record_plan_in_slow_log",
    "tidb_resource_control_strict_mode",
    "tidb_schema_cache_size",
    "tidb_server_memory_limit",
    "tidb_server_memory_limit_gc_trigger",
    "tidb_server_memory_limit_sess_min_size",
    "tidb_service_scope",
    "tidb_slow_log_max_per_sec",
    "tidb_slow_log_rules",
    "tidb_slow_log_threshold",
    "tidb_stats_cache_mem_quota",
    "tidb_stats_load_pseudo_timeout",
    "tidb_stats_load_sync_wait",
    "tidb_stmt_summary_enable_persistent",
    "tidb_stmt_summary_file_max_backups",
    "tidb_stmt_summary_file_max_days",
    "tidb_stmt_summary_file_max_size",
    "tidb_stmt_summary_filename",
    "tidb_store_limit",
    "tidb_top_sql_max_meta_count",
    "tidb_top_sql_max_time_series_count",
    "tidb_tso_client_batch_max_wait_time",
    "tidb_ttl_delete_batch_size",
    "tidb_ttl_delete_rate_limit",
    "tidb_ttl_delete_worker_count",
    "tidb_ttl_job_enable",
    "tidb_ttl_job_schedule_window_end_time",
    "tidb_ttl_job_schedule_window_start_time",
    "tidb_ttl_running_tasks",
    "tidb_ttl_scan_batch_size",
    "tidb_ttl_scan_worker_count",
    "tidb_txn_commit_batch_size",
    "validate_password.length",
    "validate_password.mixed_case_count",
    "validate_password.number_count",
    "validate_password.special_char_count",
];

/// Variables whose Go entry has a `GetStateValue` hook that is not ported yet.
const PENDING_GET_STATE_VALUE: &[&str] = &["rand_seed1", "rand_seed2", "tx_isolation_one_shot"];

// go-part: defaultSysVars (sysvar.go:L163 tidb_enforce_mpp SetSession closure) L1
fn ss_tidb_enforce_mpp(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.enforce_mpp_execution = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L167 tidb_max_tiflash_threads SetSession closure) L1
fn ss_tidb_max_tiflash_threads(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.tiflash_max_threads = tidb_opt_int64(val, DefTiFlashMaxThreads);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L171 tidb_max_bytes_before_tiflash_external_join SetSession closure) L1
fn ss_tidb_max_bytes_before_tiflash_external_join(
    s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    s.tiflash_max_bytes_before_external_join =
        tidb_opt_int64(val, DefTiFlashMaxBytesBeforeExternalJoin);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L175 tidb_max_bytes_before_tiflash_external_group_by SetSession closure) L1
fn ss_tidb_max_bytes_before_tiflash_external_group_by(
    s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    s.tiflash_max_bytes_before_external_group_by =
        tidb_opt_int64(val, DefTiFlashMaxBytesBeforeExternalGroupBy);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L179 tidb_max_bytes_before_tiflash_external_sort SetSession closure) L1
fn ss_tidb_max_bytes_before_tiflash_external_sort(
    s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    s.tiflash_max_bytes_before_external_sort =
        tidb_opt_int64(val, DefTiFlashMaxBytesBeforeExternalSort);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L183 tiflash_mem_quota_query_per_node SetSession closure) L1
fn ss_tiflash_mem_quota_query_per_node(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.tiflash_max_query_memory_per_node = tidb_opt_int64(val, DefTiFlashMemQuotaQueryPerNode);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L196 tiflash_query_spill_ratio SetSession closure) L1
fn ss_tiflash_query_spill_ratio(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.tiflash_query_spill_ratio = tidb_opt_float64(val, DefTiFlashQuerySpillRatio);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L216 tidb_opt_projection_push_down SetSession closure) L1
fn ss_tidb_opt_projection_push_down(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.allow_projection_push_down = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L220 tidb_opt_derive_topn SetSession closure) L1
fn ss_tidb_opt_derive_topn(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.allow_derive_top_n = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L224 tidb_opt_agg_push_down SetSession closure) L1
fn ss_tidb_opt_agg_push_down(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.allow_agg_push_down = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L228 tidb_opt_distinct_agg_push_down SetSession closure) L1
fn ss_tidb_opt_distinct_agg_push_down(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.allow_distinct_agg_push_down = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L232 tidb_opt_skew_distinct_agg SetSession closure) L1
fn ss_tidb_opt_skew_distinct_agg(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.enable_skew_distinct_agg = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L236 tidb_opt_three_stage_distinct_agg SetSession closure) L1
fn ss_tidb_opt_three_stage_distinct_agg(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.enable_3stage_distinct_agg = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L240 tidb_opt_enable_three_stage_multi_distinct_agg SetSession closure) L1
fn ss_tidb_opt_enable_three_stage_multi_distinct_agg(
    s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    s.enable_3stage_multi_distinct_agg = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L244 tidb_opt_enable_non_eval_scalar_subquery SetSession closure) L1
fn ss_tidb_opt_enable_non_eval_scalar_subquery(
    s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    s.explain_non_evaled_sub_query = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L248 tidb_opt_write_row_id SetSession closure) L1
fn ss_tidb_opt_write_row_id(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.allow_write_row_id = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L278 tidb_enable_chunk_rpc SetSession closure) L1
fn ss_tidb_enable_chunk_rpc(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.enable_chunk_rpc = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L294 tidb_optimizer_selectivity_level SetSession closure) L1
fn ss_tidb_optimizer_selectivity_level(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.optimizer_selectivity_level =
        tidb_opt_positive_int32(val, DefTiDBOptimizerSelectivityLevel as isize) as i64;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L298 tidb_opt_index_prune_threshold SetSession closure) L1
fn ss_tidb_opt_index_prune_threshold(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.opt_index_prune_threshold = tidb_opt_int(val, DefTiDBOptIndexPruneThreshold as isize) as i64;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L302 tidb_enable_outer_join_reorder SetSession closure) L1
fn ss_tidb_enable_outer_join_reorder(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.enable_outer_join_reorder = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L306 tidb_enable_null_aware_anti_join SetSession closure) L1
fn ss_tidb_enable_null_aware_anti_join(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.optimizer_enable_naaj = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L310 tidb_opt_enable_semi_join_rewrite SetSession closure) L1
fn ss_tidb_opt_enable_semi_join_rewrite(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.enable_semi_join_rewrite = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L318 tidb_slow_query_file SetSession closure) L1
fn ss_tidb_slow_query_file(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.slow_query_file = val.to_owned();
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L322 tidb_wait_split_region_finish SetSession closure) L1
fn ss_tidb_wait_split_region_finish(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.wait_split_region_finish = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L326 tidb_wait_split_region_timeout SetSession closure) L1
fn ss_tidb_wait_split_region_timeout(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.wait_split_region_timeout =
        (tidb_opt_positive_int32(val, DefWaitSplitRegionTimeout as isize)) as u64;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L330 tidb_low_resolution_tso SetSession closure) L1
fn ss_tidb_low_resolution_tso(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.low_resolution_tso = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L375 tidb_metric_query_step SetSession closure) L1
fn ss_tidb_metric_query_step(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.metric_schema_step = tidb_opt_int64(val, DefTiDBMetricSchemaStep);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L379 tidb_cdc_write_source SetSession closure) L1
fn ss_tidb_cdc_write_source(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.cdc_write_source = (tidb_opt_int(val, 0)) as u64;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L383 tidb_metric_query_range_duration SetSession closure) L1
fn ss_tidb_metric_query_range_duration(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.metric_schema_range_duration = tidb_opt_int64(val, DefTiDBMetricSchemaRangeDuration);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L435 tidb_request_source_type SetSession closure) L1
fn ss_tidb_request_source_type(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.explicit_request_source_type = val.to_owned();
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L467 tidb_slow_txn_log_threshold SetSession closure) L1
fn ss_tidb_slow_txn_log_threshold(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.slow_txn_threshold = tidb_opt_uint64(val, DefaultSlowTxnThreshold);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1356 tidb_enable_prepared_plan_cache SetSession closure) L1
fn ss_tidb_enable_prepared_plan_cache(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.enable_prepared_plan_cache = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1370 tidb_enable_prepared_plan_cache_memory_monitor SetSession closure) L1
fn ss_tidb_enable_prepared_plan_cache_memory_monitor(
    s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    s.enable_prepared_plan_cache_memory_monitor = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1383 tidb_enable_non_prepared_plan_cache SetSession closure) L1
fn ss_tidb_enable_non_prepared_plan_cache(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.enable_non_prepared_plan_cache = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1387 tidb_enable_non_prepared_plan_cache_for_dml SetSession closure) L1
fn ss_tidb_enable_non_prepared_plan_cache_for_dml(
    s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    s.enable_non_prepared_plan_cache_for_dml = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1397 tidb_opt_enable_fuzzy_binding SetSession closure) L1
fn ss_tidb_opt_enable_fuzzy_binding(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.enable_fuzzy_binding = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1642 tidb_enable_plan_replayer_capture SetSession closure) L1
fn ss_tidb_enable_plan_replayer_capture(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.enable_plan_replayer_capture = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1726 max_execution_time SetSession closure) L1
fn ss_max_execution_time(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.max_execution_time = (tidb_opt_positive_int32(val, 0)) as u64;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1739 tikv_client_read_timeout SetSession closure) L1
fn ss_tikv_client_read_timeout(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.tikv_client_read_timeout = (tidb_opt_positive_int32(val, 0)) as u64;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1758 default_collation_for_utf8mb4 SetSession closure) L1
fn ss_default_collation_for_utf8mb4(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.default_collation_for_utf8mb4 = val.to_owned();
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1794 tidb_foreign_key_check_in_shared_lock SetSession closure) L1
fn ss_tidb_foreign_key_check_in_shared_lock(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.foreign_key_check_in_shared_lock = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1812 auto_increment_increment SetSession closure) L1
fn ss_auto_increment_increment(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.auto_increment_increment =
        tidb_opt_positive_int32(val, DefAutoIncrementIncrement as isize) as i64;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1817 auto_increment_offset SetSession closure) L1
fn ss_auto_increment_offset(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.auto_increment_offset = tidb_opt_positive_int32(val, DefAutoIncrementOffset as isize) as i64;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1960 windowing_use_high_precision SetSession closure) L1
fn ss_windowing_use_high_precision(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.windowing_use_high_precision = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1966 tidb_allow_mpp SetSession closure) L1
fn ss_tidb_allow_mpp(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.allow_mpp_execution = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1970 tidb_allow_tiflash_cop SetSession closure) L1
fn ss_tidb_allow_tiflash_cop(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.allow_tiflash_cop = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1974 tiflash_fastscan SetSession closure) L1
fn ss_tiflash_fastscan(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.tiflash_fast_scan = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1978 tidb_mpp_store_fail_ttl SetSession closure) L1
fn ss_tidb_mpp_store_fail_ttl(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.mpp_store_fail_ttl = val.to_owned();
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1985 tidb_hash_exchange_with_new_collation SetSession closure) L1
fn ss_tidb_hash_exchange_with_new_collation(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.hash_exchange_with_new_collation = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1989 tidb_broadcast_join_threshold_count SetSession closure) L1
fn ss_tidb_broadcast_join_threshold_count(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.broadcast_join_threshold_count = tidb_opt_int64(val, DefBroadcastJoinThresholdCount);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1993 tidb_broadcast_join_threshold_size SetSession closure) L1
fn ss_tidb_broadcast_join_threshold_size(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.broadcast_join_threshold_size = tidb_opt_int64(val, DefBroadcastJoinThresholdSize);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1997 tidb_prefer_broadcast_join_by_exchange_data_size SetSession closure) L1
fn ss_tidb_prefer_broadcast_join_by_exchange_data_size(
    s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    s.prefer_bcj_by_exchange_data_size = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2003 tidb_opt_broadcast_cartesian_join SetSession closure) L1
fn ss_tidb_opt_broadcast_cartesian_join(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.allow_cartesian_bcj = tidb_opt_int(val, DefOptCartesianBCJ as isize) as i64;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2007 tidb_opt_mpp_outer_join_fixed_build_side SetSession closure) L1
fn ss_tidb_opt_mpp_outer_join_fixed_build_side(
    s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    s.mpp_outer_join_fixed_build_side = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2011 tidb_executor_concurrency SetSession closure) L1
fn ss_tidb_executor_concurrency(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.executor_concurrency = tidb_opt_positive_int32(val, DefExecutorConcurrency as isize) as i64;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2015 tidb_distsql_scan_concurrency SetSession closure) L1
fn ss_tidb_distsql_scan_concurrency(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.dist_sql_scan_concurrency =
        tidb_opt_positive_int32(val, DefDistSQLScanConcurrency as isize) as i64;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2019 tidb_analyze_distsql_scan_concurrency SetSession closure) L1
fn ss_tidb_analyze_distsql_scan_concurrency(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.analyze_dist_sql_scan_concurrency =
        tidb_opt_positive_int32(val, DefAnalyzeDistSQLScanConcurrency as isize) as i64;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2037 tidb_opt_limit_push_down_threshold SetSession closure) L1
fn ss_tidb_opt_limit_push_down_threshold(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.limit_push_down_threshold = tidb_opt_int64(val, DefOptLimitPushDownThreshold);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2041 tidb_opt_correlation_threshold SetSession closure) L1
fn ss_tidb_opt_correlation_threshold(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.correlation_threshold = tidb_opt_float64(val, DefOptCorrelationThreshold);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2045 tidb_opt_enable_correlation_adjustment SetSession closure) L1
fn ss_tidb_opt_enable_correlation_adjustment(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.enable_correlation_adjustment = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2049 tidb_opt_correlation_exp_factor SetSession closure) L1
fn ss_tidb_opt_correlation_exp_factor(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.correlation_exp_factor = tidb_opt_int64(val, DefOptCorrelationExpFactor);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2053 tidb_opt_cpu_factor SetSession closure) L1
fn ss_tidb_opt_cpu_factor(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.cpu_factor = tidb_opt_float64(val, DefOptCPUFactor);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2057 tidb_opt_tiflash_concurrency_factor SetSession closure) L1
fn ss_tidb_opt_tiflash_concurrency_factor(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.cop_tiflash_concurrency_factor = tidb_opt_float64(val, DefOptTiFlashConcurrencyFactor);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2061 tidb_opt_copcpu_factor SetSession closure) L1
fn ss_tidb_opt_copcpu_factor(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.cop_cpu_factor = tidb_opt_float64(val, DefOptCopCPUFactor);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2065 tidb_opt_network_factor SetSession closure) L1
fn ss_tidb_opt_network_factor(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.network_factor = tidb_opt_float64(val, DefOptNetworkFactor);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2069 tidb_opt_scan_factor SetSession closure) L1
fn ss_tidb_opt_scan_factor(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.scan_factor = tidb_opt_float64(val, DefOptScanFactor);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2073 tidb_opt_desc_factor SetSession closure) L1
fn ss_tidb_opt_desc_factor(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.desc_scan_factor = tidb_opt_float64(val, DefOptDescScanFactor);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2077 tidb_opt_seek_factor SetSession closure) L1
fn ss_tidb_opt_seek_factor(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.seek_factor = tidb_opt_float64(val, DefOptSeekFactor);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2081 tidb_opt_memory_factor SetSession closure) L1
fn ss_tidb_opt_memory_factor(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.memory_factor = tidb_opt_float64(val, DefOptMemoryFactor);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2085 tidb_opt_disk_factor SetSession closure) L1
fn ss_tidb_opt_disk_factor(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.disk_factor = tidb_opt_float64(val, DefOptDiskFactor);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2089 tidb_opt_index_scan_cost_factor SetSession closure) L1
fn ss_tidb_opt_index_scan_cost_factor(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.index_scan_cost_factor = tidb_opt_float64(val, DefOptIndexScanCostFactor);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2093 tidb_opt_index_reader_cost_factor SetSession closure) L1
fn ss_tidb_opt_index_reader_cost_factor(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.index_reader_cost_factor = tidb_opt_float64(val, DefOptIndexReaderCostFactor);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2097 tidb_opt_table_reader_cost_factor SetSession closure) L1
fn ss_tidb_opt_table_reader_cost_factor(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.table_reader_cost_factor = tidb_opt_float64(val, DefOptTableReaderCostFactor);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2101 tidb_opt_table_full_scan_cost_factor SetSession closure) L1
fn ss_tidb_opt_table_full_scan_cost_factor(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.table_full_scan_cost_factor = tidb_opt_float64(val, DefOptTableFullScanCostFactor);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2105 tidb_opt_table_range_scan_cost_factor SetSession closure) L1
fn ss_tidb_opt_table_range_scan_cost_factor(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.table_range_scan_cost_factor = tidb_opt_float64(val, DefOptTableRangeScanCostFactor);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2109 tidb_opt_table_rowid_scan_cost_factor SetSession closure) L1
fn ss_tidb_opt_table_rowid_scan_cost_factor(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.table_row_id_scan_cost_factor = tidb_opt_float64(val, DefOptTableRowIDScanCostFactor);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2113 tidb_opt_table_tiflash_scan_cost_factor SetSession closure) L1
fn ss_tidb_opt_table_tiflash_scan_cost_factor(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.table_tiflash_scan_cost_factor = tidb_opt_float64(val, DefOptTableTiFlashScanCostFactor);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2117 tidb_opt_index_lookup_cost_factor SetSession closure) L1
fn ss_tidb_opt_index_lookup_cost_factor(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.index_lookup_cost_factor = tidb_opt_float64(val, DefOptIndexLookupCostFactor);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2121 tidb_opt_index_merge_cost_factor SetSession closure) L1
fn ss_tidb_opt_index_merge_cost_factor(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.index_merge_cost_factor = tidb_opt_float64(val, DefOptIndexMergeCostFactor);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2125 tidb_opt_sort_cost_factor SetSession closure) L1
fn ss_tidb_opt_sort_cost_factor(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.sort_cost_factor = tidb_opt_float64(val, DefOptSortCostFactor);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2129 tidb_opt_topn_cost_factor SetSession closure) L1
fn ss_tidb_opt_topn_cost_factor(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.topn_cost_factor = tidb_opt_float64(val, DefOptTopNCostFactor);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2133 tidb_opt_limit_cost_factor SetSession closure) L1
fn ss_tidb_opt_limit_cost_factor(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.limit_cost_factor = tidb_opt_float64(val, DefOptLimitCostFactor);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2137 tidb_opt_stream_agg_cost_factor SetSession closure) L1
fn ss_tidb_opt_stream_agg_cost_factor(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.stream_agg_cost_factor = tidb_opt_float64(val, DefOptStreamAggCostFactor);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2141 tidb_opt_hash_agg_cost_factor SetSession closure) L1
fn ss_tidb_opt_hash_agg_cost_factor(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.hash_agg_cost_factor = tidb_opt_float64(val, DefOptHashAggCostFactor);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2145 tidb_opt_merge_join_cost_factor SetSession closure) L1
fn ss_tidb_opt_merge_join_cost_factor(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.merge_join_cost_factor = tidb_opt_float64(val, DefOptMergeJoinCostFactor);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2149 tidb_opt_hash_join_cost_factor SetSession closure) L1
fn ss_tidb_opt_hash_join_cost_factor(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.hash_join_cost_factor = tidb_opt_float64(val, DefOptHashJoinCostFactor);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2153 tidb_opt_index_join_cost_factor SetSession closure) L1
fn ss_tidb_opt_index_join_cost_factor(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.index_join_cost_factor = tidb_opt_float64(val, DefOptIndexJoinCostFactor);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2157 tidb_opt_index_join_max_scan_rows_ratio SetSession closure) L1
fn ss_tidb_opt_index_join_max_scan_rows_ratio(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.index_join_max_scan_rows_ratio = tidb_opt_float64(val, DefOptIndexJoinMaxScanRowsRatio);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2161 tidb_enable_new_only_full_group_by_check SetSession closure) L1
fn ss_tidb_enable_new_only_full_group_by_check(
    s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    s.optimizer_enable_new_only_full_group_by_check = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2165 tidb_opt_concurrency_factor SetSession closure) L1
fn ss_tidb_opt_concurrency_factor(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.concurrency_factor = tidb_opt_float64(val, DefOptConcurrencyFactor);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2169 tidb_opt_force_inline_cte SetSession closure) L1
fn ss_tidb_opt_force_inline_cte(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.enable_force_inline_cte = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2173 tidb_index_join_batch_size SetSession closure) L1
fn ss_tidb_index_join_batch_size(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.index_join_batch_size = tidb_opt_positive_int32(val, DefIndexJoinBatchSize as isize) as i64;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2177 tidb_index_lookup_size SetSession closure) L1
fn ss_tidb_index_lookup_size(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.index_lookup_size = tidb_opt_positive_int32(val, DefIndexLookupSize as isize) as i64;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2181 tidb_index_lookup_concurrency SetSession closure) L1
fn ss_tidb_index_lookup_concurrency(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.index_lookup_concurrency = tidb_opt_positive_int32(val, ConcurrencyUnset as isize) as i64;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2188 tidb_index_lookup_join_concurrency SetSession closure) L1
fn ss_tidb_index_lookup_join_concurrency(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.index_lookup_join_concurrency =
        tidb_opt_positive_int32(val, ConcurrencyUnset as isize) as i64;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2195 tidb_index_serial_scan_concurrency SetSession closure) L1
fn ss_tidb_index_serial_scan_concurrency(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.index_serial_scan_concurrency =
        tidb_opt_positive_int32(val, DefIndexSerialScanConcurrency as isize) as i64;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2199 tidb_skip_utf8_check SetSession closure) L1
fn ss_tidb_skip_utf8_check(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.skip_utf8_check = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2203 tidb_skip_ascii_check SetSession closure) L1
fn ss_tidb_skip_ascii_check(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.skip_ascii_check = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2211 tidb_max_chunk_size SetSession closure) L1
fn ss_tidb_max_chunk_size(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.max_chunk_size = tidb_opt_positive_int32(val, DefMaxChunkSize as isize) as i64;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2215 tidb_allow_batch_cop SetSession closure) L1
fn ss_tidb_allow_batch_cop(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.allow_batch_cop = tidb_opt_int64(val, DefTiDBAllowBatchCop);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2219 tidb_shard_row_id_bits SetSession closure) L1
fn ss_tidb_shard_row_id_bits(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.shard_row_id_bits = tidb_opt_uint64(val, DefShardRowIDBits as u64);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2223 tidb_pre_split_regions SetSession closure) L1
fn ss_tidb_pre_split_regions(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.pre_split_regions = tidb_opt_uint64(val, DefPreSplitRegions as u64);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2227 tidb_init_chunk_size SetSession closure) L1
fn ss_tidb_init_chunk_size(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.init_chunk_size = tidb_opt_positive_int32(val, DefInitChunkSize as isize) as i64;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2253 tidb_hash_join_concurrency SetSession closure) L1
fn ss_tidb_hash_join_concurrency(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.hash_join_concurrency = tidb_opt_positive_int32(val, ConcurrencyUnset as isize) as i64;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2260 tidb_projection_concurrency SetSession closure) L1
fn ss_tidb_projection_concurrency(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.projection_concurrency = tidb_opt_positive_int32(val, ConcurrencyUnset as isize) as i64;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2267 tidb_hashagg_partial_concurrency SetSession closure) L1
fn ss_tidb_hashagg_partial_concurrency(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.hash_agg_partial_concurrency = tidb_opt_positive_int32(val, ConcurrencyUnset as isize) as i64;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2274 tidb_hashagg_final_concurrency SetSession closure) L1
fn ss_tidb_hashagg_final_concurrency(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.hash_agg_final_concurrency = tidb_opt_positive_int32(val, ConcurrencyUnset as isize) as i64;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2281 tidb_window_concurrency SetSession closure) L1
fn ss_tidb_window_concurrency(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.window_concurrency = tidb_opt_positive_int32(val, ConcurrencyUnset as isize) as i64;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2288 tidb_merge_join_concurrency SetSession closure) L1
fn ss_tidb_merge_join_concurrency(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.merge_join_concurrency = tidb_opt_positive_int32(val, ConcurrencyUnset as isize) as i64;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2295 tidb_streamagg_concurrency SetSession closure) L1
fn ss_tidb_streamagg_concurrency(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.stream_agg_concurrency = tidb_opt_positive_int32(val, ConcurrencyUnset as isize) as i64;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2302 tidb_index_merge_intersection_concurrency SetSession closure) L1
fn ss_tidb_index_merge_intersection_concurrency(
    s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    s.index_merge_intersection_concurrency =
        tidb_opt_positive_int32(val, ConcurrencyUnset as isize) as i64;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2309 tidb_enable_parallel_apply SetSession closure) L1
fn ss_tidb_enable_parallel_apply(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.enable_parallel_apply = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2313 tidb_mem_quota_apply_cache SetSession closure) L1
fn ss_tidb_mem_quota_apply_cache(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.mem_quota_apply_cache = tidb_opt_int64(val, DefTiDBMemQuotaApplyCache);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2325 tidb_txn_entry_size_limit SetSession closure) L1
fn ss_tidb_txn_entry_size_limit(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.txn_entry_size_limit = tidb_opt_uint64(val, DefTiDBTxnEntrySizeLimit as u64);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2347 tidb_constraint_check_in_place SetSession closure) L1
fn ss_tidb_constraint_check_in_place(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.constraint_check_in_place = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2351 tidb_txn_mode SetSession closure) L1
fn ss_tidb_txn_mode(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.txn_mode = gostd::to_upper(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2355 tidb_enable_window_function SetSession closure) L1
fn ss_tidb_enable_window_function(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.enable_window_function = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2359 tidb_enable_pipelined_window_function SetSession closure) L1
fn ss_tidb_enable_pipelined_window_function(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.enable_pipelined_window_exec = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2363 tidb_opt_enable_no_decorrelate_in_select SetSession closure) L1
fn ss_tidb_opt_enable_no_decorrelate_in_select(
    s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    s.enable_no_decorrelate_in_select = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2367 tidb_opt_enable_alternative_logical_plans SetSession closure) L1
fn ss_tidb_opt_enable_alternative_logical_plans(
    s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    s.enable_alternative_logical_plans = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2371 tidb_enable_strict_double_type_check SetSession closure) L1
fn ss_tidb_enable_strict_double_type_check(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.enable_strict_double_type_check = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2375 tidb_enable_strict_not_null_check SetSession closure) L1
fn ss_tidb_enable_strict_not_null_check(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.enable_strict_not_null_check = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2379 tidb_enable_vectorized_expression SetSession closure) L1
fn ss_tidb_enable_vectorized_expression(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.enable_vectorized_expression = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2390 tidb_enable_fast_analyze SetSession closure) L1
fn ss_tidb_enable_fast_analyze(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.enable_fast_analyze = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2395 tidb_enable_rate_limit_action SetSession closure) L1
fn ss_tidb_enable_rate_limit_action(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.enabled_rate_limit_action = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2431 tidb_enable_auto_increment_in_generated SetSession closure) L1
fn ss_tidb_enable_auto_increment_in_generated(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.enable_auto_increment_in_generated = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2435 tidb_placement_mode SetSession closure) L1
fn ss_tidb_placement_mode(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.placement_mode = val.to_owned();
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2439 tidb_opt_join_reorder_threshold SetSession closure) L1
fn ss_tidb_opt_join_reorder_threshold(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.tidb_opt_join_reorder_threshold =
        tidb_opt_positive_int32(val, DefTiDBOptJoinReorderThreshold as isize) as i64;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2443 tidb_opt_enable_advanced_join_reorder SetSession closure) L1
fn ss_tidb_opt_enable_advanced_join_reorder(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.tidb_opt_enable_advanced_join_reorder = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2447 tidb_opt_join_reorder_through_proj SetSession closure) L1
fn ss_tidb_opt_join_reorder_through_proj(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.tidb_opt_join_reorder_through_proj = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2451 tidb_opt_join_reorder_through_sel SetSession closure) L1
fn ss_tidb_opt_join_reorder_through_sel(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.tidb_opt_join_reorder_through_sel = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2475 tidb_enable_noop_functions SetSession closure) L1
fn ss_tidb_enable_noop_functions(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.noop_funcs_mode = tidb_opt_on_off_warn(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2497 tidb_adaptive_closest_read_threshold SetSession closure) L1
fn ss_tidb_adaptive_closest_read_threshold(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.replica_closest_read_threshold = tidb_opt_int64(val, DefAdaptiveClosestReadThreshold);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2501 tidb_use_plan_baselines SetSession closure) L1
fn ss_tidb_use_plan_baselines(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.use_plan_baselines = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2510 tidb_evolve_plan_baselines SetSession closure) L1
fn ss_tidb_evolve_plan_baselines(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.evolve_plan_baselines = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2514 tidb_enable_extended_stats SetSession closure) L1
fn ss_tidb_enable_extended_stats(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.enable_extended_stats = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2518 cte_max_recursion_depth SetSession closure) L1
fn ss_cte_max_recursion_depth(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.cte_max_recursion_depth = tidb_opt_int(val, DefCTEMaxRecursionDepth as isize) as i64;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2580 tidb_enable_async_commit SetSession closure) L1
fn ss_tidb_enable_async_commit(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.enable_async_commit = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2584 tidb_enable_1pc SetSession closure) L1
fn ss_tidb_enable_1pc(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.enable_1pc = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2588 tidb_guarantee_linearizability SetSession closure) L1
fn ss_tidb_guarantee_linearizability(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.guarantee_linearizability = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2599 tidb_analyze_version SetSession closure) L1
fn ss_tidb_analyze_version(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.analyze_version = tidb_opt_positive_int32(val, DefTiDBAnalyzeVersion as isize) as i64;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2621 tidb_enable_index_merge_join SetSession closure) L1
fn ss_tidb_enable_index_merge_join(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.enable_index_merge_join = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2625 tidb_track_aggregate_memory_usage SetSession closure) L1
fn ss_tidb_track_aggregate_memory_usage(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.track_aggregate_memory_usage = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2629 tidb_multi_statement_mode SetSession closure) L1
fn ss_tidb_multi_statement_mode(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.multi_statement_mode = tidb_opt_on_off_warn(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2640 tidb_enable_exchange_partition SetSession closure) L1
fn ss_tidb_enable_exchange_partition(s: &mut SessionVars, _val: &str) -> Result<(), Error> {
    s.tidb_enable_exchange_partition = true;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2645 tidb_tmp_table_max_size SetSession closure) L1
fn ss_tidb_tmp_table_max_size(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.tmp_table_size = tidb_opt_int64(val, DefTiDBTmpTableMaxSize);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2649 tidb_enable_ordered_result_mode SetSession closure) L1
fn ss_tidb_enable_ordered_result_mode(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.enable_stable_result_mode = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2653 tidb_enable_pseudo_for_outdated_stats SetSession closure) L1
fn ss_tidb_enable_pseudo_for_outdated_stats(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.enable_pseudo_for_outdated_stats = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2657 tidb_regard_null_as_point SetSession closure) L1
fn ss_tidb_regard_null_as_point(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.regard_null_as_point = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2661 tidb_enable_paging SetSession closure) L1
fn ss_tidb_enable_paging(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.enable_paging = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2668 tidb_enable_legacy_instance_scope SetSession closure) L1
fn ss_tidb_enable_legacy_instance_scope(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.enable_legacy_instance_scope = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2702 tidb_enable_mutation_checker SetSession closure) L1
fn ss_tidb_enable_mutation_checker(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.enable_mutation_checker = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2749 tidb_rc_write_check_ts SetSession closure) L1
fn ss_tidb_rc_write_check_ts(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.rc_write_check_ts = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2753 tidb_remove_orderby_in_subquery SetSession closure) L1
fn ss_tidb_remove_orderby_in_subquery(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.remove_orderby_in_subquery = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2770 tidb_nontransactional_ignore_error SetSession closure) L1
fn ss_tidb_nontransactional_ignore_error(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.non_transactional_ignore_error = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2776 tiflash_fine_grained_shuffle_stream_count SetSession closure) L1
fn ss_tiflash_fine_grained_shuffle_stream_count(
    s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    s.tiflash_fine_grained_shuffle_stream_count =
        tidb_opt_int64(val, DefTiFlashFineGrainedShuffleStreamCount);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2781 tiflash_fine_grained_shuffle_batch_size SetSession closure) L1
fn ss_tiflash_fine_grained_shuffle_batch_size(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.tiflash_fine_grained_shuffle_batch_size =
        (tidb_opt_int64(val, DefTiFlashFineGrainedShuffleBatchSize)) as u64;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2790 tidb_min_paging_size SetSession closure) L1
fn ss_tidb_min_paging_size(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.min_paging_size = tidb_opt_positive_int32(val, DefMinPagingSize as isize) as i64;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2794 tidb_max_paging_size SetSession closure) L1
fn ss_tidb_max_paging_size(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.max_paging_size = tidb_opt_positive_int32(val, DefMaxPagingSize as isize) as i64;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2798 tidb_memory_debug_mode_min_heap_inuse SetSession closure) L1
fn ss_tidb_memory_debug_mode_min_heap_inuse(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.memory_debug_mode_min_heap_in_use = tidb_opt_int64(val, 0);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2802 tidb_memory_debug_mode_alarm_ratio SetSession closure) L1
fn ss_tidb_memory_debug_mode_alarm_ratio(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.memory_debug_mode_alarm_ratio = tidb_opt_int64(val, 0);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2806 sql_require_primary_key SetSession closure) L1
fn ss_sql_require_primary_key(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.primary_key_required = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2810 tidb_enable_analyze_snapshot SetSession closure) L1
fn ss_tidb_enable_analyze_snapshot(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.enable_analyze_snapshot = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2827 tidb_default_string_match_selectivity SetSession closure) L1
fn ss_tidb_default_string_match_selectivity(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.default_str_match_selectivity = tidb_opt_float64(val, DefTiDBDefaultStrMatchSelectivity);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2880 tidb_enable_tiflash_read_for_write_stmt SetSession closure) L1
fn ss_tidb_enable_tiflash_read_for_write_stmt(
    s: &mut SessionVars,
    _val: &str,
) -> Result<(), Error> {
    s.enable_tiflash_read_for_write_stmt = true;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2884 tidb_enable_unsafe_substitute SetSession closure) L1
fn ss_tidb_enable_unsafe_substitute(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.enable_unsafe_substitute = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2888 tidb_opt_range_max_size SetSession closure) L1
fn ss_tidb_opt_range_max_size(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.range_max_size = tidb_opt_int64(val, DefTiDBOptRangeMaxSize);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2892 tidb_opt_advanced_join_hint SetSession closure) L1
fn ss_tidb_opt_advanced_join_hint(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.enable_advanced_join_hint = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2896 tidb_opt_use_invisible_indexes SetSession closure) L1
fn ss_tidb_opt_use_invisible_indexes(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.optimizer_use_invisible_indexes = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2906 tidb_analyze_partition_concurrency SetSession closure) L1
fn ss_tidb_analyze_partition_concurrency(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.analyze_partition_concurrency = tidb_opt_int64(val, DefTiDBAnalyzePartitionConcurrency);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2913 tidb_merge_partition_stats_concurrency SetSession closure) L1
fn ss_tidb_merge_partition_stats_concurrency(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.analyze_partition_merge_concurrency =
        tidb_opt_int(val, DefTiDBMergePartitionStatsConcurrency as isize) as i64;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2920 tidb_enable_async_merge_global_stats SetSession closure) L1
fn ss_tidb_enable_async_merge_global_stats(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.enable_async_merge_global_stats = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2925 tidb_opt_prefix_index_single_scan SetSession closure) L1
fn ss_tidb_opt_prefix_index_single_scan(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.opt_prefix_index_single_scan = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2936 tidb_opt_partial_ordered_index_for_topn SetSession closure) L1
fn ss_tidb_opt_partial_ordered_index_for_topn(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.opt_partial_ordered_index_for_top_n = gostd::to_upper(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2953 tidb_enable_external_ts_read SetSession closure) L1
fn ss_tidb_enable_external_ts_read(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.enable_external_ts_read = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2958 tidb_enable_reuse_chunk SetSession closure) L1
fn ss_tidb_enable_reuse_chunk(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.enable_reuse_chunk = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3009 tidb_store_batch_size SetSession closure) L1
fn ss_tidb_store_batch_size(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.store_batch_size = tidb_opt_int(val, DefTiDBStoreBatchSize as isize) as i64;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3132 tidb_pessimistic_txn_fair_locking SetSession closure) L1
fn ss_tidb_pessimistic_txn_fair_locking(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.pessimistic_transaction_fair_locking = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3136 tidb_enable_plan_cache_for_param_limit SetSession closure) L1
fn ss_tidb_enable_plan_cache_for_param_limit(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.enable_plan_cache_for_param_limit = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3141 tidb_enable_inl_join_inner_multi_pattern SetSession closure) L1
fn ss_tidb_enable_inl_join_inner_multi_pattern(
    s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    s.enable_inl_join_inner_multi_pattern = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3154 tidb_enable_plan_cache_for_subquery SetSession closure) L1
fn ss_tidb_enable_plan_cache_for_subquery(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.enable_plan_cache_for_subquery = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3158 tidb_opt_enable_late_materialization SetSession closure) L1
fn ss_tidb_opt_enable_late_materialization(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.enable_late_materialization = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3181 tidb_opt_ordering_index_selectivity_threshold SetSession closure) L1
fn ss_tidb_opt_ordering_index_selectivity_threshold(
    s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    s.opt_ordering_idx_sel_thresh = tidb_opt_float64(val, DefTiDBOptOrderingIdxSelThresh);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3186 tidb_opt_ordering_index_selectivity_ratio SetSession closure) L1
fn ss_tidb_opt_ordering_index_selectivity_ratio(
    s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    s.opt_ordering_idx_sel_ratio = tidb_opt_float64(val, DefTiDBOptOrderingIdxSelRatio as f64);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3190 tidb_opt_enable_mpp_shared_cte_execution SetSession closure) L1
fn ss_tidb_opt_enable_mpp_shared_cte_execution(
    s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    s.enable_mpp_shared_cte_execution = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3231 tidb_plan_cache_invalidation_on_fresh_stats SetSession closure) L1
fn ss_tidb_plan_cache_invalidation_on_fresh_stats(
    s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    s.plan_cache_invalidation_on_fresh_stats = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3244 tidb_enable_fast_table_check SetSession closure) L1
fn ss_tidb_enable_fast_table_check(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.fast_check_table = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3248 tidb_skip_missing_partition_stats SetSession closure) L1
fn ss_tidb_skip_missing_partition_stats(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.skip_missing_partition_stats = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3556 tidb_idle_transaction_timeout SetSession closure) L1
fn ss_tidb_idle_transaction_timeout(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.idle_transaction_timeout =
        tidb_opt_positive_int32(val, DefTiDBIdleTransactionTimeout as isize) as i64;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3561 div_precision_increment SetSession closure) L1
fn ss_div_precision_increment(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.div_precision_increment =
        tidb_opt_positive_int32(val, DefDivPrecisionIncrement as isize) as i64;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3589 tiflash_hashagg_preaggregation_mode SetSession closure) L1
fn ss_tiflash_hashagg_preaggregation_mode(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.tiflash_pre_agg_mode = val.to_owned();
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3594 tidb_enable_lazy_cursor_fetch SetSession closure) L1
fn ss_tidb_enable_lazy_cursor_fetch(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.enable_lazy_cursor_fetch = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3728 tidb_enable_cache_prepare_stmt SetSession closure) L1
fn ss_tidb_enable_cache_prepare_stmt(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.enable_cache_prepare_stmt = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L253 tidb_batch_insert SetSession closure) L2 (writes `SessionVars.BatchInsert`, a write-path field: X in the D05.2 scoping, so only systems[] changes)
fn ss_tidb_batch_insert(_s: &mut SessionVars, _val: &str) -> Result<(), Error> {
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L257 tidb_batch_delete SetSession closure) L2 (writes `SessionVars.BatchDelete`, a write-path field: X in the D05.2 scoping, so only systems[] changes)
fn ss_tidb_batch_delete(_s: &mut SessionVars, _val: &str) -> Result<(), Error> {
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L261 tidb_batch_commit SetSession closure) L2 (writes `SessionVars.BatchCommit`, a write-path field: X in the D05.2 scoping, so only systems[] changes)
fn ss_tidb_batch_commit(_s: &mut SessionVars, _val: &str) -> Result<(), Error> {
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L334 tidb_allow_remove_auto_inc SetSession closure) L2 (writes `SessionVars.AllowRemoveAutoInc`, a write-path field: X in the D05.2 scoping, so only systems[] changes)
fn ss_tidb_allow_remove_auto_inc(_s: &mut SessionVars, _val: &str) -> Result<(), Error> {
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2207 tidb_dml_batch_size SetSession closure) L2 (writes `SessionVars.DMLBatchSize`, a write-path field: X in the D05.2 scoping, so only systems[] changes)
fn ss_tidb_dml_batch_size(_s: &mut SessionVars, _val: &str) -> Result<(), Error> {
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2332 tidb_retry_limit SetSession closure) L2 (writes `SessionVars.RetryLimit`, a write-path field: X in the D05.2 scoping, so only systems[] changes)
fn ss_tidb_retry_limit(_s: &mut SessionVars, _val: &str) -> Result<(), Error> {
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2343 tidb_disable_txn_auto_retry SetSession closure) L2 (writes `SessionVars.DisableTxnAutoRetry`, a write-path field: X in the D05.2 scoping, so only systems[] changes)
fn ss_tidb_disable_txn_auto_retry(_s: &mut SessionVars, _val: &str) -> Result<(), Error> {
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2522 allow_auto_random_explicit_insert SetSession closure) L2 (writes `SessionVars.AllowAutoRandExplicitInsert`, a write-path field: X in the D05.2 scoping, so only systems[] changes)
fn ss_allow_auto_random_explicit_insert(_s: &mut SessionVars, _val: &str) -> Result<(), Error> {
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2576 tidb_shard_allocate_step SetSession closure) L2 (writes `SessionVars.ShardAllocateStep`, a write-path field: X in the D05.2 scoping, so only systems[] changes)
fn ss_tidb_shard_allocate_step(_s: &mut SessionVars, _val: &str) -> Result<(), Error> {
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L314 tidb_ddl_reorg_priority SetSession closure) L2 (writes `SessionVars.DDLReorgPriority`, a write-path field: X in the D05.2 scoping, so only systems[] changes)
fn ss_tidb_ddl_reorg_priority(_s: &mut SessionVars, _val: &str) -> Result<(), Error> {
    Ok(())
}

// ---------------------------------------------------------------- hand-ported SetSession hooks

/// `strconv.ParseUint(s, 10, 64)` with Go's `*strconv.NumError` text.
// go-std: strconv.ParseUint L2 (base 10, bitSize 64 only)
fn go_parse_uint(s: &str) -> Result<u64, Error> {
    let fail = |reason: &str| {
        let quoted = tidb_base::gofmt::sprintf(
            "%q",
            &[tidb_base::gofmt::Arg::go_string(s.as_bytes().to_vec())],
        );
        let mut m = b"strconv.ParseUint: parsing ".to_vec();
        m.extend_from_slice(&quoted);
        m.extend_from_slice(b": ");
        m.extend_from_slice(reason.as_bytes());
        tidb_base::errors::new(m)
    };
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(fail("invalid syntax"));
    }
    s.parse::<u64>().map_err(|_| fail("value out of range"))
}

// go-part: defaultSysVars (sysvar.go:L413 tidb_read_consistency SetSession closure) L1
fn ss_tidb_read_consistency(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.read_consistency = val.to_owned();
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1360 tidb_prepared_plan_cache_size SetSession closure) L1
fn ss_tidb_prepared_plan_cache_size(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    let u_val = go_parse_uint(val);
    if let Ok(u) = u_val {
        s.prepared_plan_cache_size = u;
    }
    u_val.map(|_| ())
}

// go-part: defaultSysVars (sysvar.go:L1401 tidb_non_prepared_plan_cache_size SetSession closure) L1
fn ss_tidb_non_prepared_plan_cache_size(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    let u_val = go_parse_uint(val);
    if let Ok(u) = u_val {
        s.non_prepared_plan_cache_size = u;
    }
    u_val.map(|_| ())
}

// go-part: defaultSysVars (sysvar.go:L1411 tidb_plan_cache_max_plan_size SetSession closure) L1
fn ss_tidb_plan_cache_max_plan_size(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    let u_val = go_parse_uint(val);
    if let Ok(u) = u_val {
        s.plan_cache_max_plan_size = u;
    }
    u_val.map(|_| ())
}

// go-part: defaultSysVars (sysvar.go:L1418 tidb_session_plan_cache_size SetSession closure) L1
fn ss_tidb_session_plan_cache_size(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    let u_val = go_parse_uint(val);
    if let Ok(u) = u_val {
        s.session_plan_cache_size = u;
    }
    u_val.map(|_| ())
}

// go-part: defaultSysVars (sysvar.go:L1616 tidb_enable_plan_replayer_continuous_capture SetSession closure) L1
fn ss_tidb_enable_plan_replayer_continuous_capture(
    s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    let historical_stats_enabled = s
        .global_vars_accessor
        .get_global_sys_var(super::names::TiDBEnableHistoricalStats)?;
    if !tidb_opt_on(&historical_stats_enabled) && tidb_opt_on(val) {
        return Err(tidb_base::errors::new(format!(
            "{} should be enabled before enabling {}",
            super::names::TiDBEnableHistoricalStats,
            super::names::TiDBEnablePlanReplayerContinuousCapture
        )));
    }
    s.enable_plan_replayed_continues_capture = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1671 sql_select_limit SetSession closure) L1
fn ss_sql_select_limit(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.select_limit = go_parse_uint(val)?;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1746 collation_server SetSession closure) L1
fn ss_collation_server(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    if let Ok(coll) = tidb_types::collate::get_collation_by_name(val) {
        s.systems.insert(
            super::names::CharacterSetServer.to_owned(),
            coll.charset_name.clone(),
        );
    }
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1806 collation_database SetSession closure) L1
fn ss_collation_database(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    if let Ok(coll) = tidb_types::collate::get_collation_by_name(val) {
        s.systems.insert(
            super::names::CharsetDatabase.to_owned(),
            coll.charset_name.clone(),
        );
    }
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1842 collation_connection SetSession closure) L1
fn ss_collation_connection(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    if let Ok(coll) = tidb_types::collate::get_collation_by_name(val) {
        s.systems.insert(
            super::names::CharacterSetConnection.to_owned(),
            coll.charset_name.clone(),
        );
    }
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1859 character_set_database SetSession closure) L1
fn ss_character_set_database(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    if let (Some(cs), Ok(())) = tidb_parser::gocharset::get_charset_info(val) {
        s.systems.insert(
            super::names::CollationDatabase.to_owned(),
            cs.default_collation(),
        );
    }
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1867 innodb_lock_wait_timeout SetSession closure) L1
fn ss_innodb_lock_wait_timeout(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    let lock_wait_sec = tidb_opt_int64(val, DefInnodbLockWaitTimeout);
    s.lock_wait_timeout = lock_wait_sec.wrapping_mul(1000);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1910 character_set_connection SetSession closure) L1
fn ss_character_set_connection(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    if let (Some(cs), Ok(())) = tidb_parser::gocharset::get_charset_info(val) {
        s.systems.insert(
            super::names::CollationConnection.to_owned(),
            cs.default_collation(),
        );
    }
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1918 character_set_server SetSession closure) L1
fn ss_character_set_server(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    if let (Some(cs), Ok(())) = tidb_parser::gocharset::get_charset_info(val) {
        s.systems.insert(
            super::names::CollationServer.to_owned(),
            cs.default_collation(),
        );
    }
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1946 max_allowed_packet SetSession closure) L1
fn ss_max_allowed_packet(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    // Go assigns the result of ParseUint (0 on a syntax error, the max on overflow)
    // before checking the error.
    match go_parse_uint(val) {
        Ok(u) => {
            s.max_allowed_packet = u;
            Ok(())
        }
        Err(e) => {
            s.max_allowed_packet = if val.bytes().all(|b| b.is_ascii_digit()) && !val.is_empty() {
                u64::MAX
            } else {
                0
            };
            Err(e)
        }
    }
}

// go-part: defaultSysVars (sysvar.go:L2023 tidb_opt_insubq_to_join_and_agg SetSession closure) L1
fn ss_tidb_opt_insubq_to_join_and_agg(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    // SetAllowInSubqToJoinAndAgg
    s.allow_in_subq_to_join_and_agg = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2033 tidb_opt_prefer_range_scan SetSession closure) L1
fn ss_tidb_opt_prefer_range_scan(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    // SetAllowPreferRangeScan
    s.prefer_range_scan = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2231 tidb_enable_cascades_planner SetSession closure) L1
fn ss_tidb_enable_cascades_planner(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    // SetEnableCascadesPlanner
    s.enable_cascades_planner = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2235 tidb_enable_index_merge SetSession closure) L1
fn ss_tidb_enable_index_merge(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    // SetEnableIndexMerge
    s.enable_index_merge = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2531 tidb_enable_clustered_index SetSession closure) L1
fn ss_tidb_enable_clustered_index(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.enable_clustered_index = tidb_opt_enable_clustered(val).0;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2612 tidb_hash_join_version SetSession closure) L1
fn ss_tidb_hash_join_version(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    // joinversion.IsOptimizedVersion: strings.ToLower(v) == HashJoinVersionOptimized
    s.use_hash_join_v2 = gostd::to_lower(val) == "optimized";
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2617 tidb_opt_enable_hash_join SetSession closure) L1
fn ss_tidb_opt_enable_hash_join(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.disable_hash_join = !tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2707 tidb_txn_assertion_level SetSession closure) L1
fn ss_tidb_txn_assertion_level(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.assertion_level = tidb_opt_assertion_level(val).0;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2711 tidb_batch_pending_tiflash_count SetSession closure) L1
fn ss_tidb_batch_pending_tiflash_count(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    // strconv.Atoi: optional sign, decimal digits, fits int.
    let b = val
        .parse::<i64>()
        .unwrap_or(DefTiDBBatchPendingTiFlashCount);
    s.batch_pending_tiflash_count = b;
    Ok(())
}

// ---------------------------------------------------------------- hand-ported GetSession / GetStateValue hooks

// go-part: defaultSysVars (sysvar.go:L112 warning_count GetSession closure) L1
fn gs_warning_count(s: &mut SessionVars) -> Result<String, Error> {
    Ok(s.sys_warning_count.to_string())
}

// go-part: defaultSysVars (sysvar.go:L115 error_count GetSession closure) L1
fn gs_error_count(s: &mut SessionVars) -> Result<String, Error> {
    Ok(s.sys_error_count.to_string())
}

// go-part: defaultSysVars (sysvar.go:L268 tidb_last_txn_info GetSession closure) L1
fn gs_tidb_last_txn_info(s: &mut SessionVars) -> Result<String, Error> {
    Ok(s.last_txn_info.clone())
}

// go-part: defaultSysVars (sysvar.go:L387 last_plan_from_cache GetSession closure) L1
fn gs_last_plan_from_cache(s: &mut SessionVars) -> Result<String, Error> {
    Ok(bool_to_on_off(s.prev_found_in_plan_cache).to_owned())
}

// go-part: defaultSysVars (sysvar.go:L390 last_plan_from_binding GetSession closure) L1
fn gs_last_plan_from_binding(s: &mut SessionVars) -> Result<String, Error> {
    Ok(bool_to_on_off(s.prev_found_in_binding).to_owned())
}

// go-part: defaultSysVars (sysvar.go:L396 rand_seed1 GetSession closure) L1
fn gs_rand_seed1(_s: &mut SessionVars) -> Result<String, Error> {
    Ok("0".to_owned())
}

// go-part: defaultSysVars (sysvar.go:L404 rand_seed2 GetSession closure) L1
fn gs_rand_seed2(_s: &mut SessionVars) -> Result<String, Error> {
    Ok("0".to_owned())
}

// go-part: defaultSysVars (sysvar.go:L426 tidb_last_plan_replayer_token GetSession closure) L1
fn gs_tidb_last_plan_replayer_token(s: &mut SessionVars) -> Result<String, Error> {
    Ok(s.last_plan_replayer_token.clone())
}

// go-part: defaultSysVars (sysvar.go:L430 last_sql_use_alloc GetSession closure) L1
fn gs_last_sql_use_alloc(s: &mut SessionVars) -> Result<String, Error> {
    Ok(bool_to_on_off(s.pre_use_chunk_alloc).to_owned())
}

// go-part: defaultSysVars (sysvar.go:L433 tidb_request_source_type GetSession closure) L1
fn gs_tidb_request_source_type(s: &mut SessionVars) -> Result<String, Error> {
    Ok(s.explicit_request_source_type.clone())
}

// go-part: defaultSysVars (sysvar.go:L1943 max_allowed_packet GetSession closure) L1
fn gs_max_allowed_packet(s: &mut SessionVars) -> Result<String, Error> {
    Ok(s.max_allowed_packet.to_string())
}

// go-part: defaultSysVars (sysvar.go:L3145 tidb_enable_inl_join_inner_multi_pattern GetSession closure) L1
fn gs_tidb_enable_inl_join_inner_multi_pattern(s: &mut SessionVars) -> Result<String, Error> {
    Ok(bool_to_on_off(s.enable_inl_join_inner_multi_pattern).to_owned())
}

// go-part: defaultSysVars (sysvar.go:L102 timestamp GetStateValue closure) L1
fn gv_timestamp(s: &mut SessionVars) -> Result<(String, bool), Error> {
    match s.systems.get(super::names::Timestamp) {
        Some(timestamp) => Ok((timestamp.clone(), timestamp != DefTimestamp)),
        None => Ok((String::new(), false)),
    }
}

// go-part: defaultSysVars (sysvar.go:L120 last_insert_id GetStateValue closure) L1
fn gv_last_insert_id(_s: &mut SessionVars) -> Result<(String, bool), Error> {
    Ok((String::new(), false))
}

// go-part: defaultSysVars (sysvar.go:L125 identity GetStateValue closure) L1
fn gv_identity(_s: &mut SessionVars) -> Result<(String, bool), Error> {
    Ok((String::new(), false))
}
