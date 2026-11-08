//! go-file: pkg/sessionctx/variable/tidb_vars.go (variable names used by D05.1)
//!
//! The variable-name constants the D05.1 code refers to, with Go's identifiers (M-GONAME).
//! The values are cross-checked by the registry: every name here is a registered variable.

#![allow(non_upper_case_globals)]

/// Go constant `TiDBGCEnable`.
pub const TiDBGCEnable: &str = "tidb_gc_enable";
/// Go constant `TiDBGCRunInterval`.
pub const TiDBGCRunInterval: &str = "tidb_gc_run_interval";
/// Go constant `TiDBGCLifetime`.
pub const TiDBGCLifetime: &str = "tidb_gc_life_time";
/// Go constant `TiDBGCConcurrency`.
pub const TiDBGCConcurrency: &str = "tidb_gc_concurrency";
/// Go constant `TiDBGCScanLockMode`.
pub const TiDBGCScanLockMode: &str = "tidb_gc_scan_lock_mode";
/// Go constant `TiDBExternalTS`.
pub const TiDBExternalTS: &str = "tidb_external_ts";
/// Go constant `WarningCount`.
pub const WarningCount: &str = "warning_count";
/// Go constant `ErrorCount`.
pub const ErrorCount: &str = "error_count";
/// Go constant `TiDBEnableNoopFuncs`.
pub const TiDBEnableNoopFuncs: &str = "tidb_enable_noop_functions";
/// Go constant `TiDBSkipIsolationLevelCheck`.
pub const TiDBSkipIsolationLevelCheck: &str = "tidb_skip_isolation_level_check";
/// Go constant `TiDBServerMemoryLimit`.
pub const TiDBServerMemoryLimit: &str = "tidb_server_memory_limit";
/// Go constant `ValidatePasswordLength`.
pub const ValidatePasswordLength: &str = "validate_password.length";
/// Go constant `TiDBAnalyzeSkipColumnTypes`.
pub const TiDBAnalyzeSkipColumnTypes: &str = "tidb_analyze_skip_column_types";
/// Go constant `TiDBSchemaCacheSize`.
pub const TiDBSchemaCacheSize: &str = "tidb_schema_cache_size";
/// Go constant `Timestamp`.
pub const Timestamp: &str = "timestamp";
/// Go constant `TiDBEnableTiFlashPipelineMode`.
pub const TiDBEnableTiFlashPipelineMode: &str = "tidb_enable_tiflash_pipeline_model";
/// Go constant `TiDBIsolationReadEngines`.
pub const TiDBIsolationReadEngines: &str = "tidb_isolation_read_engines";
/// Go constant `TiDBReadConsistency`.
pub const TiDBReadConsistency: &str = "tidb_read_consistency";
/// Go constant `InitConnect`.
pub const InitConnect: &str = "init_connect";
/// Go constant `TiDBAutoAnalyzeRatio`.
pub const TiDBAutoAnalyzeRatio: &str = "tidb_auto_analyze_ratio";
/// Go constant `TiDBSuperReadOnly`.
pub const TiDBSuperReadOnly: &str = "tidb_super_read_only";
/// Go constant `TiDBRestrictedReadOnly`.
pub const TiDBRestrictedReadOnly: &str = "tidb_restricted_read_only";
/// Go constant `TiDBAutoAnalyzePartitionBatchSize`.
pub const TiDBAutoAnalyzePartitionBatchSize: &str = "tidb_auto_analyze_partition_batch_size";
/// Go constant `TiDBAnalyzeColumnOptions`.
pub const TiDBAnalyzeColumnOptions: &str = "tidb_analyze_column_options";
/// Go constant `TiDBGOGCTunerThreshold`.
pub const TiDBGOGCTunerThreshold: &str = "tidb_gogc_tuner_threshold";
/// Go constant `TiDBServerMemoryLimitSessMinSize`.
pub const TiDBServerMemoryLimitSessMinSize: &str = "tidb_server_memory_limit_sess_min_size";
/// Go constant `TiDBServerMemoryLimitGCTrigger`.
pub const TiDBServerMemoryLimitGCTrigger: &str = "tidb_server_memory_limit_gc_trigger";
/// Go constant `TiDBPrepPlanCacheSize`.
pub const TiDBPrepPlanCacheSize: &str = "tidb_prepared_plan_cache_size";
/// Go constant `TiDBSessionPlanCacheSize`.
pub const TiDBSessionPlanCacheSize: &str = "tidb_session_plan_cache_size";
/// Go constant `TiDBNonPreparedPlanCacheSize`.
pub const TiDBNonPreparedPlanCacheSize: &str = "tidb_non_prepared_plan_cache_size";
/// Go constant `TiDBAutoAnalyzeConcurrency`.
pub const TiDBAutoAnalyzeConcurrency: &str = "tidb_auto_analyze_concurrency";
/// Go constant `TiDBEnableHistoricalStats`.
pub const TiDBEnableHistoricalStats: &str = "tidb_enable_historical_stats";
/// Go constant `TiDBEnablePlanReplayerContinuousCapture`.
pub const TiDBEnablePlanReplayerContinuousCapture: &str =
    "tidb_enable_plan_replayer_continuous_capture";
