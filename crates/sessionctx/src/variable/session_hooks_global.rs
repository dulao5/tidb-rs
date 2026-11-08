//! go-file: pkg/sessionctx/variable/sysvar.go
//!
//! The SetGlobal / GetGlobal hooks whose only effect is a process-wide atomic of the variable
//! package (D05.2 T5). Translated by `scripts/d052/gen_global_hooks.py`-style pattern matching
//! of the Go closure bodies (one Go statement each); maintained by hand from here on.
//! Registered in `session_hooks.rs`.

#![allow(non_upper_case_globals)]
// The defaults of Go are untyped constants: the translated stores cast them to the field type.
#![allow(clippy::unnecessary_cast)]

use std::sync::atomic::Ordering::SeqCst;

use tidb_base::Error;

use super::session::SessionVars;
use super::tidb_defs::*;
use super::tidb_vars::*;
use super::varsutil::*;

// go-part: defaultSysVars (sysvar.go:L510 ddl_slow_threshold GetGlobal closure) L1
pub(super) fn gg_ddl_slow_threshold(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(DDLSlowOprThreshold.load(SeqCst).to_string())
}

// go-part: defaultSysVars (sysvar.go:L1049 max_user_connections GetGlobal closure) L1
pub(super) fn gg_max_user_connections(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(MaxUserConnectionsValue.load(SeqCst).to_string())
}

// go-part: defaultSysVars (sysvar.go:L1571 password_history GetGlobal closure) L1
pub(super) fn gg_password_history(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(PasswordHistory.load(SeqCst).to_string())
}

// go-part: defaultSysVars (sysvar.go:L1577 password_reuse_interval GetGlobal closure) L1
pub(super) fn gg_password_reuse_interval(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(PasswordReuseInterval.load(SeqCst).to_string())
}

// go-part: defaultSysVars (sysvar.go:L752 pd_enable_follower_handle_region GetGlobal closure) L1
pub(super) fn gg_pd_enable_follower_handle_region(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(bool_to_on_off(EnablePDFollowerHandleRegion.load(SeqCst)).to_owned())
}

// go-part: defaultSysVars (sysvar.go:L619 performance_schema_session_connect_attrs_size GetGlobal closure) L1
pub(super) fn gg_performance_schema_session_connect_attrs_size(
    _s: &mut SessionVars,
) -> Result<String, Error> {
    Ok(ConnectAttrsSize.load(SeqCst).to_string())
}

// go-part: defaultSysVars (sysvar.go:L1109 tidb_analyze_column_options GetGlobal closure) L1
pub(super) fn gg_tidb_analyze_column_options(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(AnalyzeColumnOptions.load())
}

// go-part: defaultSysVars (sysvar.go:L1484 tidb_auto_analyze_concurrency GetGlobal closure) L1
pub(super) fn gg_tidb_auto_analyze_concurrency(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(AutoAnalyzeConcurrency.load(SeqCst).to_string())
}

// go-part: defaultSysVars (sysvar.go:L3661 tidb_cb_pd_metadata_error_rate_threshold_ratio GetGlobal closure) L1
pub(super) fn gg_tidb_cb_pd_metadata_error_rate_threshold_ratio(
    _s: &mut SessionVars,
) -> Result<String, Error> {
    Ok(tidb_base::gostd::strconv::format_float(
        CircuitBreakerPDMetadataErrorRateThresholdRatio.load(),
        b'f',
        -1,
        64,
    ))
}

// go-part: defaultSysVars (sysvar.go:L2838 tidb_ddl_disk_quota GetGlobal closure) L1
pub(super) fn gg_tidb_ddl_disk_quota(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(DDLDiskQuota.load(SeqCst).to_string())
}

// go-part: defaultSysVars (sysvar.go:L2831 tidb_ddl_enable_fast_reorg GetGlobal closure) L1
pub(super) fn gg_tidb_ddl_enable_fast_reorg(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(bool_to_on_off(EnableFastReorg.load(SeqCst)).to_owned())
}

// go-part: defaultSysVars (sysvar.go:L830 tidb_ddl_reorg_max_write_speed GetGlobal closure) L1
pub(super) fn gg_tidb_ddl_reorg_max_write_speed(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(DDLReorgMaxWriteSpeed.load(SeqCst).to_string())
}

