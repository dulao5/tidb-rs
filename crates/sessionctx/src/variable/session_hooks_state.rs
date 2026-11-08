//! go-file: pkg/sessionctx/variable/sysvar.go
//! go-file: pkg/sessionctx/variable/varsutil.go
//!
//! The session hooks that read or write the non-scalar state of `SessionVars` (D05.2 T4/T6),
//! and the varsutil helpers they call (`setSnapshotTS`, `setTxnReadTS`, `setReadStaleness`,
//! `parseTSFromNumberOrTime`). Registered in `session_hooks.rs`.

#![allow(non_upper_case_globals)]

use std::sync::Arc;
use std::sync::atomic::Ordering::SeqCst;

use parking_lot::Mutex;
use tidb_base::Error;
use tidb_base::contextutil::WarnAppender;
use tidb_base::kv::{TiDB, TiFlash, TiKV};
use tidb_base::mysql;

use super::error::{ErrWarnDeprecatedSyntaxSimpleMsg, ErrWrongValueForVar};
use super::session::SessionVars;
use super::session_state::*;
use super::tidb_defs::*;
use super::tidb_vars::*;
use super::varsutil::*;

// ---------------------------------------------------------------- varsutil helpers

/// The warning sink of the statement's TypeCtx (Go: the StatementContext itself).
// glue: collects the warnings of a types.Context into the session's StmtCtx
#[derive(Default)]
struct WarnCollector(Mutex<Vec<Error>>);

impl WarnAppender for WarnCollector {
    // glue: StatementContext.AppendWarning through the TypeCtx
    fn append_warning(&self, err: Error) {
        self.0.lock().push(err);
    }
    // glue: StatementContext.AppendNote through the TypeCtx (recorded as a warning here)
    fn append_note(&self, msg: Error) {
        self.0.lock().push(msg);
    }
}

/// `types.ParseTime(s.StmtCtx.TypeCtx(), sVal, mysql.TypeTimestamp, types.MaxFsp)` and
/// `t.GoTime(s.Location())`, then `oracle.GoTimeToTS`. A fresh StmtCtx's TypeCtx has the
/// default statement flags and UTC; its warnings go to the StmtCtx.
// glue: the common part of parseTSFromNumberOrTime / setTxnReadTS
fn parse_timestamp_to_ts(s: &mut SessionVars, s_val: &str) -> Result<u64, Error> {
    let collector = Arc::new(WarnCollector::default());
    let ctx = tidb_types::context::new_context(
        tidb_types::context::DefaultStmtFlags,
        tidb_base::gostd::time::utc(),
        collector.clone(),
    );
    let (t, err) = tidb_types::time::parse_time(
        &ctx,
        s_val.as_bytes(),
        mysql::TypeTimestamp,
        tidb_types::fsp::MaxFsp,
    );
    for w in collector.0.lock().drain(..) {
        s.stmt_ctx.append_warning(w);
    }
    err?;
    let (t1, err) = t.go_time(&s.location());
    let ts = go_time_to_ts(&t1);
    err.map(|()| ts)
}

/// oracle.GoTimeToTS converts a Go time to uint64 timestamp.
// go-dep: github.com/tikv/client-go/v2/oracle.GoTimeToTS L1
fn go_time_to_ts(t: &tidb_base::gostd::time::Time) -> u64 {
    let unix_nano = t
        .unix()
        .wrapping_mul(1_000_000_000)
        .wrapping_add(t.nanosecond());
    let physical = unix_nano / 1_000_000;
    (physical << 18) as u64
}

/// setSnapshotTS
// go: setSnapshotTS L1
pub(crate) fn set_snapshot_ts(s: &mut SessionVars, s_val: &str) -> Result<(), Error> {
    if s_val.is_empty() {
        s.snapshot_ts = 0;
        s.state.snapshot_infoschema = false;
        return Ok(());
    }
    if s.read_staleness != 0 {
        return Err(tidb_base::errors::new(
            "tidb_read_staleness should be clear before setting tidb_snapshot",
        ));
    }
    let (tso, err) = match parse_ts_from_number_or_time(s, s_val) {
        Ok(t) => (t, Ok(())),
        Err(e) => (0, Err(e)),
    };
    s.snapshot_ts = tso;
    // tx_read_ts should be mutual exclusive with tidb_snapshot
    s.state.txn_read_ts = TxnReadTS::new(0);
    err
}