/// Go constant `DefaultCollationForUTF8MB4`.
pub const DefaultCollationForUTF8MB4: &str = "default_collation_for_utf8mb4";
/// Go constant `ForeignKeyChecks`.
pub const ForeignKeyChecks: &str = "foreign_key_checks";
/// Go constant `CharacterSetClient`.
pub const CharacterSetClient: &str = "character_set_client";
/// Go constant `CharsetDatabase`.
pub const CharsetDatabase: &str = "character_set_database";
/// Go constant `GroupConcatMaxLen`.
pub const GroupConcatMaxLen: &str = "group_concat_max_len";
/// Go constant `CharacterSetConnection`.
pub const CharacterSetConnection: &str = "character_set_connection";
/// Go constant `CharacterSetServer`.
pub const CharacterSetServer: &str = "character_set_server";
/// Go constant `MaxAllowedPacket`.
pub const MaxAllowedPacket: &str = "max_allowed_packet";
/// Go constant `TiDBIndexLookupConcurrency`.
pub const TiDBIndexLookupConcurrency: &str = "tidb_index_lookup_concurrency";
/// Go constant `TiDBExecutorConcurrency`.
pub const TiDBExecutorConcurrency: &str = "tidb_executor_concurrency";
/// Go constant `TiDBIndexLookupJoinConcurrency`.
pub const TiDBIndexLookupJoinConcurrency: &str = "tidb_index_lookup_join_concurrency";
/// Go constant `TiDBEnableListTablePartition`.
pub const TiDBEnableListTablePartition: &str = "tidb_enable_list_partition";
/// Go constant `TiDBHashJoinConcurrency`.
pub const TiDBHashJoinConcurrency: &str = "tidb_hash_join_concurrency";
/// Go constant `TiDBProjectionConcurrency`.
pub const TiDBProjectionConcurrency: &str = "tidb_projection_concurrency";
/// Go constant `TiDBHashAggPartialConcurrency`.
pub const TiDBHashAggPartialConcurrency: &str = "tidb_hashagg_partial_concurrency";
/// Go constant `TiDBHashAggFinalConcurrency`.
pub const TiDBHashAggFinalConcurrency: &str = "tidb_hashagg_final_concurrency";
/// Go constant `TiDBWindowConcurrency`.
pub const TiDBWindowConcurrency: &str = "tidb_window_concurrency";
/// Go constant `TiDBMergeJoinConcurrency`.
pub const TiDBMergeJoinConcurrency: &str = "tidb_merge_join_concurrency";
/// Go constant `TiDBStreamAggConcurrency`.
pub const TiDBStreamAggConcurrency: &str = "tidb_streamagg_concurrency";
/// Go constant `TiDBIndexMergeIntersectionConcurrency`.
pub const TiDBIndexMergeIntersectionConcurrency: &str = "tidb_index_merge_intersection_concurrency";
/// Go constant `TiDBAllowFallbackToTiKV`.
pub const TiDBAllowFallbackToTiKV: &str = "tidb_allow_fallback_to_tikv";
/// Go constant `TxReadOnly`.
pub const TxReadOnly: &str = "tx_read_only";
/// Go constant `TransactionReadOnly`.
pub const TransactionReadOnly: &str = "transaction_read_only";
/// Go constant `OfflineMode`.
pub const OfflineMode: &str = "offline_mode";
/// Go constant `SuperReadOnly`.
pub const SuperReadOnly: &str = "super_read_only";
/// Go constant `ReadOnly`.
pub const ReadOnly: &str = "read_only";
/// Go constant `SQLAutoIsNull`.
pub const SQLAutoIsNull: &str = "sql_auto_is_null";
/// Go constant `TiDBPartitionPruneMode`.
pub const TiDBPartitionPruneMode: &str = "tidb_partition_prune_mode";
/// Go constant `TiDBHashJoinVersion`.
pub const TiDBHashJoinVersion: &str = "tidb_hash_join_version";
/// Go constant `TiDBMemQuotaQuery`.
pub const TiDBMemQuotaQuery: &str = "tidb_mem_quota_query";
/// Go constant `TiDBOptPartialOrderedIndexForTopN`.
pub const TiDBOptPartialOrderedIndexForTopN: &str = "tidb_opt_partial_ordered_index_for_topn";
/// Go constant `MppExchangeCompressionMode`.
pub const MppExchangeCompressionMode: &str = "mpp_exchange_compression_mode";
/// Go constant `MppVersion`.
pub const MppVersion: &str = "mpp_version";
/// Go constant `TiDBRuntimeFilterTypeName`.
pub const TiDBRuntimeFilterTypeName: &str = "tidb_runtime_filter_type";
/// Go constant `TiDBRuntimeFilterModeName`.
pub const TiDBRuntimeFilterModeName: &str = "tidb_runtime_filter_mode";
/// Go constant `TiDBSessionAlias`.
pub const TiDBSessionAlias: &str = "tidb_session_alias";
/// Go constant `TiFlashHashAggPreAggMode`.
pub const TiFlashHashAggPreAggMode: &str = "tiflash_hashagg_preaggregation_mode";
/// Go constant `CharacterSetFilesystem`.
pub const CharacterSetFilesystem: &str = "character_set_filesystem";
/// Go constant `SecureAuth`.
pub const SecureAuth: &str = "secure_auth";
/// Go constant `TiDBAllowMPPExecution`.
pub const TiDBAllowMPPExecution: &str = "tidb_allow_mpp";
/// Go constant `ValidatePasswordMixedCaseCount`.
pub const ValidatePasswordMixedCaseCount: &str = "validate_password.mixed_case_count";
/// Go constant `ValidatePasswordNumberCount`.
pub const ValidatePasswordNumberCount: &str = "validate_password.number_count";
/// Go constant `ValidatePasswordSpecialCharCount`.
pub const ValidatePasswordSpecialCharCount: &str = "validate_password.special_char_count";
/// Go constant `TiDBEnableTablePartition`.
pub const TiDBEnableTablePartition: &str = "tidb_enable_table_partition";
/// Go constant `TiDBEnableGlobalIndex`.
pub const TiDBEnableGlobalIndex: &str = "tidb_enable_global_index";
/// Go constant `TiDBEnableExchangePartition`.
pub const TiDBEnableExchangePartition: &str = "tidb_enable_exchange_partition";
/// Go constant `TiDBEnableNewCostInterface`.
pub const TiDBEnableNewCostInterface: &str = "tidb_enable_new_cost_interface";
/// Go constant `TiDBEnableTiFlashReadForWriteStmt`.
pub const TiDBEnableTiFlashReadForWriteStmt: &str = "tidb_enable_tiflash_read_for_write_stmt";
/// Go constant `TiDBDisableTxnAutoRetry`.
pub const TiDBDisableTxnAutoRetry: &str = "tidb_disable_txn_auto_retry";
/// Go constant `TiDBEnableFastAnalyze`.
pub const TiDBEnableFastAnalyze: &str = "tidb_enable_fast_analyze";
/// Go constant `TiDBEvolvePlanBaselines`.
pub const TiDBEvolvePlanBaselines: &str = "tidb_evolve_plan_baselines";
/// Go constant `TiDBEnableClusteredIndex`.
pub const TiDBEnableClusteredIndex: &str = "tidb_enable_clustered_index";
/// Go constant `TiDBAnalyzeVersion`.
pub const TiDBAnalyzeVersion: &str = "tidb_analyze_version";
/// Go constant `TiDBMPPStoreFailTTL`.
pub const TiDBMPPStoreFailTTL: &str = "tidb_mpp_store_fail_ttl";
/// Go constant `TiDBEnforceMPPExecution`.
pub const TiDBEnforceMPPExecution: &str = "tidb_enforce_mpp";
/// Go constant `TiFlashQuerySpillRatio`.
pub const TiFlashQuerySpillRatio: &str = "tiflash_query_spill_ratio";
/// Go constant `TiDBScatterRegion`.
pub const TiDBScatterRegion: &str = "tidb_scatter_region";
/// Go constant `TiDBGOGCTunerMaxValue`.
pub const TiDBGOGCTunerMaxValue: &str = "tidb_gogc_tuner_max_value";
/// Go constant `TiDBGOGCTunerMinValue`.
pub const TiDBGOGCTunerMinValue: &str = "tidb_gogc_tuner_min_value";
/// Go constant `TiDBEnableAutoAnalyzePriorityQueue`.
pub const TiDBEnableAutoAnalyzePriorityQueue: &str = "tidb_enable_auto_analyze_priority_queue";
/// Go constant `TiDBEnableColumnTracking`.
pub const TiDBEnableColumnTracking: &str = "tidb_enable_column_tracking";
/// Go constant `RequireSecureTransport`.
pub const RequireSecureTransport: &str = "require_secure_transport";
/// Go constant `TiDBMaxAutoAnalyzeTime`.
pub const TiDBMaxAutoAnalyzeTime: &str = "tidb_max_auto_analyze_time";
/// Go constant `SQLModeVar`.
pub const SQLModeVar: &str = "sql_mode";
/// Go constant `TimeZone`.
pub const TimeZone: &str = "time_zone";
/// Go constant `TiDBServiceScope`.
pub const TiDBServiceScope: &str = "tidb_service_scope";
/// Go constant `TiDBMaxDistTaskNodes`.
pub const TiDBMaxDistTaskNodes: &str = "tidb_max_dist_task_nodes";
/// Go constant `TiDBTxnReadTS`.
pub const TiDBTxnReadTS: &str = "tx_read_ts";
/// Go constant `TxnIsolation`.
pub const TxnIsolation: &str = "tx_isolation";
/// Go constant `TransactionIsolation`.
pub const TransactionIsolation: &str = "transaction_isolation";
/// Go constant `TxnIsolationOneShot`.
pub const TxnIsolationOneShot: &str = "tx_isolation_one_shot";
/// Go constant `CollationServer`.
pub const CollationServer: &str = "collation_server";
/// Go constant `CollationDatabase`.
pub const CollationDatabase: &str = "collation_database";
/// Go constant `CollationConnection`.
pub const CollationConnection: &str = "collation_connection";
/// Go constant `CharacterSetResults`.
pub const CharacterSetResults: &str = "character_set_results";
/// Go constant `TiDBEnableLocalTxn`.
pub const TiDBEnableLocalTxn: &str = "tidb_enable_local_txn";
/// Go constant `TiDBEnableAutoAnalyze`.
pub const TiDBEnableAutoAnalyze: &str = "tidb_enable_auto_analyze";
