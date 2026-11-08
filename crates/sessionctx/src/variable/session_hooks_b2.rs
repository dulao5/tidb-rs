//! go-file: pkg/sessionctx/variable/sysvar.go
//!
//! The SetGlobal / GetGlobal hooks of D05.2 PR B2: variable-package state with more than one
//! statement, state of other packages ([`super::hook_state`]) and the callbacks other
//! components register. Registered in `session_hooks.rs`.
//!
//! Effects on components tidb-rs does not run (the Go GC tuner, the logger, metrics, the DDL
//! owner, client-go's global config) are runtime-only: the value is validated and kept, the
//! component itself is not driven (N8 §3.4, spec §2.3). Each such call is marked
//! `[runtime-only]` where it occurs.

#![allow(non_upper_case_globals)]

use std::sync::atomic::Ordering::SeqCst;

use tidb_base::Error;
use tidb_base::errors;
use tidb_base::gofmt::Arg;
use tidb_base::gostd;

use super::hook_state::*;
use super::names::*;
use super::session::SessionVars;
use super::session_hooks_ext::parse_int64;
use super::tidb_defs::*;
use super::tidb_vars::*;
use super::varsutil::*;

/// `strconv.ParseInt(s, 10, 32)` with Go's error.
// go-std: strconv.ParseInt(s, 10, 32) L1
fn parse_int32(s: &str) -> Result<i64, Error> {
    let (v, err) = tidb_types::gostd::parse_int(s.as_bytes(), 32);
    match err {
        Ok(()) => Ok(v),
        Err(e) => Err(e.into_base()),
    }
}

/// `strconv.ParseFloat(s, 64)` with Go's error.
// go-std: strconv.ParseFloat(s, 64) L1
fn parse_float(s: &str) -> Result<f64, Error> {
    let (v, err) = tidb_types::gostd::parse_float(s);
    match err {
        Ok(()) => Ok(v),
        Err(e) => Err(errors::new(e.to_string())),
    }
}

/// `strconv.FormatFloat(f, 'f', -1, 64)`.
// go-std: strconv.FormatFloat L1
fn format_float(f: f64) -> String {
    gostd::strconv::format_float(f, b'f', -1, 64)
}

/// `units.RAMInBytes` (docker/go-units v0.5.0 `parseSize` with the binary map).
// go-dep: github.com/docker/go-units.RAMInBytes L1
pub(crate) fn ram_in_bytes(size_str: &str) -> Result<i64, Error> {
    let invalid = || errors::new(format!("invalid size: '{size_str}'"));
    let b = size_str.as_bytes();
    let Some(sep) = b.iter().rposition(|c| b"01234567890. ".contains(c)) else {
        // There should be at least a digit.
        return Err(invalid());
    };
    let (num, sfx) = if b[sep] != b' ' {
        (&size_str[..sep + 1], &size_str[sep + 1..])
    } else {
        // Omit the space separator.
        (&size_str[..sep], &size_str[sep + 1..])
    };
    let mut size = parse_float(num)?;
    // Backward compatibility: reject negative sizes.
    if size < 0.0 {
        return Err(invalid());
    }
    if sfx.is_empty() {
        return Ok(gostd::conv::f64_to_i64(size));
    }
    let bad_suffix = |s: &str| errors::new(format!("invalid suffix: '{s}'"));
    // Process the suffix.
    if sfx.len() > 3 {
        // Too long.
        return Err(bad_suffix(sfx));
    }
    let sfx = sfx.to_ascii_lowercase();
    let sb = sfx.as_bytes();
    // Trivial case: b suffix.
    if sb[0] == b'b' {
        if sb.len() > 1 {
            // no extra characters allowed after b.
            return Err(bad_suffix(&sfx));
        }
        return Ok(gostd::conv::f64_to_i64(size));
    }
    // A suffix from the map.
    let mul: i64 = match sb[0] {
        b'k' => 1 << 10,
        b'm' => 1 << 20,
        b'g' => 1 << 30,
        b't' => 1 << 40,
        b'p' => 1 << 50,
        _ => return Err(bad_suffix(&sfx)),
    };
    size *= mul as f64;
    // The suffix may have extra "b" or "ib" (e.g. KiB or MB).
    if (sb.len() == 2 && sb[1] != b'b') || (sb.len() == 3 && &sfx[1..] != "ib") {
        return Err(bad_suffix(&sfx));
    }
    Ok(gostd::conv::f64_to_i64(size))
}

/// `units.PiB`.
const PiB: i64 = 1 << 50;

// go-part: defaultSysVars (sysvar.go:L440 tidb_log_file_max_days SetGlobal closure) L1
pub(super) fn sg_tidb_log_file_max_days(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    let max_age = parse_int32(val)?;
    GlobalLogMaxDays.store(max_age as i32, SeqCst);
    // logutil.ReplaceLogger(cfg with File.MaxDays): the Go logger [runtime-only].
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L457 tidb_config GetGlobal closure) L1
pub(super) fn gg_tidb_config(_s: &mut SessionVars) -> Result<String, Error> {
    tidb_base::config::get_json_config()
}