/// parseTSFromNumberOrTime
// go: parseTSFromNumberOrTime L2 (on a GoTime error Go still returns the converted ts; the caller keeps it)
pub(crate) fn parse_ts_from_number_or_time(s: &mut SessionVars, s_val: &str) -> Result<u64, Error> {
    if let Ok(tso) = super::session_hooks::go_parse_uint(s_val) {
        return Ok(tso);
    }
    parse_timestamp_to_ts(s, s_val)
}

/// setTxnReadTS
// go: setTxnReadTS L1
pub(crate) fn set_txn_read_ts(s: &mut SessionVars, s_val: &str) -> Result<(), Error> {
    if s_val.is_empty() {
        s.state.txn_read_ts = TxnReadTS::new(0);
        return Ok(());
    }
    let ts = parse_timestamp_to_ts(s, s_val)?;
    s.state.txn_read_ts = TxnReadTS::new(ts);
    // tx_read_ts should be mutual exclusive with tidb_snapshot
    s.snapshot_ts = 0;
    s.state.snapshot_infoschema = false;
    Ok(())
}

/// setReadStaleness
// go: setReadStaleness L1
pub(crate) fn set_read_staleness(s: &mut SessionVars, s_val: &str) -> Result<(), Error> {
    if s_val.is_empty() || s_val == "0" {
        s.read_staleness = 0;
        return Ok(());
    }
    if s.snapshot_ts != 0 {
        return Err(tidb_base::errors::new(
            "tidb_snapshot should be clear before setting tidb_read_staleness",
        ));
    }
    let (s_value, err) = tidb_types::gostd::parse_int(s_val.as_bytes(), 32);
    if let Err(e) = err {
        return Err(e.into_base());
    }
    s.read_staleness = s_value * 1_000_000_000;
    Ok(())
}

// ---------------------------------------------------------------- SetSession

// go-part: defaultSysVars (sysvar.go:L130 txn_scope SetSession closure) L1
pub(super) fn ss_txn_scope(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    match val {
        tidb_kv::txn_scope_var::GlobalTxnScope => {
            s.state.txn_scope = tidb_kv::txn_scope_var::new_global_txn_scope_var();
        }
        tidb_kv::txn_scope_var::LocalTxnScope => {
            if !EnableLocalTxn.load(SeqCst) {
                return Err(ErrWrongValueForVar.gen_with_stack(
                    "@@txn_scope can not be set to local when tidb_enable_local_txn is off",
                    vec![],
                ));
            }
            let txn_scope = tidb_base::config::get_txn_scope_from_config();
            if txn_scope == tidb_kv::txn_scope_var::GlobalTxnScope {
                return Err(ErrWrongValueForVar.gen_with_stack(
                    "@@txn_scope can not be set to local when zone label is empty or \"global\"",
                    vec![],
                ));
            }
            s.state.txn_scope = tidb_kv::txn_scope_var::new_local_txn_scope_var(&txn_scope);
        }
        _ => {
            return Err(ErrWrongValueForVar
                .gen_with_stack("@@txn_scope value should be global or local", vec![]));
        }
    }
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L150 tx_read_ts SetSession closure) L1
pub(super) fn ss_tx_read_ts(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    set_txn_read_ts(s, val)
}

// go-part: defaultSysVars (sysvar.go:L155 tidb_read_staleness SetSession closure) L1
pub(super) fn ss_tidb_read_staleness(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    set_read_staleness(s, val)
}

// go-part: defaultSysVars (sysvar.go:L209 tidb_snapshot SetSession closure) L1
pub(super) fn ss_tidb_snapshot(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    set_snapshot_ts(s, val)
}

// go-part: defaultSysVars (sysvar.go:L284 tx_isolation_one_shot SetSession closure) L1
pub(super) fn ss_tx_isolation_one_shot(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.state.txn_isolation_level_one_shot.state = OneShotState::OneShotSet;
    s.state.txn_isolation_level_one_shot.value = val.to_owned();
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L358 tidb_isolation_read_engines SetSession closure) L1
pub(super) fn ss_tidb_isolation_read_engines(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.state.isolation_read_engines.clear();
    for engine in val.split(',') {
        if engine == TiKV.name() {
            s.state.isolation_read_engines.insert(TiKV);
        } else if engine == TiFlash.name() {
            // If the tiflash is removed by the strict SQL mode. The hint should also not take effect.
            if !s.stmt_ctx.tiflash_engine_removed_due_to_strict_sql_mode {
                s.state.isolation_read_engines.insert(TiFlash);
            }
        } else if engine == TiDB.name() {
            s.state.isolation_read_engines.insert(TiDB);
        }
    }
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L393 rand_seed1 SetSession closure) L1
pub(super) fn ss_rand_seed1(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.state
        .rng
        .set_seed1(tidb_opt_positive_int32(val, 0) as u32);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L401 rand_seed2 SetSession closure) L1
