//! go-file: pkg/sessionctx/variable/sysvar.go
//! go-file: pkg/sessionctx/variable/varsutil.go
//!
//! SetGlobal / GetGlobal hooks whose targets live outside the variable package (D05.2 T5):
//! the instance config (`config.GetGlobalConfig().Instance`), the LDAP plugins' settings
//! ([`super::ldap_state`]), the memory package's limits and a few variable-package values
//! with more than one statement. Registered in `session_hooks.rs`.
//!
//! Instance config: Go writes the plain (non-atomic) fields of the global `*Config` in place
//! (`config.GetGlobalConfig().Instance.MaxConnections = …`, or `atomic.StoreUint64(&…)`).
//! The Rust global config is an immutable `Arc` behind an `ArcSwap`, so these hooks clone the
//! config, change the field and store it (M-SNAP, L2): later readers see the same value.

#![allow(non_upper_case_globals)]

use std::sync::Arc;
use std::sync::atomic::Ordering::SeqCst;

use tidb_base::Error;
use tidb_base::config;
use tidb_base::errors;
use tidb_base::gostd;

use super::ldap_state::{LDAPSASLAuthImpl, LDAPSimpleAuthImpl};
use super::session::SessionVars;
use super::tidb_defs::*;
use super::tidb_vars::*;
use super::varsutil::*;

/// Go `MaskPwd` (sysvar.go).
pub const MaskPwd: &str = "******";

/// Go `logutil.DefaultRecordPlanInSlowLog` (go-dep).
const DefaultRecordPlanInSlowLog: i64 = 1;
/// Go `logutil.DefaultSlowThreshold` (go-dep).
const DefaultSlowThreshold: i64 = 300;

/// Clones the global config, applies `f` and stores the copy (see the module docs).
// glue: in-place write of a plain field of the global *Config (M-SNAP)
fn update_global_config(f: impl FnOnce(&mut config::Config)) {
    let mut c = (**config::get_global_config()).clone();
    f(&mut c);
    config::store_global_config(Arc::new(c));
}

/// `strconv.ParseInt(s, 10, 64)` with Go's error.
// go-std: strconv.ParseInt(s, 10, 64) L1
fn parse_int64(s: &str) -> Result<i64, Error> {
    let (v, err) = tidb_types::gostd::parse_int(s.as_bytes(), 64);
    match err {
        Ok(()) => Ok(v),
        Err(e) => Err(e.into_base()),
    }
}

/// `time.ParseDuration` with Go's error.
// go-std: time.ParseDuration L1
fn parse_duration(s: &str) -> Result<i64, Error> {
    gostd::time::parse_duration(s).map_err(errors::new)
}

// ---------------------------------------------------------------- switchDDL / switchStats

/// Go `EnableDDL` / `DisableDDL` / `EnableStatsOwner` / `DisableStatsOwner`: callbacks the
/// domain registers on a Go instance. tidb-rs runs neither DDL nor the stats owner, so they
/// are never registered (`nil` in Go), and the switches below do nothing.
// go: switchDDL L1
fn switch_ddl(_on: bool) -> Result<(), Error> {
    Ok(())
}

// go: switchStats L1
fn switch_stats(_on: bool) -> Result<(), Error> {
    Ok(())
}

// ---------------------------------------------------------------- instance config

// go-part: defaultSysVars (sysvar.go:L555 max_connections SetGlobal closure) L2
pub(super) fn sg_max_connections(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    let v = tidb_opt_int64(val, 0) as u32;
    update_global_config(|c| c.instance.max_connections = v);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L555 max_connections GetGlobal closure) L1
pub(super) fn gg_max_connections(_s: &mut SessionVars) -> Result<String, Error> {
    Ok((config::get_global_config().instance.max_connections as u64).to_string())
}