// go-part: defaultSysVars (sysvar.go:L626 max_prepared_stmt_count SetGlobal closure) L1
pub(super) fn sg_max_prepared_stmt_count(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    let num = parse_int64(val)?;
    MaxPreparedStmtCountValue.store(num, SeqCst);
    Ok(())
}

/// The SetGlobal closures that hand the value to the PD client:
/// `(*SetPDClientDynamicOption.Load())(name, val)`.
// glue: shared body of the five PD-client dynamic option SetGlobal closures (sysvar.go:L740-L757, L3614)
fn set_pd_client_dynamic_option(name: &str, val: &str) -> Result<(), Error> {
    registered(&SetPDClientDynamicOption)(name, val)
}

// go-part: defaultSysVars (sysvar.go:L740 tidb_tso_client_batch_max_wait_time SetGlobal closure) L1
pub(super) fn sg_tidb_tso_client_batch_max_wait_time(
    _s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    set_pd_client_dynamic_option("tidb_tso_client_batch_max_wait_time", val)
}

// go-part: defaultSysVars (sysvar.go:L747 tidb_enable_tso_follower_proxy SetGlobal closure) L1
pub(super) fn sg_tidb_enable_tso_follower_proxy(
    _s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    set_pd_client_dynamic_option("tidb_enable_tso_follower_proxy", val)
}

// go-part: defaultSysVars (sysvar.go:L752 pd_enable_follower_handle_region SetGlobal closure) L1
pub(super) fn sg_pd_enable_follower_handle_region(
    _s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    set_pd_client_dynamic_option("pd_enable_follower_handle_region", val)
}

// go-part: defaultSysVars (sysvar.go:L757 tidb_enable_batch_query_region SetGlobal closure) L1
pub(super) fn sg_tidb_enable_batch_query_region(
    _s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    set_pd_client_dynamic_option("tidb_enable_batch_query_region", val)
}

// go-part: defaultSysVars (sysvar.go:L3614 tidb_tso_client_rpc_mode SetGlobal closure) L1
pub(super) fn sg_tidb_tso_client_rpc_mode(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    set_pd_client_dynamic_option("tidb_tso_client_rpc_mode", val)
}