pub(super) fn ss_rand_seed2(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.state
        .rng
        .set_seed2(tidb_opt_positive_int32(val, 0) as u32);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L843 tidb_scatter_region SetSession closure) L1
pub(super) fn ss_tidb_scatter_region(vars: &mut SessionVars, val: &str) -> Result<(), Error> {
    vars.scatter_region = val.to_owned();
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1653 tidb_row_format_version SetSession closure) L1
pub(super) fn ss_tidb_row_format_version(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    let format_version = tidb_opt_int64(val, DefTiDBRowFormatV1);
    if format_version == DefTiDBRowFormatV1 {
        s.state.row_encoder_enable = false;
    } else if format_version == super::tidb_vars::DefTiDBRowFormatV2 {
        s.state.row_encoder_enable = true;
    }
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1694 sql_mode SetSession closure) L1
pub(super) fn ss_sql_mode(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    let val = mysql::format_sql_mode_str(val);
    // Modes is a list of different modes separated by commas.
    let (sql_mode, err) = mysql::get_sql_mode(&val);
    if let Err(e) = err {
        return Err(tidb_base::errors::new(e.error()));
    }
    s.sql_mode = sql_mode.0;
    s.set_status_flag(
        mysql::ServerStatusNoBackslashEscaped,
        sql_mode.has_no_backslash_escapes_mode(),
    );
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1776 time_zone SetSession closure) L1
pub(super) fn ss_time_zone(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    let tz = super::util::parse_time_zone(val)?;
    s.time_zone = Some(tz);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1848 autocommit SetSession closure) L1
pub(super) fn ss_autocommit(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    let is_autocommit = tidb_opt_on(val);
    // Implicitly commit the possible ongoing transaction if mode is changed from off to on.
    if !s.is_autocommit() && is_autocommit {
        s.set_in_txn(false);
    }
    s.set_status_flag(mysql::ServerStatusAutocommit, is_autocommit);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L1897 group_concat_max_len SetSession closure) L1
pub(super) fn ss_group_concat_max_len(sv: &mut SessionVars, s: &str) -> Result<(), Error> {
    // Go assigns ParseUint's value even on error: 0 on a syntax error, the max on a range error.
    match super::session_hooks::go_parse_uint(s) {
        Ok(v) => {
            sv.group_concat_max_len = v;
            Ok(())
        }
        Err(e) => {
            let range = !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
            sv.group_concat_max_len = if range { u64::MAX } else { 0 };
            Err(e)
        }
    }
}

// go-part: defaultSysVars (sysvar.go:L2317 tidb_backoff_lock_fast SetSession closure) L1
pub(super) fn ss_tidb_backoff_lock_fast(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.state.kv_vars.backoff_lock_fast =
        tidb_opt_positive_int32(val, DefBackoffLockFast as isize) as i32;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2321 tidb_backoff_weight SetSession closure) L1
pub(super) fn ss_tidb_backoff_weight(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.state.kv_vars.back_off_weight =
        tidb_opt_positive_int32(val, DefBackOffWeight as isize) as i32;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2422 tidb_allow_fallback_to_tikv SetSession closure) L1