// go-part: defaultSysVars (sysvar.go:L491 tidb_check_mb4_value_in_utf8 SetGlobal closure) L1
pub(super) fn sg_tidb_check_mb4_value_in_utf8(
    _s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    config::get_global_config()
        .instance
        .check_mb4_value_in_utf8
        .store(tidb_opt_on(val));
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L491 tidb_check_mb4_value_in_utf8 GetGlobal closure) L1
pub(super) fn gg_tidb_check_mb4_value_in_utf8(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(bool_to_on_off(
        config::get_global_config()
            .instance
            .check_mb4_value_in_utf8
            .load(),
    )
    .to_owned())
}

// go-part: defaultSysVars (sysvar.go:L531 tidb_enable_collect_execution_info SetGlobal closure) L1
pub(super) fn sg_tidb_enable_collect_execution_info(
    _s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    let new_value = tidb_opt_on(val);
    if config::get_global_config()
        .instance
        .enable_collect_execution_info
        .load()
        != new_value
    {
        update_global_config(|c| c.instance.enable_collect_execution_info.store(new_value));
    }
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L531 tidb_enable_collect_execution_info GetGlobal closure) L1
pub(super) fn gg_tidb_enable_collect_execution_info(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(bool_to_on_off(
        config::get_global_config()
            .instance
            .enable_collect_execution_info
            .load(),
    )
    .to_owned())
}

// go-part: defaultSysVars (sysvar.go:L561 tidb_enable_ddl SetGlobal closure) L1
pub(super) fn sg_tidb_enable_ddl(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    let cfg = config::get_global_config();
    let (old_val, new_val) = (cfg.instance.tidb_enable_ddl.load(), tidb_opt_on(val));
    if old_val != new_val {
        switch_ddl(new_val)?;
        cfg.instance.tidb_enable_ddl.store(new_val);
    }
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L561 tidb_enable_ddl GetGlobal closure) L1
pub(super) fn gg_tidb_enable_ddl(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(bool_to_on_off(config::get_global_config().instance.tidb_enable_ddl.load()).to_owned())
}

// go-part: defaultSysVars (sysvar.go:L577 tidb_enable_stats_owner SetGlobal closure) L1
pub(super) fn sg_tidb_enable_stats_owner(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    let cfg = config::get_global_config();
    let (old_val, new_val) = (
        cfg.instance.tidb_enable_stats_owner.load(),
        tidb_opt_on(val),
    );
    if old_val != new_val {
        switch_stats(new_val)?;
        cfg.instance.tidb_enable_stats_owner.store(new_val);
    }
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L577 tidb_enable_stats_owner GetGlobal closure) L1
pub(super) fn gg_tidb_enable_stats_owner(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(bool_to_on_off(
        config::get_global_config()
            .instance
            .tidb_enable_stats_owner
            .load(),
    )
    .to_owned())
}

// go-part: defaultSysVars (sysvar.go:L485 tidb_enable_slow_log SetGlobal closure) L1
pub(super) fn sg_tidb_enable_slow_log(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    config::get_global_config()
        .instance
        .enable_slow_log
        .store(tidb_opt_on(val));
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L485 tidb_enable_slow_log GetGlobal closure) L1
pub(super) fn gg_tidb_enable_slow_log(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(bool_to_on_off(config::get_global_config().instance.enable_slow_log.load()).to_owned())
}

// go-part: defaultSysVars (sysvar.go:L478 tidb_record_plan_in_slow_log SetGlobal closure) L2
pub(super) fn sg_tidb_record_plan_in_slow_log(
    _s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    let v = tidb_opt_int64(val, DefaultRecordPlanInSlowLog) as u32;
    update_global_config(|c| c.instance.record_plan_in_slow_log = v);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L478 tidb_record_plan_in_slow_log GetGlobal closure) L1
pub(super) fn gg_tidb_record_plan_in_slow_log(_s: &mut SessionVars) -> Result<String, Error> {
    let enabled = config::get_global_config().instance.record_plan_in_slow_log == 1;
    Ok(bool_to_on_off(enabled).to_owned())
}

// go-part: defaultSysVars (sysvar.go:L472 tidb_slow_log_threshold SetGlobal closure) L2
pub(super) fn sg_tidb_slow_log_threshold(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    let v = tidb_opt_int64(val, DefaultSlowThreshold) as u64;
    update_global_config(|c| c.instance.slow_threshold = v);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L472 tidb_slow_log_threshold GetGlobal closure) L1