// go-part: defaultSysVars (sysvar.go:L1096 tidb_enable_auto_analyze GetGlobal closure) L1
pub(super) fn gg_tidb_enable_auto_analyze(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(bool_to_on_off(RunAutoAnalyze.load(SeqCst)).to_owned())
}

// go-part: defaultSysVars (sysvar.go:L1131 tidb_enable_auto_analyze_priority_queue GetGlobal closure) L1
pub(super) fn gg_tidb_enable_auto_analyze_priority_queue(
    _s: &mut SessionVars,
) -> Result<String, Error> {
    Ok(bool_to_on_off(EnableAutoAnalyzePriorityQueue.load(SeqCst)).to_owned())
}

// go-part: defaultSysVars (sysvar.go:L1315 tidb_enable_batch_dml GetGlobal closure) L1
pub(super) fn gg_tidb_enable_batch_dml(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(bool_to_on_off(EnableBatchDML.load(SeqCst)).to_owned())
}

// go-part: defaultSysVars (sysvar.go:L757 tidb_enable_batch_query_region GetGlobal closure) L1
pub(super) fn gg_tidb_enable_batch_query_region(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(bool_to_on_off(EnableBatchQueryRegion.load(SeqCst)).to_owned())
}

// go-part: defaultSysVars (sysvar.go:L3464 tidb_enable_check_constraint GetGlobal closure) L1
pub(super) fn gg_tidb_enable_check_constraint(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(bool_to_on_off(EnableCheckConstraint.load(SeqCst)).to_owned())
}

// go-part: defaultSysVars (sysvar.go:L1528 tidb_enable_dist_task GetGlobal closure) L1
pub(super) fn gg_tidb_enable_dist_task(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(bool_to_on_off(EnableDistTask.load(SeqCst)).to_owned())
}

// go-part: defaultSysVars (sysvar.go:L1536 tidb_enable_fast_create_table GetGlobal closure) L1
pub(super) fn gg_tidb_enable_fast_create_table(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(bool_to_on_off(EnableFastCreateTable.load(SeqCst)).to_owned())
}

// go-part: defaultSysVars (sysvar.go:L1801 tidb_enable_foreign_key GetGlobal closure) L1
pub(super) fn gg_tidb_enable_foreign_key(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(bool_to_on_off(EnableForeignKey.load(SeqCst)).to_owned())
}

// go-part: defaultSysVars (sysvar.go:L1588 tidb_enable_historical_stats_for_capture GetGlobal closure) L1
pub(super) fn gg_tidb_enable_historical_stats_for_capture(
    _s: &mut SessionVars,
) -> Result<String, Error> {
    Ok(bool_to_on_off(EnableHistoricalStatsForCapture.load(SeqCst)).to_owned())
}

// go-part: defaultSysVars (sysvar.go:L1426 tidb_enable_instance_plan_cache GetGlobal closure) L1
pub(super) fn gg_tidb_enable_instance_plan_cache(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(bool_to_on_off(EnableInstancePlanCache.load(SeqCst)).to_owned())
}

// go-part: defaultSysVars (sysvar.go:L762 tidb_enable_local_txn GetGlobal closure) L1
pub(super) fn gg_tidb_enable_local_txn(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(bool_to_on_off(EnableLocalTxn.load(SeqCst)).to_owned())
}

// go-part: defaultSysVars (sysvar.go:L1520 tidb_enable_metadata_lock GetGlobal closure) L1
pub(super) fn gg_tidb_enable_metadata_lock(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(bool_to_on_off(EnableMDL.load(SeqCst)).to_owned())
}

// go-part: defaultSysVars (sysvar.go:L1542 tidb_enable_noop_variables GetGlobal closure) L1
pub(super) fn gg_tidb_enable_noop_variables(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(bool_to_on_off(EnableNoopVariables.load(SeqCst)).to_owned())
}

// go-part: defaultSysVars (sysvar.go:L3119 tidb_enable_resource_control GetGlobal closure) L1
pub(super) fn gg_tidb_enable_resource_control(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(bool_to_on_off(EnableResourceControl.load(SeqCst)).to_owned())
}