pub(super) fn ss_tidb_allow_fallback_to_tikv(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.state.allow_fallback_to_tikv.clear();
    for engine in val.split(',') {
        if engine == TiFlash.name() {
            s.state.allow_fallback_to_tikv.insert(TiFlash);
        }
    }
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2479 tidb_replica_read SetSession closure) L1
pub(super) fn ss_tidb_replica_read(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    use tidb_kv::option::*;
    let eq = tidb_base::gostd::equal_fold;
    if eq(val, "follower") {
        s.set_replica_read(ReplicaReadFollower);
    } else if eq(val, "leader-and-follower") {
        s.set_replica_read(ReplicaReadMixed);
    } else if eq(val, "leader") || val.is_empty() {
        s.set_replica_read(ReplicaReadLeader);
    } else if eq(val, "closest-replicas") {
        s.set_replica_read(ReplicaReadClosest);
    } else if eq(val, "closest-adaptive") {
        s.set_replica_read(ReplicaReadClosestAdaptive);
    } else if eq(val, "learner") {
        s.set_replica_read(ReplicaReadLearner);
    } else if eq(val, "prefer-leader") {
        s.set_replica_read(ReplicaReadPreferLeader);
    }
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2550 tidb_partition_prune_mode SetSession closure) L1
pub(super) fn ss_tidb_partition_prune_mode(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    let new_mode = val.trim().to_lowercase();
    if new_mode == "static" {
        s.stmt_ctx
            .append_warning(ErrWarnDeprecatedSyntaxSimpleMsg.fast_gen(
                "static prune mode is deprecated and will be removed in the future release.",
                vec![],
            ));
    }
    if s.state.partition_prune_mode == "static" && new_mode == "dynamic" {
        s.stmt_ctx.append_warning(tidb_base::errors::new(
            "Please analyze all partition tables again for consistency between partition and global stats",
        ));
        s.stmt_ctx.append_warning(tidb_base::errors::new(
            "Please avoid setting partition prune mode to dynamic at session level and set partition prune mode to dynamic at global level",
        ));
    }
    s.state.partition_prune_mode = new_mode;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2571 tidb_redact_log SetSession closure) L1
pub(super) fn ss_tidb_redact_log(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.enable_redact_log = val.to_owned();
    RedactLogEnabled.store(val.to_owned());
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2673 tidb_stats_load_sync_wait SetSession closure) L1
pub(super) fn ss_tidb_stats_load_sync_wait(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.state.stats_load_sync_wait = tidb_opt_int64(val, DefTiDBStatsLoadSyncWait);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2686 tidb_sysdate_is_now SetSession closure) L1
pub(super) fn ss_tidb_sysdate_is_now(vars: &mut SessionVars, s: &str) -> Result<(), Error> {
    vars.sysdate_is_now = tidb_opt_on(s);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2692 tidb_enable_parallel_hashagg_spill SetSession closure) L1
pub(super) fn ss_tidb_enable_parallel_hashagg_spill(
    vars: &mut SessionVars,
    s: &str,
) -> Result<(), Error> {
    vars.enable_parallel_hashagg_spill = tidb_opt_on(s);
    if !vars.enable_parallel_hashagg_spill {
        vars.stmt_ctx.append_warning(ErrWarnDeprecatedSyntaxSimpleMsg.fast_gen(
            "tidb_enable_parallel_hashagg_spill will be removed in the future and hash aggregate spill will be enabled by default.",
            vec![],
        ));
    }
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2720 tidb_ignore_prepared_cache_close_stmt SetSession closure) L1
pub(super) fn ss_tidb_ignore_prepared_cache_close_stmt(
    vars: &mut SessionVars,
    s: &str,
) -> Result<(), Error> {
    vars.ignore_prepared_cache_close_stmt = tidb_opt_on(s);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2732 tidb_enable_new_cost_interface SetSession closure) L1
pub(super) fn ss_tidb_enable_new_cost_interface(
    vars: &mut SessionVars,
    s: &str,
) -> Result<(), Error> {
    vars.enable_new_cost_interface = tidb_opt_on(s);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2738 tidb_cost_model_version SetSession closure) L1
pub(super) fn ss_tidb_cost_model_version(vars: &mut SessionVars, s: &str) -> Result<(), Error> {
    vars.cost_model_version = tidb_opt_int64(s, 1);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2744 tidb_index_join_double_read_penalty_cost_rate SetSession closure) L1
pub(super) fn ss_tidb_index_join_double_read_penalty_cost_rate(
    vars: &mut SessionVars,
    s: &str,
) -> Result<(), Error> {
    vars.index_join_double_read_penalty_cost_rate = tidb_opt_float64(s, 0.0);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2757 tidb_mem_quota_query SetSession closure) L1
pub(super) fn ss_tidb_mem_quota_query(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.mem_quota_query = tidb_opt_int64(val, super::tidb_vars::DefTiDBMemQuotaQuery);
    s.state.mem_tracker_bytes_limit = s.mem_quota_query;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2819 tidb_stats_update_during_ddl SetSession closure) L1