pub(super) fn gg_tidb_slow_log_threshold(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(config::get_global_config()
        .instance
        .slow_threshold
        .to_string())
}

// go-part: defaultSysVars (sysvar.go:L3533 tidb_service_scope SetGlobal closure) L2
pub(super) fn sg_tidb_service_scope(_s: &mut SessionVars, s: &str) -> Result<(), Error> {
    let new_value = gostd::to_lower(s);
    ServiceScope.store(new_value.clone());
    if config::get_global_config().instance.tidb_service_scope != new_value {
        update_global_config(|c| c.instance.tidb_service_scope = new_value);
    }
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L546 plugin_dir GetGlobal closure) L1
pub(super) fn gg_plugin_dir(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(config::get_global_config().instance.plugin_dir.clone())
}

// go-part: defaultSysVars (sysvar.go:L543 plugin_load GetGlobal closure) L1
pub(super) fn gg_plugin_load(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(config::get_global_config().instance.plugin_load.clone())
}

// go-part: defaultSysVars (sysvar.go:L549 plugin_audit_log_buffer_size GetGlobal closure) L1
pub(super) fn gg_plugin_audit_log_buffer_size(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(config::get_global_config()
        .instance
        .plugin_audit_log_buffer_size
        .to_string())
}

// go-part: defaultSysVars (sysvar.go:L552 plugin_audit_log_flush_interval GetGlobal closure) L1
pub(super) fn gg_plugin_audit_log_flush_interval(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(config::get_global_config()
        .instance
        .plugin_audit_log_flush_interval
        .to_string())
}

// go-part: defaultSysVars (sysvar.go:L599 tidb_stmt_summary_enable_persistent GetGlobal closure) L1
pub(super) fn gg_tidb_stmt_summary_enable_persistent(
    _s: &mut SessionVars,
) -> Result<String, Error> {
    Ok(bool_to_on_off(
        config::get_global_config()
            .instance
            .stmt_summary_enable_persistent,
    )
    .to_owned())
}

// go-part: defaultSysVars (sysvar.go:L602 tidb_stmt_summary_filename GetGlobal closure) L1
pub(super) fn gg_tidb_stmt_summary_filename(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(config::get_global_config()
        .instance
        .stmt_summary_filename
        .clone())
}

// go-part: defaultSysVars (sysvar.go:L605 tidb_stmt_summary_file_max_days GetGlobal closure) L1
pub(super) fn gg_tidb_stmt_summary_file_max_days(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(config::get_global_config()
        .instance
        .stmt_summary_file_max_days
        .to_string())
}

// go-part: defaultSysVars (sysvar.go:L608 tidb_stmt_summary_file_max_size GetGlobal closure) L1
pub(super) fn gg_tidb_stmt_summary_file_max_size(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(config::get_global_config()
        .instance
        .stmt_summary_file_max_size
        .to_string())
}

// go-part: defaultSysVars (sysvar.go:L611 tidb_stmt_summary_file_max_backups GetGlobal closure) L1
pub(super) fn gg_tidb_stmt_summary_file_max_backups(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(config::get_global_config()
        .instance
        .stmt_summary_file_max_backups
        .to_string())
}

// ---------------------------------------------------------------- variable package

// go-part: defaultSysVars (sysvar.go:L497 tidb_pprof_sql_cpu SetGlobal closure) L1
pub(super) fn sg_tidb_pprof_sql_cpu(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    EnablePProfSQLCPU.store(
        (tidb_opt_positive_int32(val, DefTiDBPProfSQLCPU as isize) as u32) > 0,
        SeqCst,
    );
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L497 tidb_pprof_sql_cpu GetGlobal closure) L1
pub(super) fn gg_tidb_pprof_sql_cpu(_s: &mut SessionVars) -> Result<String, Error> {
    let mut val = "0";
    if EnablePProfSQLCPU.load(SeqCst) {
        val = "1";
    }
    Ok(val.to_owned())
}