// go-part: defaultSysVars (sysvar.go:L1663 tidb_enable_row_level_checksum GetGlobal closure) L1
pub(super) fn gg_tidb_enable_row_level_checksum(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(bool_to_on_off(EnableRowLevelChecksum.load(SeqCst)).to_owned())
}

// go-part: defaultSysVars (sysvar.go:L203 tidb_enable_tiflash_pipeline_model GetGlobal closure) L1
pub(super) fn gg_tidb_enable_tiflash_pipeline_model(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(bool_to_on_off(TiFlashEnablePipelineMode.load(SeqCst)).to_owned())
}

// go-part: defaultSysVars (sysvar.go:L1554 tidb_enable_tmp_storage_on_oom GetGlobal closure) L1
pub(super) fn gg_tidb_enable_tmp_storage_on_oom(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(bool_to_on_off(EnableTmpStorageOnOOM.load(SeqCst)).to_owned())
}

// go-part: defaultSysVars (sysvar.go:L747 tidb_enable_tso_follower_proxy GetGlobal closure) L1
pub(super) fn gg_tidb_enable_tso_follower_proxy(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(bool_to_on_off(EnableTSOFollowerProxy.load(SeqCst)).to_owned())
}

// go-part: defaultSysVars (sysvar.go:L522 tidb_expensive_query_time_threshold GetGlobal closure) L1
pub(super) fn gg_tidb_expensive_query_time_threshold(
    _s: &mut SessionVars,
) -> Result<String, Error> {
    Ok(ExpensiveQueryTimeThreshold.load(SeqCst).to_string())
}

// go-part: defaultSysVars (sysvar.go:L528 tidb_expensive_txn_time_threshold GetGlobal closure) L1
pub(super) fn gg_tidb_expensive_txn_time_threshold(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(ExpensiveTxnTimeThreshold.load(SeqCst).to_string())
}

// go-part: defaultSysVars (sysvar.go:L463 tidb_general_log GetGlobal closure) L1
pub(super) fn gg_tidb_general_log(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(bool_to_on_off(ProcessGeneralLog.load(SeqCst)).to_owned())
}

// go-part: defaultSysVars (sysvar.go:L1144 tidb_gogc_tuner_threshold GetGlobal closure) L1
pub(super) fn gg_tidb_gogc_tuner_threshold(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(tidb_base::gostd::strconv::format_float(
        GOGCTunerThreshold.load(),
        b'f',
        -1,
        64,
    ))
}

// go-part: defaultSysVars (sysvar.go:L2965 tidb_ignore_inlist_plan_digest GetGlobal closure) L1
pub(super) fn gg_tidb_ignore_inlist_plan_digest(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(bool_to_on_off(IgnoreInlistPlanDigest.load(SeqCst)).to_owned())
}

// go-part: defaultSysVars (sysvar.go:L1448 tidb_instance_plan_cache_max_size GetGlobal closure) L1
pub(super) fn gg_tidb_instance_plan_cache_max_size(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(InstancePlanCacheMaxMemSize.load(SeqCst).to_string())
}

// go-part: defaultSysVars (sysvar.go:L1436 tidb_instance_plan_cache_reserved_percentage GetGlobal closure) L1
pub(super) fn gg_tidb_instance_plan_cache_reserved_percentage(
    _s: &mut SessionVars,
) -> Result<String, Error> {
    Ok(tidb_base::gostd::strconv::format_float(
        InstancePlanCacheReservedPercentage.load(),
        b'f',
        -1,
        64,
    ))
}

// go-part: defaultSysVars (sysvar.go:L454 tidb_log_file_max_days GetGlobal closure) L1
pub(super) fn gg_tidb_log_file_max_days(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(GlobalLogMaxDays.load(SeqCst).to_string())
}

// go-part: defaultSysVars (sysvar.go:L1468 tidb_max_auto_analyze_time GetGlobal closure) L1
pub(super) fn gg_tidb_max_auto_analyze_time(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(MaxAutoAnalyzeTime.load(SeqCst).to_string())
}

// go-part: defaultSysVars (sysvar.go:L1460 tidb_mem_oom_action GetGlobal closure) L1
pub(super) fn gg_tidb_mem_oom_action(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(OOMAction.load())
}