pub(super) fn ss_tidb_stats_update_during_ddl(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    if tidb_opt_on(val) && s.analyze_version == 1 {
        return Err(tidb_base::errors::new(
            "tidb_stats_update_during_ddl can only be enabled with tidb_analyze_version 2",
        ));
    }
    s.enable_ddl_analyze = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L2866 tidb_constraint_check_in_place_pessimistic SetSession closure) L2 (the LazyPessimisticUniqueCheckSetCount metric is not kept)
pub(super) fn ss_tidb_constraint_check_in_place_pessimistic(
    s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    s.constraint_check_in_place_pessimistic = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3033 mpp_exchange_compression_mode SetSession closure) L1
pub(super) fn ss_mpp_exchange_compression_mode(
    s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    s.mpp_exchange_compression_mode = to_exchange_compression_mode(val).0;
    if s.choose_mpp_version() == tidb_kv::mpp::MppVersionV0.0
        && s.mpp_exchange_compression_mode != ExchangeCompressionModeUnspecified
    {
        s.stmt_ctx.append_warning(tidb_base::errors::new(format!(
            "mpp exchange compression won't work under current mpp version {}",
            tidb_kv::mpp::MppVersionV0.0
        )));
    }
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3056 mpp_version SetSession closure) L1
pub(super) fn ss_mpp_version(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.mpp_version = to_mpp_version(val).0;
    Ok(())
}

/// setTiFlashComputeDispatchPolicy (the SetSession and SetGlobal hooks of tiflash_compute_dispatch_policy).
// go: setTiFlashComputeDispatchPolicy L1
pub(super) fn ss_tiflash_compute_dispatch_policy(
    s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    // tiflashcompute.GetDispatchPolicyByStr
    let p = match val {
        "consistent_hash" => 1,
        "round_robin" => 0,
        _ => {
            return Err(tidb_base::errors::new(format!(
                "unexpected tiflash_compute dispatch policy, expect [consistent_hash round_robin], got {val}"
            )));
        }
    };
    s.tiflash_compute_dispatch_policy = p;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3162 tidb_load_based_replica_read_threshold SetSession closure) L1
pub(super) fn ss_tidb_load_based_replica_read_threshold(
    s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    let d = tidb_base::gostd::time::parse_duration(val)
        .map_err(|e| tidb_base::errors::new(e.to_string()))?;
    s.load_based_replica_read_threshold = d;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3212 tidb_opt_fix_control SetSession closure) L1
pub(super) fn ss_tidb_opt_fix_control(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    let (new_map, warnings) = parse_to_map(val)?;
    for warning in warnings {
        s.stmt_ctx.append_warning(tidb_base::errors::new(warning));
    }
    s.state.optimizer_fix_control = new_map;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3227 tidb_analyze_skip_column_types SetSession closure) L1