// go-part: defaultSysVars (sysvar.go:L513 tidb_force_priority SetGlobal closure) L1
pub(super) fn sg_tidb_force_priority(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    ForcePriority.store(tidb_base::mysql::str2_priority(val).0 as i32, SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L513 tidb_force_priority GetGlobal closure) L1
pub(super) fn gg_tidb_force_priority(_s: &mut SessionVars) -> Result<String, Error> {
    let p = tidb_base::mysql::PriorityEnum(ForcePriority.load(SeqCst) as _);
    // A missing key reads the zero value "" (Go map index).
    Ok(tidb_base::mysql::Priority2Str
        .get(&p)
        .map(|s| (*s).to_owned())
        .unwrap_or_default())
}

// go-part: defaultSysVars (sysvar.go:L735 disconnect_on_expired_password GetGlobal closure) L1
pub(super) fn gg_disconnect_on_expired_password(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(bool_to_on_off(!IsSandBoxModeEnabled.load(SeqCst)).to_owned())
}

// go-part: defaultSysVars (sysvar.go:L1262 tidb_enable_column_tracking SetGlobal closure) L1
pub(super) fn sg_tidb_enable_column_tracking(
    _s: &mut SessionVars,
    _val: &str,
) -> Result<(), Error> {
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1262 tidb_enable_column_tracking GetGlobal closure) L1
pub(super) fn gg_tidb_enable_column_tracking(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(bool_to_on_off(true).to_owned())
}

// go-part: defaultSysVars (sysvar.go:L1559 tidb_memory_usage_alarm_ratio GetGlobal closure) L1
pub(super) fn gg_tidb_memory_usage_alarm_ratio(_s: &mut SessionVars) -> Result<String, Error> {
    // fmt.Sprintf("%g", f) is strconv.FormatFloat(f, 'g', -1, 64).
    Ok(gostd::strconv::format_float(
        MemoryUsageAlarmRatio.load(),
        b'g',
        -1,
        64,
    ))
}

// go-part: defaultSysVars (sysvar.go:L1592 tidb_historical_stats_duration SetGlobal closure) L1
pub(super) fn sg_tidb_historical_stats_duration(
    _s: &mut SessionVars,
    s: &str,
) -> Result<(), Error> {
    let d = parse_duration(s)?;
    HistoricalStatsDuration.store(d, SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1592 tidb_historical_stats_duration GetGlobal closure) L1
pub(super) fn gg_tidb_historical_stats_duration(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(gostd::time::duration_string(
        HistoricalStatsDuration.load(SeqCst),
    ))
}

// go-part: defaultSysVars (sysvar.go:L3625 tidb_advancer_check_point_lag_limit SetGlobal closure) L1
pub(super) fn sg_tidb_advancer_check_point_lag_limit(
    _s: &mut SessionVars,
    s: &str,
) -> Result<(), Error> {
    let d = parse_duration(s)?;
    AdvancerCheckPointLagLimit.store(d, SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3625 tidb_advancer_check_point_lag_limit GetGlobal closure) L1
pub(super) fn gg_tidb_advancer_check_point_lag_limit(
    _s: &mut SessionVars,
) -> Result<String, Error> {
    Ok(gostd::time::duration_string(
        AdvancerCheckPointLagLimit.load(SeqCst),
    ))
}

// ---------------------------------------------------------------- memory package

// go-part: defaultSysVars (sysvar.go:L1545 tidb_enable_gc_aware_memory_track SetGlobal closure) L1
pub(super) fn sg_tidb_enable_gc_aware_memory_track(
    _s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    tidb_memory::tracker::EnableGCAwareMemoryTrack.store(tidb_opt_on(val), SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1545 tidb_enable_gc_aware_memory_track GetGlobal closure) L1
pub(super) fn gg_tidb_enable_gc_aware_memory_track(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(bool_to_on_off(tidb_memory::tracker::EnableGCAwareMemoryTrack.load(SeqCst)).to_owned())
}

// go-part: defaultSysVars (sysvar.go:L1200 tidb_server_memory_limit_sess_min_size SetGlobal closure) L1
pub(super) fn sg_tidb_server_memory_limit_sess_min_size(
    _s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    let int_val = gostd::strconv::parse_uint10(val.as_bytes()).map_err(errors::new)?;
    tidb_memory::tracker::ServerMemoryLimitSessMinSize.store(int_val, SeqCst);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1200 tidb_server_memory_limit_sess_min_size GetGlobal closure) L1
pub(super) fn gg_tidb_server_memory_limit_sess_min_size(
    _s: &mut SessionVars,
) -> Result<String, Error> {
    Ok(tidb_memory::tracker::ServerMemoryLimitSessMinSize
        .load(SeqCst)
        .to_string())
}

// go-part: defaultSysVars (sysvar.go:L1176 tidb_server_memory_limit GetGlobal closure) L1
pub(super) fn gg_tidb_server_memory_limit(_s: &mut SessionVars) -> Result<String, Error> {
    Ok((**tidb_memory::tracker::ServerMemoryLimitOriginText.load()).clone())
}

// ---------------------------------------------------------------- LDAP plugins

// go-part: defaultSysVars (sysvar.go:L3252 authentication_ldap_sasl_auth_method_name SetGlobal closure) L1
pub(super) fn sg_authentication_ldap_sasl_auth_method_name(
    _s: &mut SessionVars,
    s: &str,
) -> Result<(), Error> {
    LDAPSASLAuthImpl.set_sasl_auth_method(s);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3252 authentication_ldap_sasl_auth_method_name GetGlobal closure) L1
pub(super) fn gg_authentication_ldap_sasl_auth_method_name(
    _s: &mut SessionVars,
) -> Result<String, Error> {
    Ok(LDAPSASLAuthImpl.get_sasl_auth_method())
}

// go-part: defaultSysVars (sysvar.go:L3258 authentication_ldap_sasl_server_host SetGlobal closure) L1
pub(super) fn sg_authentication_ldap_sasl_server_host(
    _s: &mut SessionVars,
    s: &str,
) -> Result<(), Error> {
    LDAPSASLAuthImpl.set_ldap_server_host(s);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3258 authentication_ldap_sasl_server_host GetGlobal closure) L1
pub(super) fn gg_authentication_ldap_sasl_server_host(
    _s: &mut SessionVars,
) -> Result<String, Error> {
    Ok(LDAPSASLAuthImpl.get_ldap_server_host())
}

// go-part: defaultSysVars (sysvar.go:L3265 authentication_ldap_sasl_server_port SetGlobal closure) L1
pub(super) fn sg_authentication_ldap_sasl_server_port(
    _s: &mut SessionVars,
    s: &str,
) -> Result<(), Error> {
    let val = parse_int64(s)?;
    LDAPSASLAuthImpl.set_ldap_server_port(val as isize);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3265 authentication_ldap_sasl_server_port GetGlobal closure) L1
pub(super) fn gg_authentication_ldap_sasl_server_port(
    _s: &mut SessionVars,
) -> Result<String, Error> {
    Ok(LDAPSASLAuthImpl.get_ldap_server_port().to_string())
}

// go-part: defaultSysVars (sysvar.go:L3275 authentication_ldap_sasl_tls SetGlobal closure) L1
pub(super) fn sg_authentication_ldap_sasl_tls(_s: &mut SessionVars, s: &str) -> Result<(), Error> {
    LDAPSASLAuthImpl.set_enable_tls(tidb_opt_on(s));
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3275 authentication_ldap_sasl_tls GetGlobal closure) L1
pub(super) fn gg_authentication_ldap_sasl_tls(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(bool_to_on_off(LDAPSASLAuthImpl.get_enable_tls()).to_owned())
}

// go-part: defaultSysVars (sysvar.go:L3281 authentication_ldap_sasl_ca_path SetGlobal closure) L1
pub(super) fn sg_authentication_ldap_sasl_ca_path(
    _s: &mut SessionVars,
    s: &str,
) -> Result<(), Error> {
    LDAPSASLAuthImpl.set_ca_path(s)
}

// go-part: defaultSysVars (sysvar.go:L3281 authentication_ldap_sasl_ca_path GetGlobal closure) L1
pub(super) fn gg_authentication_ldap_sasl_ca_path(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(LDAPSASLAuthImpl.get_ca_path())
}

// go-part: defaultSysVars (sysvar.go:L3286 authentication_ldap_sasl_user_search_attr SetGlobal closure) L1
pub(super) fn sg_authentication_ldap_sasl_user_search_attr(
    _s: &mut SessionVars,
    s: &str,
) -> Result<(), Error> {
    LDAPSASLAuthImpl.set_search_attr(s);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3286 authentication_ldap_sasl_user_search_attr GetGlobal closure) L1
pub(super) fn gg_authentication_ldap_sasl_user_search_attr(
    _s: &mut SessionVars,
) -> Result<String, Error> {
    Ok(LDAPSASLAuthImpl.get_search_attr())
}

// go-part: defaultSysVars (sysvar.go:L3293 authentication_ldap_sasl_bind_base_dn SetGlobal closure) L1
pub(super) fn sg_authentication_ldap_sasl_bind_base_dn(
    _s: &mut SessionVars,
    s: &str,
) -> Result<(), Error> {
    LDAPSASLAuthImpl.set_bind_base_dn(s);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3293 authentication_ldap_sasl_bind_base_dn GetGlobal closure) L1
pub(super) fn gg_authentication_ldap_sasl_bind_base_dn(
    _s: &mut SessionVars,
) -> Result<String, Error> {
    Ok(LDAPSASLAuthImpl.get_bind_base_dn())
}

// go-part: defaultSysVars (sysvar.go:L3299 authentication_ldap_sasl_bind_root_dn SetGlobal closure) L1
pub(super) fn sg_authentication_ldap_sasl_bind_root_dn(
    _s: &mut SessionVars,
    s: &str,
) -> Result<(), Error> {
    LDAPSASLAuthImpl.set_bind_root_dn(s);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3299 authentication_ldap_sasl_bind_root_dn GetGlobal closure) L1
pub(super) fn gg_authentication_ldap_sasl_bind_root_dn(
    _s: &mut SessionVars,
) -> Result<String, Error> {
    Ok(LDAPSASLAuthImpl.get_bind_root_dn())
}

// go-part: defaultSysVars (sysvar.go:L3305 authentication_ldap_sasl_bind_root_pwd SetGlobal closure) L1
pub(super) fn sg_authentication_ldap_sasl_bind_root_pwd(
    _s: &mut SessionVars,
    s: &str,
) -> Result<(), Error> {
    LDAPSASLAuthImpl.set_bind_root_pw(s);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3305 authentication_ldap_sasl_bind_root_pwd GetGlobal closure) L1
pub(super) fn gg_authentication_ldap_sasl_bind_root_pwd(
    _s: &mut SessionVars,
) -> Result<String, Error> {
    if LDAPSASLAuthImpl.get_bind_root_pw().is_empty() {
        return Ok(String::new());
    }
    Ok(MaskPwd.to_owned())
}

// go-part: defaultSysVars (sysvar.go:L3315 authentication_ldap_sasl_init_pool_size SetGlobal closure) L1
pub(super) fn sg_authentication_ldap_sasl_init_pool_size(
    _s: &mut SessionVars,
    s: &str,
) -> Result<(), Error> {
    let val = parse_int64(s)?;
    LDAPSASLAuthImpl.set_init_capacity(val as isize);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3315 authentication_ldap_sasl_init_pool_size GetGlobal closure) L1
pub(super) fn gg_authentication_ldap_sasl_init_pool_size(
    _s: &mut SessionVars,
) -> Result<String, Error> {
    Ok(LDAPSASLAuthImpl.get_init_capacity().to_string())
}

// go-part: defaultSysVars (sysvar.go:L3325 authentication_ldap_sasl_max_pool_size SetGlobal closure) L1
pub(super) fn sg_authentication_ldap_sasl_max_pool_size(
    _s: &mut SessionVars,
    s: &str,
) -> Result<(), Error> {
    let val = parse_int64(s)?;
    LDAPSASLAuthImpl.set_max_capacity(val as isize);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3325 authentication_ldap_sasl_max_pool_size GetGlobal closure) L1
pub(super) fn gg_authentication_ldap_sasl_max_pool_size(
    _s: &mut SessionVars,
) -> Result<String, Error> {
    Ok(LDAPSASLAuthImpl.get_max_capacity().to_string())
}

// go-part: defaultSysVars (sysvar.go:L3343 authentication_ldap_simple_server_host SetGlobal closure) L1
pub(super) fn sg_authentication_ldap_simple_server_host(
    _s: &mut SessionVars,
    s: &str,
) -> Result<(), Error> {
    LDAPSimpleAuthImpl.set_ldap_server_host(s);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3343 authentication_ldap_simple_server_host GetGlobal closure) L1
pub(super) fn gg_authentication_ldap_simple_server_host(
    _s: &mut SessionVars,
) -> Result<String, Error> {
    Ok(LDAPSimpleAuthImpl.get_ldap_server_host())
}

// go-part: defaultSysVars (sysvar.go:L3350 authentication_ldap_simple_server_port SetGlobal closure) L1
pub(super) fn sg_authentication_ldap_simple_server_port(
    _s: &mut SessionVars,
    s: &str,
) -> Result<(), Error> {
    let val = parse_int64(s)?;
    LDAPSimpleAuthImpl.set_ldap_server_port(val as isize);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3350 authentication_ldap_simple_server_port GetGlobal closure) L1
pub(super) fn gg_authentication_ldap_simple_server_port(
    _s: &mut SessionVars,
) -> Result<String, Error> {
    Ok(LDAPSimpleAuthImpl.get_ldap_server_port().to_string())
}

// go-part: defaultSysVars (sysvar.go:L3360 authentication_ldap_simple_tls SetGlobal closure) L1
pub(super) fn sg_authentication_ldap_simple_tls(
    _s: &mut SessionVars,
    s: &str,
) -> Result<(), Error> {
    LDAPSimpleAuthImpl.set_enable_tls(tidb_opt_on(s));
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3360 authentication_ldap_simple_tls GetGlobal closure) L1
pub(super) fn gg_authentication_ldap_simple_tls(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(bool_to_on_off(LDAPSimpleAuthImpl.get_enable_tls()).to_owned())
}

// go-part: defaultSysVars (sysvar.go:L3366 authentication_ldap_simple_ca_path SetGlobal closure) L1
pub(super) fn sg_authentication_ldap_simple_ca_path(
    _s: &mut SessionVars,
    s: &str,
) -> Result<(), Error> {
    LDAPSimpleAuthImpl.set_ca_path(s)
}

// go-part: defaultSysVars (sysvar.go:L3366 authentication_ldap_simple_ca_path GetGlobal closure) L1
pub(super) fn gg_authentication_ldap_simple_ca_path(_s: &mut SessionVars) -> Result<String, Error> {
    Ok(LDAPSimpleAuthImpl.get_ca_path())
}

// go-part: defaultSysVars (sysvar.go:L3371 authentication_ldap_simple_user_search_attr SetGlobal closure) L1
pub(super) fn sg_authentication_ldap_simple_user_search_attr(
    _s: &mut SessionVars,
    s: &str,
) -> Result<(), Error> {
    LDAPSimpleAuthImpl.set_search_attr(s);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3371 authentication_ldap_simple_user_search_attr GetGlobal closure) L1
pub(super) fn gg_authentication_ldap_simple_user_search_attr(
    _s: &mut SessionVars,
) -> Result<String, Error> {
    Ok(LDAPSimpleAuthImpl.get_search_attr())
}

// go-part: defaultSysVars (sysvar.go:L3378 authentication_ldap_simple_bind_base_dn SetGlobal closure) L1
pub(super) fn sg_authentication_ldap_simple_bind_base_dn(
    _s: &mut SessionVars,
    s: &str,
) -> Result<(), Error> {
    LDAPSimpleAuthImpl.set_bind_base_dn(s);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3378 authentication_ldap_simple_bind_base_dn GetGlobal closure) L1
pub(super) fn gg_authentication_ldap_simple_bind_base_dn(
    _s: &mut SessionVars,
) -> Result<String, Error> {
    Ok(LDAPSimpleAuthImpl.get_bind_base_dn())
}

// go-part: defaultSysVars (sysvar.go:L3384 authentication_ldap_simple_bind_root_dn SetGlobal closure) L1
pub(super) fn sg_authentication_ldap_simple_bind_root_dn(
    _s: &mut SessionVars,
    s: &str,
) -> Result<(), Error> {
    LDAPSimpleAuthImpl.set_bind_root_dn(s);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3384 authentication_ldap_simple_bind_root_dn GetGlobal closure) L1
pub(super) fn gg_authentication_ldap_simple_bind_root_dn(
    _s: &mut SessionVars,
) -> Result<String, Error> {
    Ok(LDAPSimpleAuthImpl.get_bind_root_dn())
}

// go-part: defaultSysVars (sysvar.go:L3390 authentication_ldap_simple_bind_root_pwd SetGlobal closure) L1
pub(super) fn sg_authentication_ldap_simple_bind_root_pwd(
    _s: &mut SessionVars,
    s: &str,
) -> Result<(), Error> {
    LDAPSimpleAuthImpl.set_bind_root_pw(s);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3390 authentication_ldap_simple_bind_root_pwd GetGlobal closure) L1
pub(super) fn gg_authentication_ldap_simple_bind_root_pwd(
    _s: &mut SessionVars,
) -> Result<String, Error> {
    if LDAPSimpleAuthImpl.get_bind_root_pw().is_empty() {
        return Ok(String::new());
    }
    Ok(MaskPwd.to_owned())
}

// go-part: defaultSysVars (sysvar.go:L3400 authentication_ldap_simple_init_pool_size SetGlobal closure) L1
pub(super) fn sg_authentication_ldap_simple_init_pool_size(
    _s: &mut SessionVars,
    s: &str,
) -> Result<(), Error> {
    let val = parse_int64(s)?;
    LDAPSimpleAuthImpl.set_init_capacity(val as isize);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3400 authentication_ldap_simple_init_pool_size GetGlobal closure) L1
pub(super) fn gg_authentication_ldap_simple_init_pool_size(
    _s: &mut SessionVars,
) -> Result<String, Error> {
    Ok(LDAPSimpleAuthImpl.get_init_capacity().to_string())
}

// go-part: defaultSysVars (sysvar.go:L3410 authentication_ldap_simple_max_pool_size SetGlobal closure) L1
pub(super) fn sg_authentication_ldap_simple_max_pool_size(
    _s: &mut SessionVars,
    s: &str,
) -> Result<(), Error> {
    let val = parse_int64(s)?;
    LDAPSimpleAuthImpl.set_max_capacity(val as isize);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3410 authentication_ldap_simple_max_pool_size GetGlobal closure) L1
pub(super) fn gg_authentication_ldap_simple_max_pool_size(
    _s: &mut SessionVars,
) -> Result<String, Error> {
    Ok(LDAPSimpleAuthImpl.get_max_capacity().to_string())
}

// go-part: defaultSysVars (sysvar.go:L3335 authentication_ldap_simple_auth_method_name SetGlobal closure) L1
pub(super) fn sg_authentication_ldap_simple_auth_method_name(
    _s: &mut SessionVars,
    s: &str,
) -> Result<(), Error> {
    let s = gostd::to_upper(s);
    // Only "SIMPLE" is supported
    if s != "SIMPLE" {
        return Err(errors::errorf(
            "auth method %s is not supported",
            &[tidb_base::gofmt::Arg::from(s)],
        ));
    }
    Ok(())
}