// go-part: defaultSysVars (sysvar.go:L762 tidb_enable_local_txn SetGlobal closure) L1
pub(super) fn sg_tidb_enable_local_txn(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    let old_val = EnableLocalTxn.load(SeqCst);
    let new_val = tidb_opt_on(val);
    // Make sure the TxnScope is always Global when disable the Local Txn.
    // ON -> OFF
    if old_val && !new_val {
        s.state.txn_scope = tidb_kv::txn_scope_var::new_global_txn_scope_var();
    }
    EnableLocalTxn.store(new_val, SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L804 tidb_ddl_flashback_concurrency SetGlobal closure) L1
pub(super) fn sg_tidb_ddl_flashback_concurrency(
    _s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    set_ddl_flashback_concurrency(tidb_opt_positive_int32(
        val,
        DefTiDBDDLFlashbackConcurrency as isize,
    ) as i32);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L808 tidb_ddl_reorg_worker_cnt SetGlobal closure) L1
pub(super) fn sg_tidb_ddl_reorg_worker_cnt(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    set_ddl_reorg_worker_counter(
        tidb_opt_positive_int32(val, DefTiDBDDLReorgWorkerCount as isize) as i32,
    );
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L812 tidb_ddl_reorg_batch_size SetGlobal closure) L1
pub(super) fn sg_tidb_ddl_reorg_batch_size(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    set_ddl_reorg_batch_size(
        tidb_opt_positive_int32(val, DefTiDBDDLReorgBatchSize as isize) as i32,
    );
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L816 tidb_ddl_reorg_max_write_speed SetGlobal closure) L1
pub(super) fn sg_tidb_ddl_reorg_max_write_speed(
    _s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    let i64v = ram_in_bytes(val)?;
    if !(0..=PiB).contains(&i64v) {
        // Here we limit the max value to 1 PiB instead of math.MaxInt64, since:
        // 1. it is large enough
        // 2. units.RAMInBytes would first cast the size to a float, and may lose precision when the size is too large
        return Err(errors::new(format!(
            "invalid value for '{i64v}', it should be within [{}, {}]",
            0, PiB
        )));
    }
    DDLReorgMaxWriteSpeed.store(i64v, SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L833 tidb_ddl_error_count_limit SetGlobal closure) L1
pub(super) fn sg_tidb_ddl_error_count_limit(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    set_ddl_error_count_limit(tidb_opt_int64(val, DefTiDBDDLErrorCountLimit));
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L837 tidb_max_delta_schema_count SetGlobal closure) L1
pub(super) fn sg_tidb_max_delta_schema_count(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    // It's a global variable, but it also wants to be cached in server.
    set_max_delta_schema_count(tidb_opt_int64(val, DefTiDBMaxDeltaSchemaCount));
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L887 tidb_store_limit GetGlobal closure) L1
pub(super) fn gg_tidb_store_limit(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(StoreLimit.load(SeqCst).to_string())
}

// go-part: defaultSysVars (sysvar.go:L887 tidb_store_limit SetGlobal closure) L1
pub(super) fn sg_tidb_store_limit(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    StoreLimit.store(tidb_opt_int64(val, DefTiDBStoreLimit), SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L893 tidb_txn_commit_batch_size GetGlobal closure) L1
pub(super) fn gg_tidb_txn_commit_batch_size(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(TxnCommitBatchSize.load(SeqCst).to_string())
}

// go-part: defaultSysVars (sysvar.go:L893 tidb_txn_commit_batch_size SetGlobal closure) L1
pub(super) fn sg_tidb_txn_commit_batch_size(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    TxnCommitBatchSize.store(
        tidb_opt_int64(val, DefTxnCommitBatchSize as i64) as u64,
        SeqCst,
    );
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L901 tidb_restricted_read_only SetGlobal closure) L1
pub(super) fn sg_tidb_restricted_read_only(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    let on = tidb_opt_on(val);
    // For user initiated SET GLOBAL, also change the value of TiDBSuperReadOnly
    if on && s.stmt_ctx.stmt_type == "Set" {
        s.global_vars_accessor
            .set_global_sys_var_only(TiDBSuperReadOnly, "ON", false)?;
        let sv = super::variable::get_sys_var(TiDBSuperReadOnly)
            .expect("tidb_super_read_only is registered");
        (sv.set_global
            .expect("tidb_super_read_only has a SetGlobal hook"))(s, "ON")?;
    }
    RestrictedReadOnly.store(on, SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L933 tidb_enable_gogc_tuner SetGlobal closure) L1
pub(super) fn sg_tidb_enable_gogc_tuner(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    let on = tidb_opt_on(val);
    EnableGOGCTuner.store(on, SeqCst);
    // gctuner.SetDefaultGOGC (when off), GlobalMemoryLimitTuner.UpdateMemoryLimit: the Go GC
    // [runtime-only].
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L942 tidb_gogc_tuner_max_value SetGlobal closure) L1
pub(super) fn sg_tidb_gogc_tuner_max_value(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    let max_value = tidb_opt_int64(val, DefTiDBGOGCMaxValue);
    maxGCPercent.store(max_value as u32, SeqCst); // gctuner.SetMaxGCPercent
    // gctuner.GlobalMemoryLimitTuner.UpdateMemoryLimit: the Go GC [runtime-only].
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L942 tidb_gogc_tuner_max_value GetGlobal closure) L1
pub(super) fn gg_tidb_gogc_tuner_max_value(_s: &mut SessionVars) -> Result<String, Error> {
    Ok((maxGCPercent.load(SeqCst) as i64).to_string())
}

// go-part: defaultSysVars (sysvar.go:L959 tidb_gogc_tuner_min_value SetGlobal closure) L1
pub(super) fn sg_tidb_gogc_tuner_min_value(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    let min_value = tidb_opt_int64(val, DefTiDBGOGCMinValue);
    minGCPercent.store(min_value as u32, SeqCst); // gctuner.SetMinGCPercent
    // gctuner.GlobalMemoryLimitTuner.UpdateMemoryLimit: the Go GC [runtime-only].
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L959 tidb_gogc_tuner_min_value GetGlobal closure) L1
pub(super) fn gg_tidb_gogc_tuner_min_value(_s: &mut SessionVars) -> Result<String, Error> {
    Ok((minGCPercent.load(SeqCst) as i64).to_string())
}

// go-part: defaultSysVars (sysvar.go:L1020 tidb_table_cache_lease SetGlobal closure) L1
pub(super) fn sg_tidb_table_cache_lease(_s: &mut SessionVars, s_val: &str) -> Result<(), Error> {
    let val = parse_int64(s_val)?;
    TableCacheLease.store(val, SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1029 tidb_auto_analyze_partition_batch_size SetGlobal closure) L1
pub(super) fn sg_tidb_auto_analyze_partition_batch_size(
    _vars: &mut SessionVars,
    s: &str,
) -> Result<(), Error> {
    let val = parse_int64(s)?;
    AutoAnalyzePartitionBatchSize.store(val, SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1058 tidb_top_sql_max_time_series_count GetGlobal closure) L1
pub(super) fn gg_tidb_top_sql_max_time_series_count(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(TopSQLMaxStatementCount.load(SeqCst).to_string())
}

// go-part: defaultSysVars (sysvar.go:L1058 tidb_top_sql_max_time_series_count SetGlobal closure) L1
pub(super) fn sg_tidb_top_sql_max_time_series_count(
    _vars: &mut SessionVars,
    s: &str,
) -> Result<(), Error> {
    let val = parse_int64(s)?;
    TopSQLMaxStatementCount.store(val, SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1068 tidb_top_sql_max_meta_count GetGlobal closure) L1
pub(super) fn gg_tidb_top_sql_max_meta_count(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(TopSQLMaxCollect.load(SeqCst).to_string())
}

// go-part: defaultSysVars (sysvar.go:L1068 tidb_top_sql_max_meta_count SetGlobal closure) L1
pub(super) fn sg_tidb_top_sql_max_meta_count(
    _vars: &mut SessionVars,
    s: &str,
) -> Result<(), Error> {
    let val = parse_int64(s)?;
    TopSQLMaxCollect.store(val, SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1080 tidb_persist_analyze_options SetGlobal closure) L1
pub(super) fn sg_tidb_persist_analyze_options(
    _s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    let persist = tidb_opt_on(val);
    PersistAnalyzeOptions.store(persist, SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1104 tidb_analyze_column_options SetGlobal closure) L1
pub(super) fn sg_tidb_analyze_column_options(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    AnalyzeColumnOptions.store(gostd::to_upper(val));
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1143 tidb_gogc_tuner_threshold SetGlobal closure) L1
pub(super) fn sg_tidb_gogc_tuner_threshold(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    let factor = tidb_opt_float64(val, DefTiDBGOGCTunerThreshold);
    GOGCTunerThreshold.store(factor);
    let mut mem_total = tidb_memory::tracker::ServerMemoryLimit.load(SeqCst);
    if mem_total == 0 {
        mem_total = tidb_memory::meminfo::mem_total()?;
    }
    if factor > 0.0 {
        let _threshold = mem_total as f64 * factor;
        // gctuner.Tuning(uint64(threshold)): the Go GC [runtime-only].
    }
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1176 tidb_server_memory_limit SetGlobal closure) L1
pub(super) fn sg_tidb_server_memory_limit(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    let (bt, str_, err) = parse_memory_limit(s, val, val);
    err?;
    tidb_memory::tracker::ServerMemoryLimitOriginText.store(std::sync::Arc::new(str_));
    tidb_memory::tracker::ServerMemoryLimit.store(bt, SeqCst);
    let _threshold = bt as f64 * GOGCTunerThreshold.load();
    // gctuner.Tuning(uint64(threshold)), GlobalMemoryLimitTuner.UpdateMemoryLimit: the Go
    // GC [runtime-only].
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1228 tidb_server_memory_limit_gc_trigger GetGlobal closure) L1
pub(super) fn gg_tidb_server_memory_limit_gc_trigger(
    _s: &mut SessionVars,
) -> Result<String, Error> {
    Ok(format_float(memoryLimitTunerPercentage.load()))
}

// go-part: defaultSysVars (sysvar.go:L1228 tidb_server_memory_limit_gc_trigger SetGlobal closure) L1
pub(super) fn sg_tidb_server_memory_limit_gc_trigger(
    _s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    let float_value = parse_float(val)?;
    memoryLimitTunerPercentage.store(float_value); // GlobalMemoryLimitTuner.SetPercentage
    // GlobalMemoryLimitTuner.UpdateMemoryLimit: the Go GC [runtime-only].
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1277 require_secure_transport GetGlobal closure) L1
pub(super) fn gg_require_secure_transport(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(bool_to_on_off(super::hook_state::RequireSecureTransport.load(SeqCst)).to_owned())
}

// go-part: defaultSysVars (sysvar.go:L1277 require_secure_transport SetGlobal closure) L1
pub(super) fn sg_require_secure_transport(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    super::hook_state::RequireSecureTransport.store(tidb_opt_on(val), SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1318 tidb_stats_cache_mem_quota SetGlobal closure) L1
pub(super) fn sg_tidb_stats_cache_mem_quota(_vars: &mut SessionVars, s: &str) -> Result<(), Error> {
    let v = tidb_opt_int64(s, DefTiDBStatsCacheMemQuota);
    let oldv = StatsCacheMemQuota.load(SeqCst);
    if v != oldv {
        StatsCacheMemQuota.store(v, SeqCst);
        registered(&SetStatsCacheCapacity)(v);
    }
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1339 tidb_committer_concurrency SetGlobal closure) L1
pub(super) fn sg_tidb_committer_concurrency(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    CommitterConcurrency.store(
        tidb_opt_int64(val, DefTiDBCommitterConcurrency) as i32,
        SeqCst,
    );
    // tikvcfg.StoreGlobalConfig(config.GetGlobalConfig().GetTiKVConfig()): client-go's
    // global config [runtime-only].
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1339 tidb_committer_concurrency GetGlobal closure) L1
pub(super) fn gg_tidb_committer_concurrency(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(CommitterConcurrency.load(SeqCst).to_string())
}

// go-part: defaultSysVars (sysvar.go:L1347 tidb_mem_quota_analyze GetGlobal closure) L1
pub(super) fn gg_tidb_mem_quota_analyze(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(registered(&GetMemQuotaAnalyze)().to_string())
}

// go-part: defaultSysVars (sysvar.go:L1347 tidb_mem_quota_analyze SetGlobal closure) L1
pub(super) fn sg_tidb_mem_quota_analyze(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    registered(&SetMemQuotaAnalyze)(tidb_opt_int64(val, DefTiDBMemQuotaAnalyze));
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1374 tidb_prepared_plan_cache_memory_guard_ratio SetGlobal closure) L1
pub(super) fn sg_tidb_prepared_plan_cache_memory_guard_ratio(
    _s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    let f = parse_float(val);
    if let Ok(f) = &f {
        PreparedPlanCacheMemoryGuardRatio.store(*f);
    }
    f.map(|_| ())
}

// go-part: defaultSysVars (sysvar.go:L1433 tidb_instance_plan_cache_reserved_percentage SetGlobal closure) L1
pub(super) fn sg_tidb_instance_plan_cache_reserved_percentage(
    _s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    let v = tidb_opt_float64(val, DefTiDBInstancePlanCacheReservedPercentage);
    if !(0.0..=1.0).contains(&v) {
        return Err(errors::errorf(
            "invalid tidb_instance_plan_cache_reserved_percentage value %s",
            &[Arg::from(val.to_owned())],
        ));
    }
    InstancePlanCacheReservedPercentage.store(v);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1447 tidb_instance_plan_cache_max_size SetGlobal closure) L1
pub(super) fn sg_tidb_instance_plan_cache_max_size(
    _s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    let (v, str_) = parse_byte_size(val);
    // Go also checks `v < 0`, never true for its uint64 `v`.
    if str_.is_empty() {
        return Err(errors::errorf(
            "invalid tidb_instance_plan_cache_max_mem_size value %s",
            &[Arg::from(val.to_owned())],
        ));
    }
    InstancePlanCacheMaxMemSize.store(v as i64, SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1467 tidb_max_auto_analyze_time SetGlobal closure) L1
pub(super) fn sg_tidb_max_auto_analyze_time(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    let num = parse_int64(val)?;
    MaxAutoAnalyzeTime.store(num, SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1479 tidb_auto_analyze_concurrency SetGlobal closure) L1
pub(super) fn sg_tidb_auto_analyze_concurrency(
    _s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    let num = parse_int64(val)?;
    AutoAnalyzeConcurrency.store(num as i32, SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1512 tidb_enable_metadata_lock SetGlobal closure) L1
pub(super) fn sg_tidb_enable_metadata_lock(
    _vars: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    if EnableMDL.load(SeqCst) != tidb_opt_on(val) {
        registered(&SwitchMDL)(tidb_opt_on(val))?;
    }
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1523 tidb_enable_dist_task SetGlobal closure) L1
pub(super) fn sg_tidb_enable_dist_task(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    if EnableDistTask.load(SeqCst) != tidb_opt_on(val) {
        EnableDistTask.store(tidb_opt_on(val), SeqCst);
    }
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1531 tidb_enable_fast_create_table SetGlobal closure) L1
pub(super) fn sg_tidb_enable_fast_create_table(
    _s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    if EnableFastCreateTable.load(SeqCst) != tidb_opt_on(val) {
        EnableFastCreateTable.store(tidb_opt_on(val), SeqCst);
    }
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1603 tidb_low_resolution_tso_update_interval SetGlobal closure) L1
pub(super) fn sg_tidb_low_resolution_tso_update_interval(
    _s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    LowResolutionTSOUpdateInterval.store(
        tidb_opt_int64(val, DefTiDBLowResolutionTSOUpdateInterval) as u32,
        SeqCst,
    );
    let f = SetLowResolutionTSOUpdateInterval.read().clone();
    if let Some(f) = f {
        let interval =
            std::time::Duration::from_millis(LowResolutionTSOUpdateInterval.load(SeqCst) as u64);
        return f(interval);
    }
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1650 tidb_row_format_version SetGlobal closure) L1
pub(super) fn sg_tidb_row_format_version(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    set_ddl_reorg_row_format(tidb_opt_int64(val, DefTiDBRowFormatV2));
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1705 tidb_load_binding_timeout SetGlobal closure) L1
pub(super) fn sg_tidb_load_binding_timeout(vars: &mut SessionVars, s: &str) -> Result<(), Error> {
    let timeout_ms = tidb_opt_positive_int32(s, DefTiDBLoadBindingTimeout as isize);
    vars.load_binding_timeout = timeout_ms as u64;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2325 tidb_txn_entry_size_limit SetGlobal closure) L1
pub(super) fn sg_tidb_txn_entry_size_limit(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    TxnEntrySizeLimit.store(
        tidb_opt_uint64(val, DefTiDBTxnEntrySizeLimit as u64),
        SeqCst,
    );
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2542 tidb_partition_prune_mode SetGlobal closure) L1
pub(super) fn sg_tidb_partition_prune_mode(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    let new_mode = val.trim().to_lowercase();
    if new_mode == "static" {
        s.stmt_ctx
            .append_warning(super::error::ErrWarnDeprecatedSyntaxSimpleMsg.fast_gen(
                "static prune mode is deprecated and will be removed in the future release.",
                vec![],
            ));
    }
    if new_mode == "dynamic" {
        s.stmt_ctx.append_warning(errors::new(
            "Please analyze all partition tables again for consistency between partition and global stats",
        ));
    }
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2661 tidb_enable_paging SetGlobal closure) L1
pub(super) fn sg_tidb_enable_paging(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.enable_paging = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2785 tidb_simplified_metrics SetGlobal closure) L1
pub(super) fn sg_tidb_simplified_metrics(_vars: &mut SessionVars, _s: &str) -> Result<(), Error> {
    // metrics.ToggleSimplifiedMode(TiDBOptOn(s)): the Go metrics registry [runtime-only].
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2814 tidb_generate_binary_plan SetGlobal closure) L1
pub(super) fn sg_tidb_generate_binary_plan(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    GenerateBinaryPlan.store(tidb_opt_on(val), SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2838 tidb_ddl_disk_quota SetGlobal closure) L1
pub(super) fn sg_tidb_ddl_disk_quota(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    DDLDiskQuota.store(tidb_opt_uint64(val, DefTiDBDDLDiskQuota as u64), SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2940 tidb_external_ts SetGlobal closure) L1
pub(super) fn sg_tidb_external_ts(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    let ts = super::session_hooks_state::parse_ts_from_number_or_time(s, val)?;
    registered(&SetExternalTimestamp)(ts)
}

// go-part: defaultSysVars (sysvar.go:L2940 tidb_external_ts GetGlobal closure) L1
pub(super) fn gg_tidb_external_ts(_s: &mut SessionVars) -> Result<String, Error> {
    let ts = registered(&GetExternalTimestamp)()?;
    Ok((ts as i64).to_string())
}

/// The shared body of the TTL `ParseInt` + store SetGlobal closures.
// glue: shared body of the TTL SetGlobal closures (sysvar.go:L2974-L3170)
fn set_ttl_i64(cell: &std::sync::atomic::AtomicI64, s: &str) -> Result<(), Error> {
    let val = parse_int64(s)?;
    cell.store(val, SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2974 tidb_ttl_scan_batch_size SetGlobal closure) L1
pub(super) fn sg_tidb_ttl_scan_batch_size(_vars: &mut SessionVars, s: &str) -> Result<(), Error> {
    set_ttl_i64(&TTLScanBatchSize, s)
}

// go-part: defaultSysVars (sysvar.go:L2985 tidb_ttl_delete_batch_size SetGlobal closure) L1
pub(super) fn sg_tidb_ttl_delete_batch_size(_vars: &mut SessionVars, s: &str) -> Result<(), Error> {
    set_ttl_i64(&TTLDeleteBatchSize, s)
}

// go-part: defaultSysVars (sysvar.go:L2996 tidb_ttl_delete_rate_limit SetGlobal closure) L1
pub(super) fn sg_tidb_ttl_delete_rate_limit(_vars: &mut SessionVars, s: &str) -> Result<(), Error> {
    set_ttl_i64(&TTLDeleteRateLimit, s)
}

/// `time.ParseInLocation(FullDayTimeFormat, s, time.UTC)` with Go's error.
// go-std: time.ParseInLocation L2 ("15:04 -0700" only)
fn parse_full_day_time_utc(s: &str) -> Result<super::timeparse::DayTime, Error> {
    super::timeparse::parse_day_time(super::variable::FullDayTimeFormat, s, &gostd::time::utc())
        .map_err(errors::new)
}

// go-part: defaultSysVars (sysvar.go:L3062 tidb_ttl_job_schedule_window_start_time SetGlobal closure) L1
pub(super) fn sg_tidb_ttl_job_schedule_window_start_time(
    _vars: &mut SessionVars,
    s: &str,
) -> Result<(), Error> {
    let start_time = parse_full_day_time_utc(s)?;
    *TTLJobScheduleWindowStartTime.lock() = start_time;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3062 tidb_ttl_job_schedule_window_start_time GetGlobal closure) L1
pub(super) fn gg_tidb_ttl_job_schedule_window_start_time(
    _vars: &mut SessionVars,
) -> Result<String, Error> {
    let start_time = TTLJobScheduleWindowStartTime.lock().clone();
    Ok(super::timeparse::format_full_day_time(&start_time))
}

// go-part: defaultSysVars (sysvar.go:L3075 tidb_ttl_job_schedule_window_end_time SetGlobal closure) L1
pub(super) fn sg_tidb_ttl_job_schedule_window_end_time(
    _vars: &mut SessionVars,
    s: &str,
) -> Result<(), Error> {
    let end_time = parse_full_day_time_utc(s)?;
    *TTLJobScheduleWindowEndTime.lock() = end_time;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3075 tidb_ttl_job_schedule_window_end_time GetGlobal closure) L1
pub(super) fn gg_tidb_ttl_job_schedule_window_end_time(
    _vars: &mut SessionVars,
) -> Result<String, Error> {
    let end_time = TTLJobScheduleWindowEndTime.lock().clone();
    Ok(super::timeparse::format_full_day_time(&end_time))
}

// go-part: defaultSysVars (sysvar.go:L3088 tidb_ttl_scan_worker_count SetGlobal closure) L1
pub(super) fn sg_tidb_ttl_scan_worker_count(_vars: &mut SessionVars, s: &str) -> Result<(), Error> {
    let val = parse_int64(s)?;
    TTLScanWorkerCount.store(val as i32, SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3100 tidb_ttl_delete_worker_count SetGlobal closure) L1
pub(super) fn sg_tidb_ttl_delete_worker_count(
    _vars: &mut SessionVars,
    s: &str,
) -> Result<(), Error> {
    let val = parse_int64(s)?;
    TTLDeleteWorkerCount.store(val as i32, SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3112 tidb_enable_resource_control SetGlobal closure) L1
pub(super) fn sg_tidb_enable_resource_control(
    _vars: &mut SessionVars,
    s: &str,
) -> Result<(), Error> {
    if tidb_opt_on(s) != EnableResourceControl.load(SeqCst) {
        EnableResourceControl.store(tidb_opt_on(s), SeqCst);
        registered(&SetGlobalResourceControl)(tidb_opt_on(s));
        // logutil.BgLogger().Info("set resource control", …) [runtime-only log].
    }
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3122 tidb_resource_control_strict_mode SetGlobal closure) L1
pub(super) fn sg_tidb_resource_control_strict_mode(
    _vars: &mut SessionVars,
    s: &str,
) -> Result<(), Error> {
    let op_on = tidb_opt_on(s);
    if op_on != EnableResourceControlStrictMode.load(SeqCst) {
        EnableResourceControlStrictMode.store(op_on, SeqCst);
        // logutil.BgLogger().Info("change resource control strict mode", …) [runtime-only log].
    }
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3170 tidb_ttl_running_tasks SetGlobal closure) L1
pub(super) fn sg_tidb_ttl_running_tasks(_vars: &mut SessionVars, s: &str) -> Result<(), Error> {
    let val = parse_int64(s)?;
    TTLRunningTasks.store(val as i32, SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3194 tidb_opt_fix_control SetGlobal closure) L1
pub(super) fn sg_tidb_opt_fix_control(vars: &mut SessionVars, val: &str) -> Result<(), Error> {
    // validation logic for setting global
    // we don't put this in Validation to avoid repeating the checking logic for setting session.
    let (_, warnings) = super::session_state::parse_to_map(val)?;
    for warning in warnings {
        vars.stmt_ctx.append_warning(errors::new(warning));
    }
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3467 tidb_schema_cache_size SetGlobal closure) L1
pub(super) fn sg_tidb_schema_cache_size(vars: &mut SessionVars, val: &str) -> Result<(), Error> {
    // It does not take effect immediately, but within a ddl lease, infoschema reload would cause the v2 to be used.
    let (bt, str_, err) = parse_schema_cache_size(vars, val, val);
    err?;
    let change = ChangeSchemaCacheSize.read().clone();
    if SchemaCacheSize.load(SeqCst) != bt
        && let Some(change) = change
    {
        change(bt)?;
    }
    SchemaCacheSize.store(bt, SeqCst);
    SchemaCacheSizeOriginText.store(str_);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3659 tidb_cb_pd_metadata_error_rate_threshold_ratio SetGlobal closure) L1
pub(super) fn sg_tidb_cb_pd_metadata_error_rate_threshold_ratio(
    _s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    let v = tidb_opt_float64(val, DefTiDBCircuitBreakerPDMetaErrorRateRatio);
    if !(0.0..=1.0).contains(&v) {
        return Err(errors::errorf(
            "invalid tidb_cb_pd_metadata_error_rate_threshold_ratio value %s",
            &[Arg::from(val.to_owned())],
        ));
    }
    CircuitBreakerPDMetadataErrorRateThresholdRatio.store(v);
    let f = ChangePDMetadataCircuitBreakerErrorRateThresholdRatio
        .read()
        .clone();
    if let Some(f) = f {
        f(gostd::conv::f64_to_u32(v * 100.0));
    }
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3704 tidb_slow_log_max_per_sec SetGlobal closure) L1
pub(super) fn sg_tidb_slow_log_max_per_sec(_sv: &mut SessionVars, s: &str) -> Result<(), Error> {
    let d = tidb_opt_int(s, 0) as i64;
    // [M-F2I] int(GlobalSlowLogRateLimiter.Limit()): int(rate.Inf) depends on the target.
    if d == gostd::conv::f64_to_i64(GlobalSlowLogRateLimit.load()) {
        return Ok(());
    }
    if d == 0 {
        GlobalSlowLogRateLimit.store(f64::INFINITY);
        return Ok(());
    }
    GlobalSlowLogRateLimit.store(d as f64);
    GlobalSlowLogRateBurst.store(d, SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3704 tidb_slow_log_max_per_sec GetGlobal closure) L1
pub(super) fn gg_tidb_slow_log_max_per_sec(_sv: &mut SessionVars) -> Result<String, Error> {
    // [M-F2I] int(rate.Inf): MinInt64 on amd64, saturated on arm64 (tidb-rs #27).
    Ok(gostd::conv::f64_to_i64(GlobalSlowLogRateLimit.load()).to_string())
}

/// `config.GetGlobalConfig().Instance.StmtSummaryEnablePersistent`: statement summary v2
/// (`GlobalStmtSummary`, D14.2) instead of the v1 map.
// glue: the persistent-branch test of the stmtsummaryv2 wrappers
fn stmt_summary_persistent() -> bool {
    tidb_base::config::get_global_config()
        .instance
        .stmt_summary_enable_persistent
}

// go-part: defaultSysVars (sysvar.go:L858 tidb_enable_stmt_summary SetGlobal closure) L1
// go-dep: github.com/pingcap/tidb/pkg/util/stmtsummary/v2.SetEnabled L2 (v2 branch [runtime-only], D14.2)
pub(super) fn sg_tidb_enable_stmt_summary(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    if !stmt_summary_persistent() {
        stmt_summary::Enabled.store(tidb_opt_on(val), SeqCst);
        // `if !value { ssMap.Clear() }`: the summaries are D14.2.
    }
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L862 tidb_stmt_summary_internal_query SetGlobal closure) L1
// go-dep: github.com/pingcap/tidb/pkg/util/stmtsummary/v2.SetEnableInternalQuery L2 (v2 branch [runtime-only], D14.2)
pub(super) fn sg_tidb_stmt_summary_internal_query(
    _s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    if !stmt_summary_persistent() {
        stmt_summary::EnableInternalQuery.store(tidb_opt_on(val), SeqCst);
    }
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L866 tidb_stmt_summary_refresh_interval SetGlobal closure) L1
// go-dep: github.com/pingcap/tidb/pkg/util/stmtsummary/v2.SetRefreshInterval L2 (v2 branch [runtime-only], D14.2)
pub(super) fn sg_tidb_stmt_summary_refresh_interval(
    _s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    // convert val to int64
    let v = tidb_opt_int64(val, DefTiDBStmtSummaryRefreshInterval);
    if !stmt_summary_persistent() {
        stmt_summary::RefreshInterval.store(v, SeqCst);
    }
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L871 tidb_stmt_summary_history_size SetGlobal closure) L1
// go-dep: github.com/pingcap/tidb/pkg/util/stmtsummary/v2.SetHistorySize L1
pub(super) fn sg_tidb_stmt_summary_history_size(
    _s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    let v = tidb_opt_int(val, DefTiDBStmtSummaryHistorySize as isize);
    if !stmt_summary_persistent() {
        stmt_summary::HistorySize.store(v as i32, SeqCst);
    }
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L875 tidb_stmt_summary_max_stmt_count SetGlobal closure) L1
// go-dep: github.com/pingcap/tidb/pkg/util/stmtsummary/v2.SetMaxStmtCount L2 (v2 branch [runtime-only], D14.2)
pub(super) fn sg_tidb_stmt_summary_max_stmt_count(
    _s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    let v = tidb_opt_int(val, DefTiDBStmtSummaryMaxStmtCount as isize);
    if !stmt_summary_persistent() {
        stmt_summary::MaxStmtCount.store(v as u32, SeqCst);
        // ssMap.summaryMap.SetCapacity: the summaries are D14.2.
    }
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L879 tidb_stmt_summary_max_sql_length SetGlobal closure) L1
// go-dep: github.com/pingcap/tidb/pkg/util/stmtsummary/v2.SetMaxSQLLength L2 (v2 branch [runtime-only], D14.2)
pub(super) fn sg_tidb_stmt_summary_max_sql_length(
    _s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    let v = tidb_opt_int(val, DefTiDBStmtSummaryMaxSQLLength as isize);
    if !stmt_summary_persistent() {
        stmt_summary::MaxSQLLength.store(v as i32, SeqCst);
    }
    Ok(())
}