// go-part: defaultSysVars (sysvar.go:L798 tidb_mem_quota_binding_cache GetGlobal closure) L1
pub(super) fn gg_tidb_mem_quota_binding_cache(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(MemQuotaBindingCache.load(SeqCst).to_string())
}

// go-part: defaultSysVars (sysvar.go:L1568 tidb_memory_usage_alarm_keep_record_num GetGlobal closure) L1
pub(super) fn gg_tidb_memory_usage_alarm_keep_record_num(
    _s: &mut SessionVars,
) -> Result<String, Error> {
    Ok(MemoryUsageAlarmKeepRecordNum.load(SeqCst).to_string())
}

// go-part: defaultSysVars (sysvar.go:L1085 tidb_persist_analyze_options GetGlobal closure) L1
pub(super) fn gg_tidb_persist_analyze_options(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(bool_to_on_off(PersistAnalyzeOptions.load(SeqCst)).to_owned())
}

// go-part: defaultSysVars (sysvar.go:L1380 tidb_prepared_plan_cache_memory_guard_ratio GetGlobal closure) L1
pub(super) fn gg_tidb_prepared_plan_cache_memory_guard_ratio(
    _s: &mut SessionVars,
) -> Result<String, Error> {
    Ok(tidb_base::gostd::strconv::format_float(
        PreparedPlanCacheMemoryGuardRatio.load(),
        b'f',
        -1,
        64,
    ))
}

// go-part: defaultSysVars (sysvar.go:L1336 tidb_query_log_max_len GetGlobal closure) L1
pub(super) fn gg_tidb_query_log_max_len(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(QueryLogMaxLen.load(SeqCst).to_string())
}

// go-part: defaultSysVars (sysvar.go:L596 tidb_rc_read_check_ts GetGlobal closure) L1
pub(super) fn gg_tidb_rc_read_check_ts(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(bool_to_on_off(EnableRCReadCheckTS.load(SeqCst)).to_owned())
}

// go-part: defaultSysVars (sysvar.go:L3129 tidb_resource_control_strict_mode GetGlobal closure) L1
pub(super) fn gg_tidb_resource_control_strict_mode(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(bool_to_on_off(EnableResourceControlStrictMode.load(SeqCst)).to_owned())
}

// go-part: defaultSysVars (sysvar.go:L3490 tidb_schema_cache_size GetGlobal closure) L1
pub(super) fn gg_tidb_schema_cache_size(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(SchemaCacheSizeOriginText.load())
}

// go-part: defaultSysVars (sysvar.go:L3547 tidb_service_scope GetGlobal closure) L1
pub(super) fn gg_tidb_service_scope(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(ServiceScope.load())
}

// go-part: defaultSysVars (sysvar.go:L1320 tidb_stats_cache_mem_quota GetGlobal closure) L1
pub(super) fn gg_tidb_stats_cache_mem_quota(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(StatsCacheMemQuota.load(SeqCst).to_string())
}

// go-part: defaultSysVars (sysvar.go:L1304 tidb_stats_load_pseudo_timeout GetGlobal closure) L1
pub(super) fn gg_tidb_stats_load_pseudo_timeout(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(bool_to_on_off(StatsLoadPseudoTimeout.load(SeqCst)).to_owned())
}

// go-part: defaultSysVars (sysvar.go:L2677 tidb_stats_load_sync_wait GetGlobal closure) L1
pub(super) fn gg_tidb_stats_load_sync_wait(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(StatsLoadSyncWait.load(SeqCst).to_string())
}

// go-part: defaultSysVars (sysvar.go:L741 tidb_tso_client_batch_max_wait_time GetGlobal closure) L1
pub(super) fn gg_tidb_tso_client_batch_max_wait_time(
    _s: &mut SessionVars,
) -> Result<String, Error> {
    Ok(tidb_base::gostd::strconv::format_float(
        MaxTSOBatchWaitInterval.load(),
        b'f',
        -1,
        64,
    ))
}

// go-part: defaultSysVars (sysvar.go:L2992 tidb_ttl_delete_batch_size GetGlobal closure) L1
pub(super) fn gg_tidb_ttl_delete_batch_size(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(TTLDeleteBatchSize.load(SeqCst).to_string())
}