pub(super) fn ss_tidb_analyze_skip_column_types(
    s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    s.state.analyze_skip_column_types = super::varsutil::parse_analyze_skip_column_types(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3236 tiflash_replica_read SetSession closure) L1
pub(super) fn ss_tiflash_replica_read(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.tiflash_replica_read = get_tiflash_replica_read_by_str(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3431 tidb_runtime_filter_type SetSession closure) L1
pub(super) fn ss_tidb_runtime_filter_type(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.state.runtime_filter_types = to_runtime_filter_type(val).0;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3446 tidb_runtime_filter_mode SetSession closure) L1
pub(super) fn ss_tidb_runtime_filter_mode(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    s.runtime_filter_mode = runtime_filter_mode_string_to_mode(val).0;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3456 tidb_lock_unchanged_keys SetSession closure) L1
pub(super) fn ss_tidb_lock_unchanged_keys(vars: &mut SessionVars, s: &str) -> Result<(), Error> {
    vars.lock_unchanged_keys = tidb_opt_on(s);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3516 tidb_session_alias SetSession closure) L1
pub(super) fn ss_tidb_session_alias(vars: &mut SessionVars, s: &str) -> Result<(), Error> {
    vars.session_alias = s.to_owned();
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3528 tidb_opt_objective SetSession closure) L1
pub(super) fn ss_tidb_opt_objective(vars: &mut SessionVars, s: &str) -> Result<(), Error> {
    vars.opt_objective = s.to_owned();
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3566 tidb_dml_type SetSession closure) L2 (BulkDMLEnabled is X)
pub(super) fn ss_tidb_dml_type(_s: &mut SessionVars, val: &str) -> Result<(), Error> {
    let lower_val = val.to_lowercase();
    // BulkDMLEnabled is a write-path field (X in the D05.2 scoping): only the error path is kept.
    if tidb_base::gostd::equal_fold(&lower_val, "standard") {
        return Ok(());
    }
    if tidb_base::gostd::equal_fold(&lower_val, "bulk") {
        return Ok(());
    }
    Err(tidb_base::errors::new(format!("unsupport DML type: {val}")))
}

// go-part: defaultSysVars (sysvar.go:L3598 tidb_enable_shared_lock_promotion SetSession closure) L2 (the warning log is not kept)
pub(super) fn ss_tidb_enable_shared_lock_promotion(
    s: &mut SessionVars,
    val: &str,
) -> Result<(), Error> {
    s.shared_lock_promotion = tidb_opt_on(val);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3654 tidb_index_lookup_pushdown_policy SetSession closure) L1
pub(super) fn ss_tidb_index_lookup_pushdown_policy(
    vars: &mut SessionVars,
    s: &str,
) -> Result<(), Error> {
    vars.index_look_up_push_down_policy = s.to_owned();
    Ok(())
}

// ---------------------------------------------------------------- GetSession

// go-part: defaultSysVars (sysvar.go:L96 timestamp GetSession closure) L2 (the statement time cache: the current time)
pub(super) fn gs_timestamp(s: &mut SessionVars) -> Result<String, Error> {
    if let Some(timestamp) = s.systems.get(super::names::Timestamp)
        && timestamp != DefTimestamp
    {
        return Ok(timestamp.clone());
    }
    let now = tidb_base::gostd::time::now();
    let nanos = now
        .unix()
        .wrapping_mul(1_000_000_000)
        .wrapping_add(now.nanosecond());
    Ok(tidb_base::gostd::strconv::format_float(
        nanos as f64 / 1e9,
        b'f',
        -1,
        64,
    ))
}

// go-part: defaultSysVars (sysvar.go:L118 last_insert_id GetSession closure) L1
pub(super) fn gs_last_insert_id(s: &mut SessionVars) -> Result<String, Error> {
    Ok(s.stmt_ctx.prev_last_insert_id.to_string())
}

// go-part: defaultSysVars (sysvar.go:L123 identity GetSession closure) L1
pub(super) fn gs_identity(s: &mut SessionVars) -> Result<String, Error> {
    Ok(s.stmt_ctx.prev_last_insert_id.to_string())
}

// go-part: defaultSysVars (sysvar.go:L147 txn_scope GetSession closure) L1
pub(super) fn gs_txn_scope(s: &mut SessionVars) -> Result<String, Error> {
    Ok(s.state.txn_scope.get_var_value().to_owned())
}

// go-part: defaultSysVars (sysvar.go:L265 tidb_current_ts GetSession closure) L1
pub(super) fn gs_tidb_current_ts(s: &mut SessionVars) -> Result<String, Error> {
    Ok(s.state.txn_ctx.start_ts.to_string())
}

// go-part: defaultSysVars (sysvar.go:L271 tidb_last_query_info GetSession closure) L1
pub(super) fn gs_tidb_last_query_info(s: &mut SessionVars) -> Result<String, Error> {
    Ok(s.state.last_query_info.marshal_json())
}

// go-part: defaultSysVars (sysvar.go:L418 tidb_last_ddl_info GetSession closure) L1
pub(super) fn gs_tidb_last_ddl_info(s: &mut SessionVars) -> Result<String, Error> {
    Ok(s.state.last_ddl_info.marshal_json())
}

// go-part: defaultSysVars (sysvar.go:L847 tidb_scatter_region GetSession closure) L1
pub(super) fn gs_tidb_scatter_region(vars: &mut SessionVars) -> Result<String, Error> {
    Ok(vars.scatter_region.clone())
}

// go-part: defaultSysVars (sysvar.go:L1627 tidb_enable_plan_replayer_continuous_capture GetSession closure) L1
pub(super) fn gs_tidb_enable_plan_replayer_continuous_capture(
    vars: &mut SessionVars,
) -> Result<String, Error> {
    Ok(bool_to_on_off(vars.enable_plan_replayed_continues_capture).to_owned())
}

// go-part: defaultSysVars (sysvar.go:L1646 tidb_enable_plan_replayer_capture GetSession closure) L1
pub(super) fn gs_tidb_enable_plan_replayer_capture(
    vars: &mut SessionVars,
) -> Result<String, Error> {
    Ok(bool_to_on_off(vars.enable_plan_replayer_capture).to_owned())
}

// go-part: defaultSysVars (sysvar.go:L1904 group_concat_max_len GetSession closure) L1
pub(super) fn gs_group_concat_max_len(sv: &mut SessionVars) -> Result<String, Error> {
    Ok(sv.group_concat_max_len.to_string())
}

// go-part: defaultSysVars (sysvar.go:L2548 tidb_partition_prune_mode GetSession closure) L1
pub(super) fn gs_tidb_partition_prune_mode(s: &mut SessionVars) -> Result<String, Error> {
    Ok(s.state.partition_prune_mode.clone())
}

// go-part: defaultSysVars (sysvar.go:L3240 tiflash_replica_read GetSession closure) L1
pub(super) fn gs_tiflash_replica_read(s: &mut SessionVars) -> Result<String, Error> {
    Ok(get_tiflash_replica_read(s.tiflash_replica_read).to_owned())
}

// go-part: defaultSysVars (sysvar.go:L3519 tidb_session_alias GetSession closure) L1
pub(super) fn gs_tidb_session_alias(vars: &mut SessionVars) -> Result<String, Error> {
    Ok(vars.session_alias.clone())
}

// go-part: defaultSysVars (sysvar.go:L3686 tidb_slow_log_rules GetSession closure) L1
pub(super) fn gs_tidb_slow_log_rules(vars: &mut SessionVars) -> Result<String, Error> {
    if let Some(rules) = &vars.state.slow_log_rules.slow_log_rules {
        return Ok(rules.raw_rules.clone());
    }
    Ok(String::new())
}

// ---------------------------------------------------------------- GetStateValue

// go-part: defaultSysVars (sysvar.go:L288 tx_isolation_one_shot GetStateValue closure) L1
pub(super) fn gv_tx_isolation_one_shot(s: &mut SessionVars) -> Result<(String, bool), Error> {
    if s.state.txn_isolation_level_one_shot.state != OneShotState::OneShotDef {
        return Ok((s.state.txn_isolation_level_one_shot.value.clone(), true));
    }
    Ok((String::new(), false))
}

// go-part: defaultSysVars (sysvar.go:L398 rand_seed1 GetStateValue closure) L1
pub(super) fn gv_rand_seed1(s: &mut SessionVars) -> Result<(String, bool), Error> {
    Ok((s.state.rng.get_seed1().to_string(), true))
}

// go-part: defaultSysVars (sysvar.go:L406 rand_seed2 GetStateValue closure) L1
pub(super) fn gv_rand_seed2(s: &mut SessionVars) -> Result<(String, bool), Error> {
    Ok((s.state.rng.get_seed2().to_string(), true))
}

// go-part: defaultSysVars (sysvar.go:L3677 tidb_slow_log_rules SetSession closure) L1
pub(super) fn ss_tidb_slow_log_rules(s: &mut SessionVars, val: &str) -> Result<(), Error> {
    let slow_log_rules = super::slow_log_rules::parse_session_slow_log_rules(val)?;
    s.state.slow_log_rules.slow_log_rules = slow_log_rules;
    s.state.slow_log_rules.need_update_effective_fields = true;
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3692 tidb_slow_log_rules SetGlobal closure) L1
pub(super) fn sg_tidb_slow_log_rules(_vars: &mut SessionVars, s: &str) -> Result<(), Error> {
    let g_rules = super::slow_log_rules::parse_global_slow_log_rules(s)?;
    *super::slow_log_rules::GlobalSlowLogRules.write() = std::sync::Arc::new(g_rules);
    Ok(())
}

// go-part: defaultSysVars (sysvar.go:L3700 tidb_slow_log_rules GetGlobal closure) L1
pub(super) fn gg_tidb_slow_log_rules(_vars: &mut SessionVars) -> Result<String, Error> {
    Ok(super::slow_log_rules::GlobalSlowLogRules.read().raw_rules.clone())
}