// go-part: defaultSysVars (sysvar.go:L3003 tidb_ttl_delete_rate_limit GetGlobal closure) L1
pub(super) fn gg_tidb_ttl_delete_rate_limit(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(TTLDeleteRateLimit.load(SeqCst).to_string())
}

// go-part: defaultSysVars (sysvar.go:L3108 tidb_ttl_delete_worker_count GetGlobal closure) L1
pub(super) fn gg_tidb_ttl_delete_worker_count(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(TTLDeleteWorkerCount.load(SeqCst).to_string())
}

// go-part: defaultSysVars (sysvar.go:L2971 tidb_ttl_job_enable GetGlobal closure) L1
pub(super) fn gg_tidb_ttl_job_enable(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(bool_to_on_off(EnableTTLJob.load(SeqCst)).to_owned())
}

// go-part: defaultSysVars (sysvar.go:L3177 tidb_ttl_running_tasks GetGlobal closure) L1
pub(super) fn gg_tidb_ttl_running_tasks(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(TTLRunningTasks.load(SeqCst).to_string())
}

// go-part: defaultSysVars (sysvar.go:L2981 tidb_ttl_scan_batch_size GetGlobal closure) L1
pub(super) fn gg_tidb_ttl_scan_batch_size(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(TTLScanBatchSize.load(SeqCst).to_string())
}

// go-part: defaultSysVars (sysvar.go:L3096 tidb_ttl_scan_worker_count GetGlobal closure) L1
pub(super) fn gg_tidb_ttl_scan_worker_count(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(TTLScanWorkerCount.load(SeqCst).to_string())
}

// go-part: defaultSysVars (sysvar.go:L663 validate_password.length GetGlobal closure) L1
pub(super) fn gg_validate_password_length(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(PasswordValidationLength.load(SeqCst).to_string())
}

// go-part: defaultSysVars (sysvar.go:L685 validate_password.mixed_case_count GetGlobal closure) L1
pub(super) fn gg_validate_password_mixed_case_count(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(PasswordValidationMixedCaseCount.load(SeqCst).to_string())
}

// go-part: defaultSysVars (sysvar.go:L707 validate_password.number_count GetGlobal closure) L1
pub(super) fn gg_validate_password_number_count(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(PasswordValidtaionNumberCount.load(SeqCst).to_string())
}

// go-part: defaultSysVars (sysvar.go:L729 validate_password.special_char_count GetGlobal closure) L1
pub(super) fn gg_validate_password_special_char_count(
    _s: &mut SessionVars,
) -> Result<String, Error> {
    Ok(PasswordValidationSpecialCharCount.load(SeqCst).to_string())
}

// go-part: defaultSysVars (sysvar.go:L507 ddl_slow_threshold SetGlobal closure) L1
pub(super) fn sg_ddl_slow_threshold(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    DDLSlowOprThreshold.store(
        tidb_opt_positive_int32(val, DefTiDBDDLSlowOprThreshold as isize) as _,
        SeqCst,
    );
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1046 max_user_connections SetGlobal closure) L1
pub(super) fn sg_max_user_connections(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    MaxUserConnectionsValue.store(
        tidb_opt_int64(val, DefMaxUserConnections as i64) as _,
        SeqCst,
    );
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1573 password_history SetGlobal closure) L1
pub(super) fn sg_password_history(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    PasswordHistory.store(
        tidb_opt_int64(val, DefPasswordReuseHistory as i64) as _,
        SeqCst,
    );
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1579 password_reuse_interval SetGlobal closure) L1
pub(super) fn sg_password_reuse_interval(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    PasswordReuseInterval.store(
        tidb_opt_int64(val, DefPasswordReuseTime as i64) as _,
        SeqCst,
    );
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L622 performance_schema_session_connect_attrs_size SetGlobal closure) L1
pub(super) fn sg_performance_schema_session_connect_attrs_size(
    _s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    ConnectAttrsSize.store(tidb_opt_int64(val, DefConnectAttrsSize as i64) as _, SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3620 tidb_accelerate_user_creation_update SetGlobal closure) L1
pub(super) fn sg_tidb_accelerate_user_creation_update(
    _s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    AccelerateUserCreationUpdate.store(tidb_opt_on(val), SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2833 tidb_ddl_enable_fast_reorg SetGlobal closure) L1
pub(super) fn sg_tidb_ddl_enable_fast_reorg(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    EnableFastReorg.store(tidb_opt_on(val), SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1099 tidb_enable_auto_analyze SetGlobal closure) L1
pub(super) fn sg_tidb_enable_auto_analyze(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    RunAutoAnalyze.store(tidb_opt_on(val), SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1134 tidb_enable_auto_analyze_priority_queue SetGlobal closure) L1
pub(super) fn sg_tidb_enable_auto_analyze_priority_queue(
    _s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    EnableAutoAnalyzePriorityQueue.store(tidb_opt_on(val), SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1312 tidb_enable_batch_dml SetGlobal closure) L1
pub(super) fn sg_tidb_enable_batch_dml(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    EnableBatchDML.store(tidb_opt_on(val), SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3461 tidb_enable_check_constraint SetGlobal closure) L1
pub(super) fn sg_tidb_enable_check_constraint(
    _vars: &mut SessionVars,
    s: &str,
) -> Result<(), Error> {
    EnableCheckConstraint.store(tidb_opt_on(s), SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1798 tidb_enable_foreign_key SetGlobal closure) L1
pub(super) fn sg_tidb_enable_foreign_key(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    EnableForeignKey.store(tidb_opt_on(val), SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1584 tidb_enable_historical_stats_for_capture SetGlobal closure) L1
pub(super) fn sg_tidb_enable_historical_stats_for_capture(
    _vars: &mut SessionVars,
    s: &str,
) -> Result<(), Error> {
    EnableHistoricalStatsForCapture.store(tidb_opt_on(s), SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1429 tidb_enable_instance_plan_cache SetGlobal closure) L1
pub(super) fn sg_tidb_enable_instance_plan_cache(
    _s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    EnableInstancePlanCache.store(tidb_opt_on(val), SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1539 tidb_enable_noop_variables SetGlobal closure) L1
pub(super) fn sg_tidb_enable_noop_variables(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    EnableNoopVariables.store(tidb_opt_on(val), SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1666 tidb_enable_row_level_checksum SetGlobal closure) L1
pub(super) fn sg_tidb_enable_row_level_checksum(
    _vars: &mut SessionVars,
    s: &str,
) -> Result<(), Error> {
    EnableRowLevelChecksum.store(tidb_opt_on(s), SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L200 tidb_enable_tiflash_pipeline_model SetGlobal closure) L1
pub(super) fn sg_tidb_enable_tiflash_pipeline_model(
    _vars: &mut SessionVars,
    s: &str,
) -> Result<(), Error> {
    TiFlashEnablePipelineMode.store(tidb_opt_on(s), SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1551 tidb_enable_tmp_storage_on_oom SetGlobal closure) L1
pub(super) fn sg_tidb_enable_tmp_storage_on_oom(
    _s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    EnableTmpStorageOnOOM.store(tidb_opt_on(val), SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L519 tidb_expensive_query_time_threshold SetGlobal closure) L1
pub(super) fn sg_tidb_expensive_query_time_threshold(
    _s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    ExpensiveQueryTimeThreshold.store(
        tidb_opt_positive_int32(val, DefTiDBExpensiveQueryTimeThreshold as isize) as _,
        SeqCst,
    );
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L525 tidb_expensive_txn_time_threshold SetGlobal closure) L1
pub(super) fn sg_tidb_expensive_txn_time_threshold(
    _s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    ExpensiveTxnTimeThreshold.store(
        tidb_opt_positive_int32(val, DefTiDBExpensiveTxnTimeThreshold as isize) as _,
        SeqCst,
    );
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1016 tidb_gc_max_wait_time SetGlobal closure) L1
pub(super) fn sg_tidb_gc_max_wait_time(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    GCMaxWaitTime.store(
        tidb_opt_int64(val, DefTiDBGCMaxWaitTime as i64) as _,
        SeqCst,
    );
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L460 tidb_general_log SetGlobal closure) L1
pub(super) fn sg_tidb_general_log(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    ProcessGeneralLog.store(tidb_opt_on(val), SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2962 tidb_ignore_inlist_plan_digest SetGlobal closure) L1
pub(super) fn sg_tidb_ignore_inlist_plan_digest(
    _vars: &mut SessionVars,
    s: &str,
) -> Result<(), Error> {
    IgnoreInlistPlanDigest.store(tidb_opt_on(s), SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1463 tidb_mem_oom_action SetGlobal closure) L1
pub(super) fn sg_tidb_mem_oom_action(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    OOMAction.store(val.to_owned());
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L800 tidb_mem_quota_binding_cache SetGlobal closure) L1
pub(super) fn sg_tidb_mem_quota_binding_cache(
    _s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    MemQuotaBindingCache.store(
        tidb_opt_int64(val, DefTiDBMemQuotaBindingCache as i64) as _,
        SeqCst,
    );
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1565 tidb_memory_usage_alarm_keep_record_num SetGlobal closure) L1
pub(super) fn sg_tidb_memory_usage_alarm_keep_record_num(
    _s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    MemoryUsageAlarmKeepRecordNum.store(
        tidb_opt_int64(val, DefMemoryUsageAlarmKeepRecordNum as i64) as _,
        SeqCst,
    );
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1559 tidb_memory_usage_alarm_ratio SetGlobal closure) L1
pub(super) fn sg_tidb_memory_usage_alarm_ratio(
    _s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    MemoryUsageAlarmRatio.store(tidb_opt_float64(val, DefMemoryUsageAlarmRatio as f64));
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1333 tidb_query_log_max_len SetGlobal closure) L1
pub(super) fn sg_tidb_query_log_max_len(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    QueryLogMaxLen.store(
        tidb_opt_int64(val, DefTiDBQueryLogMaxLen as i64) as _,
        SeqCst,
    );
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L593 tidb_rc_read_check_ts SetGlobal closure) L1
pub(super) fn sg_tidb_rc_read_check_ts(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    EnableRCReadCheckTS.store(tidb_opt_on(val), SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3551 tidb_schema_version_cache_limit SetGlobal closure) L1
pub(super) fn sg_tidb_schema_version_cache_limit(
    _s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    SchemaVersionCacheLimit.store(
        tidb_opt_int64(val, DefTiDBSchemaVersionCacheLimit as i64) as _,
        SeqCst,
    );
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1307 tidb_stats_load_pseudo_timeout SetGlobal closure) L1
pub(super) fn sg_tidb_stats_load_pseudo_timeout(
    _s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    StatsLoadPseudoTimeout.store(tidb_opt_on(val), SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2680 tidb_stats_load_sync_wait SetGlobal closure) L1
pub(super) fn sg_tidb_stats_load_sync_wait(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    StatsLoadSyncWait.store(
        tidb_opt_int64(val, DefTiDBStatsLoadSyncWait as i64) as _,
        SeqCst,
    );
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L929 tidb_super_read_only SetGlobal closure) L1
pub(super) fn sg_tidb_super_read_only(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    VarTiDBSuperReadOnly.store(tidb_opt_on(val), SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2968 tidb_ttl_job_enable SetGlobal closure) L1
pub(super) fn sg_tidb_ttl_job_enable(_vars: &mut SessionVars, s: &str) -> Result<(), Error> {
    EnableTTLJob.store(tidb_opt_on(s), SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L660 validate_password.length SetGlobal closure) L1
pub(super) fn sg_validate_password_length(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    PasswordValidationLength.store(tidb_opt_int64(val, 8 as i64) as _, SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L682 validate_password.mixed_case_count SetGlobal closure) L1
pub(super) fn sg_validate_password_mixed_case_count(
    _s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    PasswordValidationMixedCaseCount.store(tidb_opt_int64(val, 1 as i64) as _, SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L704 validate_password.number_count SetGlobal closure) L1
pub(super) fn sg_validate_password_number_count(
    _s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    PasswordValidtaionNumberCount.store(tidb_opt_int64(val, 1 as i64) as _, SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L726 validate_password.special_char_count SetGlobal closure) L1
pub(super) fn sg_validate_password_special_char_count(
    _s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    PasswordValidationSpecialCharCount.store(tidb_opt_int64(val, 1 as i64) as _, SeqCst);
    Ok(())
}
