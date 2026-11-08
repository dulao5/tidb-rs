//! The `Validation` hooks of the system variable table (pkg/sessionctx/variable/sysvar.go and
//! noop.go). Go writes them as closures inside the table literals; here they are functions
//! named after the variable, found by [`validation`] (N8 §3.1 item 2). Each function carries
//! the Go position of its closure. The registry checks at construction that exactly the
//! variables whose Go entry has a `Validation` have a function here.

// Go names for constants (M-GONAME).
#![allow(non_upper_case_globals)]

use tidb_base::Error;
use tidb_base::errors;
use tidb_base::gofmt::Arg;
use tidb_base::gostd;

use super::error::*;
use super::names::*;
use super::session::SessionVars;
use super::tidb_vars::*;
use super::variable::{Off, On, IntOnly, ScopeFlag, ScopeGlobal, ScopeSession, ValResult, parse_float64};
use super::varsutil::*;

use std::sync::atomic::Ordering::SeqCst;

/// The `Validation` hook of a variable, or `None` when Go's entry has none.
// glue: the Validation fields of the Go table literals, keyed by variable name
pub(crate) fn validation(name: &str) -> Option<super::variable::ValidationFn> {
    Some(match name {
        "timestamp" => v_timestamp,
        "tx_read_ts" => v_identity,
        "tidb_enforce_mpp" => v_tidb_enforce_mpp,
        "tiflash_query_spill_ratio" => v_tiflash_query_spill_ratio,
        "tidb_enable_tiflash_pipeline_model" => v_tidb_enable_tiflash_pipeline_model,
        "tx_isolation_one_shot" | "tx_isolation" | "transaction_isolation" => check_isolation_level,
        "tidb_isolation_read_engines" => v_tidb_isolation_read_engines,
        "tidb_read_consistency" => v_tidb_read_consistency,
        "init_connect" => v_init_connect,
        "validate_password.length" => v_validate_password_length,
        "validate_password.mixed_case_count" => v_validate_password_mixed_case_count,
        "validate_password.number_count" => v_validate_password_number_count,
        "validate_password.special_char_count" => v_validate_password_special_char_count,
        "tidb_auto_analyze_ratio" => v_tidb_auto_analyze_ratio,
        "tidb_scatter_region" => v_tidb_scatter_region,
        "tidb_super_read_only" => v_tidb_super_read_only,
        "tidb_gogc_tuner_max_value" => v_tidb_gogc_tuner_max_value,
        "tidb_gogc_tuner_min_value" => v_tidb_gogc_tuner_min_value,
        "tidb_auto_analyze_partition_batch_size" => v_tidb_auto_analyze_partition_batch_size,
        "tidb_analyze_column_options" => v_tidb_analyze_column_options,
        "tidb_enable_auto_analyze_priority_queue" => v_tidb_enable_auto_analyze_priority_queue,
        "tidb_gogc_tuner_threshold" => v_tidb_gogc_tuner_threshold,
        "tidb_server_memory_limit" => v_tidb_server_memory_limit,
        "tidb_server_memory_limit_sess_min_size" => v_tidb_server_memory_limit_sess_min_size,
        "tidb_server_memory_limit_gc_trigger" => v_tidb_server_memory_limit_gc_trigger,
        "tidb_enable_column_tracking" => v_tidb_enable_column_tracking,
        "require_secure_transport" => v_require_secure_transport,
        "tidb_prepared_plan_cache_size" => v_tidb_prepared_plan_cache_size,
        "tidb_non_prepared_plan_cache_size" => v_tidb_non_prepared_plan_cache_size,
        "tidb_auto_analyze_concurrency" => v_tidb_auto_analyze_concurrency,
        "tidb_enable_plan_replayer_continuous_capture" => v_tidb_enable_plan_replayer_continuous_capture,
        "sql_mode" => v_sql_mode,
        "collation_server" | "collation_database" | "collation_connection" => check_collation,
        "default_collation_for_utf8mb4" => v_default_collation_for_utf8mb4,
        "time_zone" => v_time_zone,
        "foreign_key_checks" => v_foreign_key_checks,
        "character_set_client" => v_character_set_client,
        "character_set_results" => v_character_set_results,
        "character_set_database" => v_character_set_database,
        "group_concat_max_len" => v_group_concat_max_len,
        "character_set_connection" => v_character_set_connection,
        "character_set_server" => v_character_set_server,
        "max_allowed_packet" => v_max_allowed_packet,
        "tidb_mpp_store_fail_ttl" => v_tidb_mpp_store_fail_ttl,
        "tidb_index_lookup_concurrency" => v_tidb_index_lookup_concurrency,
        "tidb_index_lookup_join_concurrency" => v_tidb_index_lookup_join_concurrency,
        "tidb_enable_table_partition" => v_tidb_enable_table_partition,
        "tidb_enable_list_partition" => v_tidb_enable_list_partition,
        "tidb_hash_join_concurrency" => v_tidb_hash_join_concurrency,
        "tidb_projection_concurrency" => v_tidb_projection_concurrency,
        "tidb_hashagg_partial_concurrency" => v_tidb_hashagg_partial_concurrency,
        "tidb_hashagg_final_concurrency" => v_tidb_hashagg_final_concurrency,
        "tidb_window_concurrency" => v_tidb_window_concurrency,
        "tidb_merge_join_concurrency" => v_tidb_merge_join_concurrency,
        "tidb_streamagg_concurrency" => v_tidb_streamagg_concurrency,
        "tidb_index_merge_intersection_concurrency" => v_tidb_index_merge_intersection_concurrency,
        "tidb_disable_txn_auto_retry" => v_tidb_disable_txn_auto_retry,
        "tidb_enable_fast_analyze" => v_tidb_enable_fast_analyze,
        "tidb_allow_fallback_to_tikv" => v_tidb_allow_fallback_to_tikv,
        "tidb_enable_noop_functions" => v_tidb_enable_noop_functions,
        "tidb_evolve_plan_baselines" => v_tidb_evolve_plan_baselines,
        "tidb_enable_clustered_index" => v_tidb_enable_clustered_index,
        "tidb_enable_global_index" => v_tidb_enable_global_index,
        "tidb_partition_prune_mode" => v_tidb_partition_prune_mode,
        "tidb_analyze_version" => v_tidb_analyze_version,
        "tidb_hash_join_version" => v_tidb_hash_join_version,
        "tidb_enable_exchange_partition" => v_tidb_enable_exchange_partition,
        "tidb_enable_new_cost_interface" => v_tidb_enable_new_cost_interface,
        "tidb_mem_quota_query" => v_tidb_mem_quota_query,
        "tidb_enable_tiflash_read_for_write_stmt" => v_tidb_enable_tiflash_read_for_write_stmt,
        "tidb_opt_partial_ordered_index_for_topn" => v_tidb_opt_partial_ordered_index_for_topn,
        "mpp_exchange_compression_mode" => v_mpp_exchange_compression_mode,
        "mpp_version" => v_mpp_version,
        "tidb_analyze_skip_column_types" => v_tidb_analyze_skip_column_types,
        "tidb_runtime_filter_type" => v_tidb_runtime_filter_type,
        "tidb_runtime_filter_mode" => v_tidb_runtime_filter_mode,
        "tidb_schema_cache_size" => v_tidb_schema_cache_size,
        "tidb_session_alias" => v_tidb_session_alias,
        "tidb_service_scope" => v_tidb_service_scope,
        "tiflash_hashagg_preaggregation_mode" => v_tiflash_hashagg_preaggregation_mode,
        "tidb_max_dist_task_nodes" => v_tidb_max_dist_task_nodes,
        "tx_read_only" | "transaction_read_only" | "super_read_only" | "read_only" => v_check_read_only,
        "offline_mode" => v_offline_mode,
        "character_set_filesystem" => v_character_set_filesystem,
        "sql_auto_is_null" => v_sql_auto_is_null,
        "secure_auth" => v_secure_auth,
        _ => return None,
    })
}

fn a(s: &str) -> Arg {
    Arg::from(s.to_owned())
}

fn ok(v: impl Into<String>) -> ValResult {
    (v.into(), Ok(()))
}

fn fail(v: impl Into<String>, e: Error) -> ValResult {
    (v.into(), Err(e))
}

// go: sysvar.go:L105 Timestamp.Validation L1
fn v_timestamp(_vars: &mut SessionVars, normalized_value: &str, original_value: &str, _scope: ScopeFlag) -> ValResult {
    let val = tidb_opt_float64(original_value, DefTimestampFloat);
    if val > i32::MAX as f64 {
        return fail(original_value, ErrWrongValueForVar.gen_with_stack_by_args(vec![a(Timestamp), a(original_value)]));
    }
    ok(normalized_value)
}

// go: sysvar.go:L152 TiDBTxnReadTS.Validation L1
fn v_identity(_vars: &mut SessionVars, normalized_value: &str, _o: &str, _scope: ScopeFlag) -> ValResult {
    ok(normalized_value)
}

// go: sysvar.go:L158 TiDBEnforceMPPExecution.Validation L1
fn v_tidb_enforce_mpp(vars: &mut SessionVars, normalized_value: &str, _o: &str, _scope: ScopeFlag) -> ValResult {
    if tidb_opt_on(normalized_value) && !vars.allow_mpp_execution {
        return fail(
            normalized_value,
            ErrWrongValueForVar.gen_with_stack_by_args(vec![
                a("tidb_enforce_mpp"),
                a("1' but tidb_allow_mpp is 0, please activate tidb_allow_mpp at first."),
            ]),
        );
    }
    ok(normalized_value)
}

// go: sysvar.go:L187 TiFlashQuerySpillRatio.Validation L1
fn v_tiflash_query_spill_ratio(_vars: &mut SessionVars, normalized_value: &str, _o: &str, _scope: ScopeFlag) -> ValResult {
    let val = match parse_float64(normalized_value) {
        Ok(v) => v,
        Err(()) => return fail("", parse_float_error(normalized_value)),
    };
    if val > 0.85 || val < 0.0 {
        return fail("", errors::new("The valid value of tidb_tiflash_auto_spill_ratio is between 0 and 0.85"));
    }
    ok(normalized_value)
}

/// The `*strconv.NumError` of a failed `strconv.ParseFloat(s, 64)`.
// go-std: strconv.NumError.Error L1
fn parse_float_error(s: &str) -> Error {
    match tidb_types::gostd::parse_float(s).1 {
        Err(e) => errors::new(e.to_string()),
        Ok(()) => unreachable!("parse_float_error of a valid float"),
    }
}

/// `strconv.Quote`.
// go-std: strconv.Quote L1
fn go_quote(s: &str) -> String {
    String::from_utf8_lossy(&tidb_base::gofmt::sprintf("%q", &[Arg::go_string(s.as_bytes().to_vec())])).into_owned()
}

// go: sysvar.go:L205 TiDBEnableTiFlashPipelineMode.Validation L1
fn v_tidb_enable_tiflash_pipeline_model(vars: &mut SessionVars, normalized_value: &str, _o: &str, _scope: ScopeFlag) -> ValResult {
    vars.stmt_ctx.append_warning(ErrWarnDeprecatedSyntaxSimpleMsg.fast_gen_by_args(vec![a(TiDBEnableTiFlashPipelineMode)]));
    ok(normalized_value)
}

// go: sysvar.go:L338 TiDBIsolationReadEngines.Validation L1
fn v_tidb_isolation_read_engines(_vars: &mut SessionVars, normalized_value: &str, _o: &str, _scope: ScopeFlag) -> ValResult {
    use tidb_base::kv::{TiDB, TiFlash, TiKV};
    let mut format_val = String::new();
    for (i, engine) in normalized_value.split(',').enumerate() {
        let engine = trim_space(engine);
        if i != 0 {
            format_val.push(',');
        }
        if gostd::equal_fold(engine, TiKV.name()) {
            format_val.push_str(TiKV.name());
        } else if gostd::equal_fold(engine, TiFlash.name()) {
            format_val.push_str(TiFlash.name());
        } else if gostd::equal_fold(engine, TiDB.name()) {
            format_val.push_str(TiDB.name());
        } else {
            return fail(
                normalized_value,
                ErrWrongValueForVar.gen_with_stack_by_args(vec![a(TiDBIsolationReadEngines), a(normalized_value)]),
            );
        }
    }
    ok(format_val)
}

/// `strings.TrimSpace`.
// go-std: strings.TrimSpace L1
fn trim_space(s: &str) -> &str {
    s.trim_matches(gostd_is_space)
}

/// validateReadConsistencyLevel (session.go, block D05.2; used by this hook).
// go: validateReadConsistencyLevel L1
pub(crate) fn validate_read_consistency_level(val: &str) -> Result<(), Error> {
    match gostd::to_lower(val).as_str() {
        "strict" | "weak" => Ok(()),
        _ => Err(ErrWrongTypeForVar.gen_with_stack_by_args(vec![a(TiDBReadConsistency)])),
    }
}

// go: sysvar.go:L410 TiDBReadConsistency.Validation L1
fn v_tidb_read_consistency(_vars: &mut SessionVars, normalized: &str, _o: &str, _scope: ScopeFlag) -> ValResult {
    (normalized.to_owned(), validate_read_consistency_level(normalized))
}

/// `init_connect` must parse as SQL (`parser.New().ParseSQL` with the session's SQL mode and
/// parser config). The faithful parser driver is block D02.2; until then this uses the
/// PoC parser of tidb-parser, which accepts the same grammar (parser.y port).
// go: sysvar.go:L635 InitConnect.Validation L2 (PoC parse entry point until D02.2)
fn v_init_connect(_vars: &mut SessionVars, normalized_value: &str, _o: &str, _scope: ScopeFlag) -> ValResult {
    if tidb_parser::parse(normalized_value, &tidb_parser::ParseOptions::default()).is_err() {
        return fail(normalized_value, ErrWrongTypeForVar.gen_with_stack_by_args(vec![a(InitConnect)]));
    }
    ok(normalized_value)
}

fn parse_int32(s: &str) -> Result<i64, Error> {
    let (v, err) = tidb_types::gostd::parse_int(s.as_bytes(), 32);
    match err {
        Ok(()) => Ok(v),
        Err(e) => Err(e.into_base()),
    }
}

// go: sysvar.go:L649 ValidatePasswordLength.Validation L1
fn v_validate_password_length(_vars: &mut SessionVars, normalized_value: &str, _o: &str, _scope: ScopeFlag) -> ValResult {
    let (number_count, special_char_count, mixed_case_count) = (
        PasswordValidtaionNumberCount.load(SeqCst),
        PasswordValidationSpecialCharCount.load(SeqCst),
        PasswordValidationMixedCaseCount.load(SeqCst),
    );
    let length = match parse_int32(normalized_value) {
        Ok(v) => v,
        Err(e) => return fail("", e),
    };
    let min_length = number_count.wrapping_add(special_char_count).wrapping_add(mixed_case_count.wrapping_mul(2));
    if (length as i32) < min_length {
        return ok((min_length as i64).to_string());
    }
    ok(normalized_value)
}

// go: sysvar.go:L668 ValidatePasswordMixedCaseCount.Validation L1
fn v_validate_password_mixed_case_count(vars: &mut SessionVars, normalized_value: &str, _o: &str, _scope: ScopeFlag) -> ValResult {
    let (length, number_count, special_char_count) = (
        PasswordValidationLength.load(SeqCst),
        PasswordValidtaionNumberCount.load(SeqCst),
        PasswordValidationSpecialCharCount.load(SeqCst),
    );
    let mixed_case_count = match parse_int32(normalized_value) {
        Ok(v) => v,
        Err(e) => return fail("", e),
    };
    let min_length = number_count.wrapping_add(special_char_count).wrapping_add((mixed_case_count as i32).wrapping_mul(2));
    if length < min_length {
        if let Err(e) = update_password_validation_length(vars, min_length) {
            return fail("", e);
        }
    }
    ok(normalized_value)
}

// go: sysvar.go:L690 ValidatePasswordNumberCount.Validation L1
fn v_validate_password_number_count(vars: &mut SessionVars, normalized_value: &str, _o: &str, _scope: ScopeFlag) -> ValResult {
    let (length, special_char_count, mixed_case_count) = (
        PasswordValidationLength.load(SeqCst),
        PasswordValidationSpecialCharCount.load(SeqCst),
        PasswordValidationMixedCaseCount.load(SeqCst),
    );
    let number_count = match parse_int32(normalized_value) {
        Ok(v) => v,
        Err(e) => return fail("", e),
    };
    let min_length = (number_count as i32).wrapping_add(special_char_count).wrapping_add(mixed_case_count.wrapping_mul(2));
    if length < min_length {
        if let Err(e) = update_password_validation_length(vars, min_length) {
            return fail("", e);
        }
    }
    ok(normalized_value)
}

// go: sysvar.go:L712 ValidatePasswordSpecialCharCount.Validation L1
fn v_validate_password_special_char_count(vars: &mut SessionVars, normalized_value: &str, _o: &str, _scope: ScopeFlag) -> ValResult {
    let (length, number_count, mixed_case_count) = (
        PasswordValidationLength.load(SeqCst),
        PasswordValidtaionNumberCount.load(SeqCst),
        PasswordValidationMixedCaseCount.load(SeqCst),
    );
    let special_char_count = match parse_int32(normalized_value) {
        Ok(v) => v,
        Err(e) => return fail("", e),
    };
    let min_length = number_count.wrapping_add(special_char_count as i32).wrapping_add(mixed_case_count.wrapping_mul(2));
    if length < min_length {
        if let Err(e) = update_password_validation_length(vars, min_length) {
            return fail("", e);
        }
    }
    ok(normalized_value)
}

// go: sysvar.go:L783 TiDBAutoAnalyzeRatio.Validation L1
fn v_tidb_auto_analyze_ratio(_vars: &mut SessionVars, normalized_value: &str, _o: &str, _scope: ScopeFlag) -> ValResult {
    let ratio = match parse_float64(normalized_value) {
        Ok(v) => v,
        Err(()) => return fail("", parse_float_error(normalized_value)),
    };
    const MIN_RATIO: f64 = 0.00001;
    const TOLERANCE: f64 = 1e-9;
    if ratio < MIN_RATIO && (ratio - MIN_RATIO).abs() > TOLERANCE {
        return fail(
            "",
            errors::errorf("the value of %s should be greater than or equal to %f", &[a(TiDBAutoAnalyzeRatio), MIN_RATIO.into()]),
        );
    }
    ok(normalized_value)
}

/// ScatterOff means default, will not scatter region
const ScatterOff: &str = "";
/// ScatterTable means scatter region at table level
const ScatterTable: &str = "table";
/// ScatterGlobal means scatter region at global level
const ScatterGlobal: &str = "global";

// go: sysvar.go:L850 TiDBScatterRegion.Validation L1
fn v_tidb_scatter_region(_vars: &mut SessionVars, normalized_value: &str, _o: &str, _scope: ScopeFlag) -> ValResult {
    let lower_val = gostd::to_lower(normalized_value);
    if lower_val != ScatterOff && lower_val != ScatterTable && lower_val != ScatterGlobal {
        return fail(
            "",
            errors::errorf(
                "invalid value for '%s', it should be either '%s', '%s' or '%s'",
                &[a(&lower_val), a(ScatterOff), a(ScatterTable), a(ScatterGlobal)],
            ),
        );
    }
    ok(lower_val)
}

// go: sysvar.go:L917 TiDBSuperReadOnly.Validation L1
fn v_tidb_super_read_only(s: &mut SessionVars, normalized_value: &str, _o: &str, _scope: ScopeFlag) -> ValResult {
    let on = tidb_opt_on(normalized_value);
    if !on && s.stmt_ctx.stmt_type == "Set" {
        let result = match s.global_vars_accessor.get_global_sys_var(TiDBRestrictedReadOnly) {
            Ok(v) => v,
            Err(e) => return fail(normalized_value, e),
        };
        if tidb_opt_on(&result) {
            return fail(
                normalized_value,
                errors::errorf("can't turn off %s when %s is on", &[a(TiDBSuperReadOnly), a(TiDBRestrictedReadOnly)]),
            );
        }
    }
    ok(normalized_value)
}

// go: sysvar.go:L952 TiDBGOGCTunerMaxValue.Validation L1 [runtime-only]
fn v_tidb_gogc_tuner_max_value(_s: &mut SessionVars, _n: &str, origin: &str, _scope: ScopeFlag) -> ValResult {
    let max_value = tidb_opt_int64(origin, DefTiDBGOGCMaxValue);
    if max_value <= minGCPercent.load(SeqCst) as i64 {
        return fail("", errors::new("tidb_gogc_tuner_max_value should be more than tidb_gogc_tuner_min_value"));
    }
    ok(origin)
}

// go: sysvar.go:L969 TiDBGOGCTunerMinValue.Validation L1 [runtime-only]
fn v_tidb_gogc_tuner_min_value(_s: &mut SessionVars, _n: &str, origin: &str, _scope: ScopeFlag) -> ValResult {
    let min_value = tidb_opt_int64(origin, DefTiDBGOGCMinValue);
    if min_value >= maxGCPercent.load(SeqCst) as i64 {
        return fail("", errors::new("tidb_gogc_tuner_min_value should be less than tidb_gogc_tuner_max_value"));
    }
    ok(origin)
}

// go: sysvar.go:L1040 TiDBAutoAnalyzePartitionBatchSize.Validation L1
fn v_tidb_auto_analyze_partition_batch_size(vars: &mut SessionVars, normalized_value: &str, _o: &str, _scope: ScopeFlag) -> ValResult {
    vars.stmt_ctx.append_warning(ErrWarnDeprecatedSyntaxNoReplacement.fast_gen_by_args(vec![a(TiDBAutoAnalyzePartitionBatchSize)]));
    ok(normalized_value)
}

// go: sysvar.go:L1116 TiDBAnalyzeColumnOptions.Validation L1
fn v_tidb_analyze_column_options(_s: &mut SessionVars, normalized_value: &str, _o: &str, _scope: ScopeFlag) -> ValResult {
    use tidb_parser::model::{AllColumns, PredicateColumns};
    let choice = gostd::to_upper(normalized_value);
    if choice != AllColumns.to_string() && choice != PredicateColumns.to_string() {
        return fail(
            "",
            errors::errorf(
                "invalid value for %s, it should be either '%s' or '%s'",
                &[a(TiDBAnalyzeColumnOptions), a(AllColumns.to_string()), a(PredicateColumns.to_string())],
            ),
        );
    }
    ok(normalized_value)
}

// go: sysvar.go:L1138 TiDBEnableAutoAnalyzePriorityQueue.Validation L1
fn v_tidb_enable_auto_analyze_priority_queue(s: &mut SessionVars, normalized_value: &str, _o: &str, _scope: ScopeFlag) -> ValResult {
    s.stmt_ctx.append_warning(ErrWarnDeprecatedSyntaxSimpleMsg.fast_gen(
        "tidb_enable_auto_analyze_priority_queue will be removed in the future and TiDB will always use priority queue to execute auto analyze.",
        vec![],
    ));
    ok(normalized_value)
}

// go: sysvar.go:L1147 TiDBGOGCTunerThreshold.Validation L1 [runtime-only]
fn v_tidb_gogc_tuner_threshold(_s: &mut SessionVars, normalized_value: &str, _o: &str, _scope: ScopeFlag) -> ValResult {
    let float_value = tidb_opt_float64(normalized_value, DefTiDBGOGCTunerThreshold);
    let global_memory_limit_tuner = memoryLimitTunerPercentage.load();
    #[allow(clippy::impossible_comparisons)] // Go's condition, never true (upstream)
    if float_value < 0.0 && float_value > 0.9 {
        return fail("", ErrWrongValueForVar.gen_with_stack_by_args(vec![a(TiDBGOGCTunerThreshold), a(normalized_value)]));
    }
    // globalMemoryLimitTuner must not be 0. it will be 0 when tidb_server_memory_limit_gc_trigger is not set during startup.
    if global_memory_limit_tuner != 0.0 && global_memory_limit_tuner < float_value + 0.05 {
        return fail("", errors::new("tidb_gogc_tuner_threshold should be less than tidb_server_memory_limit_gc_trigger - 0.05"));
    }
    ok(gostd::strconv::format_float(float_value, b'f', -1, 64))
}

// go: sysvar.go:L1180 TiDBServerMemoryLimit.Validation L1
fn v_tidb_server_memory_limit(s: &mut SessionVars, normalized_value: &str, original_value: &str, _scope: ScopeFlag) -> ValResult {
    let (_, st, err) = parse_memory_limit(s, normalized_value, original_value);
    if let Err(e) = err {
        return fail("", e);
    }
    ok(st)
}

// go: sysvar.go:L1204 TiDBServerMemoryLimitSessMinSize.Validation L1
fn v_tidb_server_memory_limit_sess_min_size(s: &mut SessionVars, normalized_value: &str, original_value: &str, _scope: ScopeFlag) -> ValResult {
    let (mut int_val, err) = tidb_types::gostd::parse_uint(normalized_value.as_bytes(), 64);
    if let Err(e) = err {
        let (bt, st) = parse_byte_size(normalized_value);
        if st.is_empty() {
            return fail("", e.into_base());
        }
        int_val = bt;
    }
    if int_val > 0 && int_val < 128 {
        // 128 Bytes
        s.stmt_ctx.append_warning(
            ErrTruncatedWrongValue.fast_gen_by_args(vec![a(TiDBServerMemoryLimitSessMinSize), a(original_value)]),
        );
        int_val = 128;
    }
    ok(int_val.to_string())
}

// go: sysvar.go:L1232 TiDBServerMemoryLimitGCTrigger.Validation L1 [runtime-only]
fn v_tidb_server_memory_limit_gc_trigger(_s: &mut SessionVars, normalized_value: &str, _o: &str, _scope: ScopeFlag) -> ValResult {
    let float_value = match parse_float64(normalized_value) {
        Ok(v) => v,
        Err(()) => {
            let (perc, st) = parse_percentage(normalized_value);
            if st.is_empty() {
                return fail("", parse_float_error(normalized_value));
            }
            perc as f64 / 100.0
        }
    };
    let gogc_tuner_threshold = GOGCTunerThreshold.load();
    if !(0.51..=1.0).contains(&float_value) {
        // 51% ~ 100%
        return fail("", ErrWrongValueForVar.gen_with_stack_by_args(vec![a(TiDBServerMemoryLimitGCTrigger), a(normalized_value)]));
    }
    // gogcTunerThreshold must not be 0. it will be 0 when tidb_gogc_tuner_threshold is not set during startup.
    if gogc_tuner_threshold != 0.0 && float_value < gogc_tuner_threshold + 0.05 {
        return fail("", errors::new("tidb_server_memory_limit_gc_trigger should be greater than tidb_gogc_tuner_threshold + 0.05"));
    }
    ok(gostd::strconv::format_float(float_value, b'f', -1, 64))
}

// go: sysvar.go:L1269 TiDBEnableColumnTracking.Validation L1
fn v_tidb_enable_column_tracking(vars: &mut SessionVars, normalized_value: &str, _o: &str, _scope: ScopeFlag) -> ValResult {
    // This variable is deprecated and will be removed in the future.
    vars.stmt_ctx.append_warning(ErrWarnDeprecatedSyntaxSimpleMsg.fast_gen(
        "The 'tidb_enable_column_tracking' variable is deprecated and will be removed in future versions of TiDB. It is always set to 'ON' now.",
        vec![],
    ));
    ok(normalized_value)
}

// go: sysvar.go:L1284 RequireSecureTransport.Validation L1
fn v_require_secure_transport(vars: &mut SessionVars, normalized_value: &str, _o: &str, _scope: ScopeFlag) -> ValResult {
    if vars.stmt_ctx.stmt_type == "Set" && tidb_opt_on(normalized_value) {
        // On tidbcloud dedicated cluster with the default configuration, if an user modify
        // @@global.require_secure_transport=on, he can not login the cluster anymore!
        // A workaround for this is making require_secure_transport read-only for that case.
        // SEM(security enhanced mode) is enabled by default with only that settings.
        let cfg = tidb_base::config::get_global_config();
        if cfg.security.enable_sem {
            return fail("", errors::new("require_secure_transport can not be set to ON with SEM(security enhanced mode) enabled"));
        }
        // Refuse to set RequireSecureTransport to ON if the connection
        // issuing the change is not secure. This helps reduce the chance of users being locked out.
        if vars.tls_connection_state.is_none() {
            return fail("", errors::new("require_secure_transport can only be set to ON if the connection issuing the change is secure"));
        }
    }
    ok(normalized_value)
}

// go: sysvar.go:L1366 TiDBPrepPlanCacheSize.Validation L1
fn v_tidb_prepared_plan_cache_size(vars: &mut SessionVars, normalized_value: &str, _o: &str, _scope: ScopeFlag) -> ValResult {
    append_deprecation_warning(vars, TiDBPrepPlanCacheSize, TiDBSessionPlanCacheSize);
    ok(normalized_value)
}

// go: sysvar.go:L1407 TiDBNonPreparedPlanCacheSize.Validation L1
fn v_tidb_non_prepared_plan_cache_size(vars: &mut SessionVars, normalized_value: &str, _o: &str, _scope: ScopeFlag) -> ValResult {
    append_deprecation_warning(vars, TiDBNonPreparedPlanCacheSize, TiDBSessionPlanCacheSize);
    ok(normalized_value)
}

// go: sysvar.go:L1494 TiDBAutoAnalyzeConcurrency.Validation L1
fn v_tidb_auto_analyze_concurrency(_vars: &mut SessionVars, normalized_value: &str, original_value: &str, _scope: ScopeFlag) -> ValResult {
    // Check if auto-analyze and auto-analyze priority queue are enabled
    let enable_auto_analyze = RunAutoAnalyze.load(SeqCst);
    let enable_auto_analyze_priority_queue = EnableAutoAnalyzePriorityQueue.load(SeqCst);
    // Validate that both required settings are enabled
    if !enable_auto_analyze || !enable_auto_analyze_priority_queue {
        return fail(
            original_value,
            errors::errorf(
                "cannot set %s: requires both tidb_enable_auto_analyze and tidb_enable_auto_analyze_priority_queue to be true. Current values: tidb_enable_auto_analyze=%v, tidb_enable_auto_analyze_priority_queue=%v",
                &[a(TiDBAutoAnalyzeConcurrency), enable_auto_analyze.into(), enable_auto_analyze_priority_queue.into()],
            ),
        );
    }
    ok(normalized_value)
}

// go: sysvar.go:L1630 TiDBEnablePlanReplayerContinuousCapture.Validation L1
fn v_tidb_enable_plan_replayer_continuous_capture(vars: &mut SessionVars, s: &str, _o: &str, _flag: ScopeFlag) -> ValResult {
    let historical_stats_enabled = match vars.global_vars_accessor.get_global_sys_var(TiDBEnableHistoricalStats) {
        Ok(v) => v,
        Err(e) => return fail("", e),
    };
    if !tidb_opt_on(&historical_stats_enabled) && tidb_opt_on(s) {
        return fail(
            "",
            errors::errorf("%v should be enabled before enabling %v", &[a(TiDBEnableHistoricalStats), a(TiDBEnablePlanReplayerContinuousCapture)]),
        );
    }
    ok(s)
}

// go: sysvar.go:L1685 SQLModeVar.Validation L1
fn v_sql_mode(_vars: &mut SessionVars, normalized_value: &str, original_value: &str, _scope: ScopeFlag) -> ValResult {
    // Ensure the SQL mode parses
    let normalized_value = tidb_base::mysql::format_sql_mode_str(normalized_value);
    let (_, err) = tidb_base::mysql::get_sql_mode(&normalized_value);
    if let Err(e) = err {
        return fail(original_value, errors::new(e.error()));
    }
    ok(normalized_value)
}

// go: sysvar.go:L1752 DefaultCollationForUTF8MB4.Validation L1
fn v_default_collation_for_utf8mb4(vars: &mut SessionVars, normalized_value: &str, original_value: &str, scope: ScopeFlag) -> ValResult {
    let (coll, err) = check_default_collation_for_utf8mb4(vars, normalized_value, original_value, scope);
    if err.is_ok() {
        vars.stmt_ctx.append_warning(ErrWarnDeprecatedSyntaxNoReplacement.fast_gen_by_args(vec![a(DefaultCollationForUTF8MB4)]));
    }
    (coll, err)
}

// go: sysvar.go:L1767 TimeZone.Validation L1
fn v_time_zone(_vars: &mut SessionVars, normalized_value: &str, _o: &str, _scope: ScopeFlag) -> ValResult {
    if gostd::equal_fold(normalized_value, "SYSTEM") {
        return ok("SYSTEM");
    }
    (normalized_value.to_owned(), super::util::parse_time_zone(normalized_value).map(|_| ()))
}

// go: sysvar.go:L1784 ForeignKeyChecks.Validation L1
fn v_foreign_key_checks(vars: &mut SessionVars, normalized_value: &str, _o: &str, _scope: ScopeFlag) -> ValResult {
    if tidb_opt_on(normalized_value) {
        vars.foreign_key_checks = true;
        ok(On)
    } else {
        vars.foreign_key_checks = false;
        ok(Off)
    }
}

// go: sysvar.go:L1822 CharacterSetClient.Validation L1
fn v_character_set_client(_vars: &mut SessionVars, normalized_value: &str, _o: &str, _scope: ScopeFlag) -> ValResult {
    check_character_set(normalized_value, CharacterSetClient)
}

// go: sysvar.go:L1825 CharacterSetResults.Validation L1
fn v_character_set_results(_vars: &mut SessionVars, normalized_value: &str, _o: &str, _scope: ScopeFlag) -> ValResult {
    if normalized_value.is_empty() {
        return ok(normalized_value);
    }
    check_character_set(normalized_value, "")
}

// go: sysvar.go:L1857 CharsetDatabase.Validation L1
fn v_character_set_database(_vars: &mut SessionVars, normalized_value: &str, _o: &str, _scope: ScopeFlag) -> ValResult {
    check_character_set(normalized_value, CharsetDatabase)
}

// go: sysvar.go:L1880 GroupConcatMaxLen.Validation L1 (mathutil.IntBits == 64: the 32-bit branch is dead)
fn v_group_concat_max_len(_vars: &mut SessionVars, normalized_value: &str, _o: &str, _scope: ScopeFlag) -> ValResult {
    // https://dev.mysql.com/doc/refman/8.0/en/server-system-variables.html#sysvar_group_concat_max_len
    // Minimum Value 4
    // Maximum Value (64-bit platforms) 18446744073709551615
    // Maximum Value (32-bit platforms) 4294967295
    ok(normalized_value)
}

// go: sysvar.go:L1908 CharacterSetConnection.Validation L1
fn v_character_set_connection(_vars: &mut SessionVars, normalized_value: &str, _o: &str, _scope: ScopeFlag) -> ValResult {
    check_character_set(normalized_value, CharacterSetConnection)
}

// go: sysvar.go:L1916 CharacterSetServer.Validation L1
fn v_character_set_server(_vars: &mut SessionVars, normalized_value: &str, _o: &str, _scope: ScopeFlag) -> ValResult {
    check_character_set(normalized_value, CharacterSetServer)
}

// go: sysvar.go:L1925 MaxAllowedPacket.Validation L1
fn v_max_allowed_packet(vars: &mut SessionVars, normalized_value: &str, _o: &str, scope: ScopeFlag) -> ValResult {
    if vars.stmt_ctx.stmt_type == "Set" && scope == ScopeSession {
        let err = ErrReadOnly.gen_with_stack_by_args(vec![a("SESSION"), a(MaxAllowedPacket), a("GLOBAL")]);
        return fail(normalized_value, err);
    }
    // Truncate the value of max_allowed_packet to be a multiple of 1024,
    // nonmultiples are rounded down to the nearest multiple.
    let (mut u, err) = tidb_types::gostd::parse_uint(normalized_value.as_bytes(), 64);
    if let Err(e) = err {
        return fail(normalized_value, e.into_base());
    }
    let remainder = u % 1024;
    if remainder != 0 {
        vars.stmt_ctx.append_warning(ErrTruncatedWrongValue.fast_gen_by_args(vec![a(MaxAllowedPacket), a(normalized_value)]));
        u -= remainder;
    }
    ok(u.to_string())
}

// go: sysvar.go:L1981 TiDBMPPStoreFailTTL.Validation L1
fn v_tidb_mpp_store_fail_ttl(vars: &mut SessionVars, _n: &str, _o: &str, _scope: ScopeFlag) -> ValResult {
    vars.stmt_ctx.append_warning(errors::new(
        "tidb_mpp_store_fail_ttl is always 0s. This variable has been deprecated and will be removed in the future releases",
    ));
    ok(DefTiDBMPPStoreFailTTL)
}

macro_rules! deprecated_concurrency {
    ($($(#[$m:meta])* $f:ident => $name:ident;)*) => {$(
        $(#[$m])*
        fn $f(vars: &mut SessionVars, normalized_value: &str, _o: &str, _scope: ScopeFlag) -> ValResult {
            append_deprecation_warning(vars, $name, TiDBExecutorConcurrency);
            ok(normalized_value)
        }
    )*};
}

deprecated_concurrency! {
    // go: sysvar.go:L2184 TiDBIndexLookupConcurrency.Validation L1
    v_tidb_index_lookup_concurrency => TiDBIndexLookupConcurrency;
    // go: sysvar.go:L2191 TiDBIndexLookupJoinConcurrency.Validation L1
    v_tidb_index_lookup_join_concurrency => TiDBIndexLookupJoinConcurrency;
    // go: sysvar.go:L2256 TiDBHashJoinConcurrency.Validation L1
    v_tidb_hash_join_concurrency => TiDBHashJoinConcurrency;
    // go: sysvar.go:L2263 TiDBProjectionConcurrency.Validation L1
    v_tidb_projection_concurrency => TiDBProjectionConcurrency;
    // go: sysvar.go:L2270 TiDBHashAggPartialConcurrency.Validation L1
    v_tidb_hashagg_partial_concurrency => TiDBHashAggPartialConcurrency;
    // go: sysvar.go:L2277 TiDBHashAggFinalConcurrency.Validation L1
    v_tidb_hashagg_final_concurrency => TiDBHashAggFinalConcurrency;
    // go: sysvar.go:L2284 TiDBWindowConcurrency.Validation L1
    v_tidb_window_concurrency => TiDBWindowConcurrency;
    // go: sysvar.go:L2291 TiDBMergeJoinConcurrency.Validation L1
    v_tidb_merge_join_concurrency => TiDBMergeJoinConcurrency;
    // go: sysvar.go:L2298 TiDBStreamAggConcurrency.Validation L1
    v_tidb_streamagg_concurrency => TiDBStreamAggConcurrency;
    // go: sysvar.go:L2305 TiDBIndexMergeIntersectionConcurrency.Validation L1
    v_tidb_index_merge_intersection_concurrency => TiDBIndexMergeIntersectionConcurrency;
}

macro_rules! always_on_when_off {
    ($($(#[$m:meta])* $f:ident => $msg:expr;)*) => {$(
        $(#[$m])*
        fn $f(vars: &mut SessionVars, s: &str, _o: &str, _flag: ScopeFlag) -> ValResult {
            if s == Off {
                vars.stmt_ctx.append_warning(errors::new($msg));
            }
            ok(On)
        }
    )*};
}

always_on_when_off! {
    // go: sysvar.go:L2239 TiDBEnableTablePartition.Validation L1
    v_tidb_enable_table_partition => "tidb_enable_table_partition is always turned on. This variable has been deprecated and will be removed in the future releases";
    // go: sysvar.go:L2634 TiDBEnableExchangePartition.Validation L1
    v_tidb_enable_exchange_partition => "tidb_enable_exchange_partition is always turned on. This variable has been deprecated and will be removed in the future releases";
    // go: sysvar.go:L2874 TiDBEnableTiFlashReadForWriteStmt.Validation L1
    v_tidb_enable_tiflash_read_for_write_stmt => "tidb_enable_tiflash_read_for_write_stmt is always turned on. This variable has been deprecated and will be removed in the future releases";
}

// go: sysvar.go:L2246 TiDBEnableListTablePartition.Validation L1
fn v_tidb_enable_list_partition(vars: &mut SessionVars, normalized_value: &str, _o: &str, _scope: ScopeFlag) -> ValResult {
    vars.stmt_ctx.append_warning(ErrWarnDeprecatedSyntaxSimpleMsg.fast_gen_by_args(vec![a(TiDBEnableListTablePartition)]));
    if !tidb_opt_on(normalized_value) {
        return fail(normalized_value, errors::errorf("tidb_enable_list_partition is now always on, and cannot be turned off", &[]));
    }
    ok(normalized_value)
}

// go: sysvar.go:L2337 TiDBDisableTxnAutoRetry.Validation L1
fn v_tidb_disable_txn_auto_retry(vars: &mut SessionVars, normalized_value: &str, _o: &str, _scope: ScopeFlag) -> ValResult {
    if normalized_value == Off {
        vars.stmt_ctx.append_warning(errWarnDeprecatedSyntax.fast_gen_by_args(vec![a(Off), a(On)]));
    }
    ok(On)
}

// go: sysvar.go:L2384 TiDBEnableFastAnalyze.Validation L1
fn v_tidb_enable_fast_analyze(vars: &mut SessionVars, normalized_value: &str, _o: &str, _scope: ScopeFlag) -> ValResult {
    if tidb_opt_on(normalized_value) {
        vars.stmt_ctx.append_warning(errors::new(
            "the fast analyze feature has already been removed in TiDB v7.5.0, so this will have no effect",
        ));
    }
    ok(normalized_value)
}

// go: sysvar.go:L2399 TiDBAllowFallbackToTiKV.Validation L1
fn v_tidb_allow_fallback_to_tikv(_vars: &mut SessionVars, normalized_value: &str, _o: &str, _scope: ScopeFlag) -> ValResult {
    use tidb_base::kv::TiFlash;
    if normalized_value.is_empty() {
        return ok("");
    }
    let mut format_val = String::new();
    let mut seen_tiflash = false;
    for (i, engine) in normalized_value.split(',').enumerate() {
        let engine = trim_space(engine);
        if gostd::equal_fold(engine, TiFlash.name()) {
            if !seen_tiflash {
                if i != 0 {
                    format_val.push(',');
                }
                format_val.push_str(TiFlash.name());
                seen_tiflash = true;
            }
        } else {
            return fail(
                normalized_value,
                ErrWrongValueForVar.gen_with_stack_by_args(vec![a(TiDBAllowFallbackToTiKV), a(normalized_value)]),
            );
        }
    }
    ok(format_val)
}

// go: sysvar.go:L2455 TiDBEnableNoopFuncs.Validation L1
fn v_tidb_enable_noop_functions(vars: &mut SessionVars, normalized_value: &str, original_value: &str, scope: ScopeFlag) -> ValResult {
    // The behavior is very weird if someone can turn TiDBEnableNoopFuncs OFF, but keep any of the following on:
    // TxReadOnly, TransactionReadOnly, OfflineMode, SuperReadOnly, serverReadOnly, SQLAutoIsNull
    // To prevent this strange position, prevent setting to OFF when any of these sysVars are ON of the same scope.
    if normalized_value == Off {
        for potential in [TxReadOnly, TransactionReadOnly, OfflineMode, SuperReadOnly, ReadOnly, SQLAutoIsNull] {
            let (mut val, _) = vars.get_system_var(potential); // session scope
            if scope == ScopeGlobal {
                // global scope
                val = match vars.global_vars_accessor.get_global_sys_var(potential) {
                    Ok(v) => v,
                    Err(_) => {
                        return fail(original_value, errUnknownSystemVariable.gen_with_stack_by_args(vec![a(potential)]));
                    }
                };
            }
            if tidb_opt_on(&val) {
                return fail(
                    original_value,
                    errValueNotSupportedWhen.gen_with_stack_by_args(vec![a(TiDBEnableNoopFuncs), a(potential)]),
                );
            }
        }
    }
    ok(normalized_value)
}

// go: sysvar.go:L2505 TiDBEvolvePlanBaselines.Validation L1
fn v_tidb_evolve_plan_baselines(_vars: &mut SessionVars, normalized_value: &str, _o: &str, _scope: ScopeFlag) -> ValResult {
    if normalized_value == "ON" && !tidb_base::config::CheckTableBeforeDrop.load(SeqCst) {
        return fail(normalized_value, errors::errorf("Cannot enable baseline evolution feature, it is not generally available now", &[]));
    }
    ok(normalized_value)
}

// go: sysvar.go:L2526 TiDBEnableClusteredIndex.Validation L1
fn v_tidb_enable_clustered_index(vars: &mut SessionVars, normalized_value: &str, _o: &str, _scope: ScopeFlag) -> ValResult {
    if normalized_value == IntOnly {
        vars.stmt_ctx.append_warning(
            errWarnDeprecatedSyntax.fast_gen_by_args(vec![a(normalized_value), a(&format!("'{On}' or '{Off}'"))]),
        );
    }
    ok(normalized_value)
}

// go: sysvar.go:L2536 TiDBEnableGlobalIndex.Validation L1
fn v_tidb_enable_global_index(vars: &mut SessionVars, normalized_value: &str, _o: &str, _scope: ScopeFlag) -> ValResult {
    if !tidb_opt_on(normalized_value) {
        vars.stmt_ctx.append_warning(errors::new(
            "tidb_enable_global_index is always turned on. This variable has been deprecated and will be removed in the future releases",
        ));
    }
    ok(normalized_value)
}

/// PartitionPruneMode presents the prune mode used (session.go, block D05.2).
pub(crate) mod partition_prune_mode {
    /// Static indicates only prune at plan phase.
    pub const Static: &str = "static";
    /// Dynamic indicates only prune at execute phase.
    pub const Dynamic: &str = "dynamic";
    /// StaticOnly is out-of-date.
    pub const StaticOnly: &str = "static-only";
    /// DynamicOnly is out-of-date.
    pub const DynamicOnly: &str = "dynamic-only";
    /// StaticButPrepareDynamic is out-of-date.
    pub const StaticButPrepareDynamic: &str = "static-collect-dynamic";

    /// Valid indicate PruneMode is validated.
    // go: PartitionPruneMode.Valid L1
    pub fn valid(p: &str) -> bool {
        matches!(p, Static | Dynamic | StaticOnly | DynamicOnly)
    }

    /// Update updates out-of-date PruneMode.
    // go: PartitionPruneMode.Update L1
    pub fn update(p: &str) -> &str {
        match p {
            StaticOnly | StaticButPrepareDynamic => Static,
            DynamicOnly => Dynamic,
            _ => p,
        }
    }
}

// go: sysvar.go:L2542 TiDBPartitionPruneMode.Validation L1
fn v_tidb_partition_prune_mode(_vars: &mut SessionVars, normalized_value: &str, _o: &str, _scope: ScopeFlag) -> ValResult {
    let mode = partition_prune_mode::update(normalized_value);
    if !partition_prune_mode::valid(mode) {
        return fail(normalized_value, ErrWrongTypeForVar.gen_with_stack_by_args(vec![a(TiDBPartitionPruneMode)]));
    }
    ok(mode)
}

// go: sysvar.go:L2593 TiDBAnalyzeVersion.Validation L1
fn v_tidb_analyze_version(vars: &mut SessionVars, normalized_value: &str, _o: &str, _scope: ScopeFlag) -> ValResult {
    if normalized_value == "1" {
        vars.stmt_ctx.append_warning(
            errWarnDeprecatedSyntax.fast_gen_by_args(vec![a("tidb_analyze_version=1"), a("tidb_analyze_version=2")]),
        );
    }
    ok(normalized_value)
}

/// joinversion.HashJoinVersionLegacy / HashJoinVersionOptimized.
const HashJoinVersionLegacy: &str = "legacy";
const HashJoinVersionOptimized: &str = "optimized";

// go: sysvar.go:L2604 TiDBHashJoinVersion.Validation L1
fn v_tidb_hash_join_version(_vars: &mut SessionVars, normalized_value: &str, original_value: &str, _scope: ScopeFlag) -> ValResult {
    let lower_value = gostd::to_lower(normalized_value);
    if lower_value != HashJoinVersionLegacy && lower_value != HashJoinVersionOptimized {
        let err = errors::errorf(
            "incorrect value: `%s`. %s options: %s",
            &[a(original_value), a(TiDBHashJoinVersion), a(&format!("{HashJoinVersionLegacy}, {HashJoinVersionOptimized}"))],
        );
        return fail(normalized_value, err);
    }
    ok(normalized_value)
}

// go: sysvar.go:L2726 TiDBEnableNewCostInterface.Validation L1
fn v_tidb_enable_new_cost_interface(vars: &mut SessionVars, s: &str, _o: &str, _flag: ScopeFlag) -> ValResult {
    if s == Off {
        vars.stmt_ctx.append_warning(errWarnDeprecatedSyntax.fast_gen_by_args(vec![a(Off), a(On)]));
    }
    ok(On)
}

// go: sysvar.go:L2761 TiDBMemQuotaQuery.Validation L1
fn v_tidb_mem_quota_query(vars: &mut SessionVars, normalized_value: &str, original_value: &str, _scope: ScopeFlag) -> ValResult {
    let int_val = tidb_opt_int64(normalized_value, DefTiDBMemQuotaQuery);
    if int_val > 0 && int_val < 128 {
        vars.stmt_ctx.append_warning(ErrTruncatedWrongValue.fast_gen_by_args(vec![a(TiDBMemQuotaQuery), a(original_value)]));
        return ok("128");
    }
    ok(normalized_value)
}

// go: sysvar.go:L2930 TiDBOptPartialOrderedIndexForTopN.Validation L1
fn v_tidb_opt_partial_ordered_index_for_topn(_vars: &mut SessionVars, normalized_value: &str, original_value: &str, _scope: ScopeFlag) -> ValResult {
    let lower_value = gostd::to_lower(trim_space(original_value));
    if lower_value != "disable" && lower_value != "cost" {
        return fail(
            normalized_value,
            ErrWrongValueForVar.gen_with_stack_by_args(vec![a(TiDBOptPartialOrderedIndexForTopN), a(original_value)]),
        );
    }
    ok(normalized_value)
}

/// `kv.ExchangeCompressionMode` names (pkg/kv/mpp.go, deferred with MPP: DEF.MPP): NONE,
/// FAST, HIGH_COMPRESSION (tipb.CompressionMode), then UNSPECIFIED.
const EXCHANGE_COMPRESSION_MODE_NAMES: [&str; 4] = ["NONE", "FAST", "HIGH_COMPRESSION", "UNSPECIFIED"];

/// `kv.ToExchangeCompressionMode` (ok only).
// glue: port of pkg/kv.ToExchangeCompressionMode (block DEF.MPP), validity only
fn to_exchange_compression_mode_ok(name: &str) -> bool {
    EXCHANGE_COMPRESSION_MODE_NAMES.contains(&gostd::to_upper(name).as_str())
}

// go: sysvar.go:L3015 MppExchangeCompressionMode.Validation L1
fn v_mpp_exchange_compression_mode(_vars: &mut SessionVars, normalized_value: &str, original_value: &str, _scope: ScopeFlag) -> ValResult {
    if !to_exchange_compression_mode_ok(normalized_value) {
        let msg = EXCHANGE_COMPRESSION_MODE_NAMES.join(", ");
        let err = errors::errorf(
            "incorrect value: `%s`. %s options: %s",
            &[a(original_value), a(MppExchangeCompressionMode), a(&msg)],
        );
        return fail(normalized_value, err);
    }
    ok(normalized_value)
}

/// `kv.MppVersionUnspecified` .. `kv.GetNewestMppVersion()` (pkg/kv/mpp.go).
const MppVersionUnspecified: i64 = -1;

/// `kv.ToMppVersion` (ok only).
// glue: port of pkg/kv.ToMppVersion (block DEF.MPP), validity only
fn to_mpp_version_ok(name: &str) -> bool {
    let name = gostd::to_upper(name);
    if name == "UNSPECIFIED" {
        return true;
    }
    let (v, err) = tidb_types::gostd::parse_int(name.as_bytes(), 64);
    err.is_ok() && (MppVersionUnspecified..=tidb_kv::mpp::get_newest_mpp_version().0).contains(&v)
}

// go: sysvar.go:L3043 MppVersion.Validation L1
fn v_mpp_version(_vars: &mut SessionVars, normalized_value: &str, original_value: &str, _scope: ScopeFlag) -> ValResult {
    if to_mpp_version_ok(normalized_value) {
        return ok(normalized_value);
    }
    let mut err_msg = format!("incorrect value: {original_value}. {MppVersion} options: {MppVersionUnspecified} (unspecified)");
    for i in 0..=tidb_kv::mpp::get_newest_mpp_version().0 {
        err_msg = format!("{err_msg}, {i}");
    }
    fail(normalized_value, errors::new(err_msg))
}

// go: sysvar.go:L3224 TiDBAnalyzeSkipColumnTypes.Validation L1
fn v_tidb_analyze_skip_column_types(_vars: &mut SessionVars, normalized_value: &str, _o: &str, _scope: ScopeFlag) -> ValResult {
    valid_analyze_skip_column_types(normalized_value)
}

/// ToRuntimeFilterType (session.go, block D05.2): validity of a comma-separated list of
/// runtime filter type names ("IN", "MIN_MAX").
// go: ToRuntimeFilterType L2 (validity only; the type list is built with D05.2)
pub(crate) fn to_runtime_filter_type_ok(session_var_value: &str) -> bool {
    session_var_value.split(',').all(|t| matches!(gostd::to_upper(t).as_str(), "IN" | "MIN_MAX"))
}

// go: sysvar.go:L3422 TiDBRuntimeFilterTypeName.Validation L1
fn v_tidb_runtime_filter_type(_vars: &mut SessionVars, normalized_value: &str, original_value: &str, _scope: ScopeFlag) -> ValResult {
    if to_runtime_filter_type_ok(normalized_value) {
        return ok(normalized_value);
    }
    let err_msg = format!(
        "incorrect value: {original_value}. {TiDBRuntimeFilterTypeName} should be sepreated by , such as {DefRuntimeFilterType}, also we only support IN and MIN_MAX now. "
    );
    fail(normalized_value, errors::new(err_msg))
}

/// RuntimeFilterModeStringToMode (session.go, block D05.2), validity only.
// go: RuntimeFilterModeStringToMode L2 (validity only)
pub(crate) fn runtime_filter_mode_ok(name: &str) -> bool {
    matches!(name, "OFF" | "LOCAL")
}

// go: sysvar.go:L3437 TiDBRuntimeFilterModeName.Validation L1
fn v_tidb_runtime_filter_mode(_vars: &mut SessionVars, normalized_value: &str, original_value: &str, _scope: ScopeFlag) -> ValResult {
    if runtime_filter_mode_ok(normalized_value) {
        return ok(normalized_value);
    }
    let err_msg = format!("incorrect value: {original_value}. {TiDBRuntimeFilterModeName} options: {DefRuntimeFilterMode} ");
    fail(normalized_value, errors::new(err_msg))
}

// go: sysvar.go:L3468 TiDBSchemaCacheSize.Validation L1
fn v_tidb_schema_cache_size(s: &mut SessionVars, normalized_value: &str, original_value: &str, _scope: ScopeFlag) -> ValResult {
    let (_, st, err) = parse_schema_cache_size(s, normalized_value, original_value);
    if let Err(e) = err {
        return fail("", e);
    }
    ok(st)
}

// go: sysvar.go:L3494 TiDBSessionAlias.Validation L1
fn v_tidb_session_alias(s: &mut SessionVars, normalized_value: &str, original_value: &str, _scope: ScopeFlag) -> ValResult {
    // []rune(normalizedValue): invalid UTF-8 is not possible in a Rust str.
    let mut chars: Vec<char> = normalized_value.chars().collect();
    let mut normalized_value = normalized_value.to_owned();
    let mut warning_added = false;
    if chars.len() > 64 {
        s.stmt_ctx.append_warning(ErrTruncatedWrongValue.fast_gen_by_args(vec![a(TiDBSessionAlias), a(original_value)]));
        warning_added = true;
        chars.truncate(64);
        normalized_value = chars.iter().collect();
    }
    // truncate to a valid identifier
    while !normalized_value.is_empty() && super::util::is_in_correct_identifier_name(&normalized_value) {
        if !warning_added {
            s.stmt_ctx.append_warning(ErrTruncatedWrongValue.fast_gen_by_args(vec![a(TiDBSessionAlias), a(original_value)]));
            warning_added = true;
        }
        chars.pop();
        normalized_value = chars.iter().collect();
    }
    ok(normalized_value)
}

// go: sysvar.go:L3534 TiDBServiceScope.Validation L1
fn v_tidb_service_scope(_vars: &mut SessionVars, normalized_value: &str, original_value: &str, _scope: ScopeFlag) -> ValResult {
    (normalized_value.to_owned(), super::util::check_service_scope(original_value))
}

/// ForcePreAggStr means 1st hashagg will be pre aggregated (session.go, block D05.2).
const ForcePreAggStr: &str = "force_preagg";
/// AutoStr means TiFlash will decide which policy for 1st hashagg.
const AutoStr: &str = "auto";
/// ForceStreamingStr means 1st hashagg will for pass through all blocks.
const ForceStreamingStr: &str = "force_streaming";

/// ValidTiFlashPreAggMode returns all valid modes.
// go: ValidTiFlashPreAggMode L1
pub(crate) fn valid_tiflash_pre_agg_mode() -> String {
    format!("{ForcePreAggStr}, {AutoStr}, {ForceStreamingStr}")
}

// go: sysvar.go:L3581 TiFlashHashAggPreAggMode.Validation L1 (ToTiPBTiFlashPreAggMode validity)
fn v_tiflash_hashagg_preaggregation_mode(_vars: &mut SessionVars, normalized_value: &str, original_value: &str, _scope: ScopeFlag) -> ValResult {
    if matches!(normalized_value, ForcePreAggStr | ForceStreamingStr | AutoStr) {
        return ok(normalized_value);
    }
    let err_msg = format!("incorrect value: `{original_value}`. {TiFlashHashAggPreAggMode} options: {}", valid_tiflash_pre_agg_mode());
    fail(normalized_value, errors::new(err_msg))
}

// go: sysvar.go:L3606 TiDBMaxDistTaskNodes.Validation L1
fn v_tidb_max_dist_task_nodes(_s: &mut SessionVars, normalized_value: &str, _o: &str, _scope: ScopeFlag) -> ValResult {
    let max_nodes = tidb_opt_int(normalized_value, DefTiDBMaxDistTaskNodes);
    if max_nodes == 0 {
        return fail(normalized_value, errors::new("max_dist_task_nodes should be -1 or [1, 128]"));
    }
    ok(normalized_value)
}

// go: noop.go:L32 TxReadOnly.Validation L1 (also TransactionReadOnly L35, SuperReadOnly L41, ReadOnly L44)
fn v_check_read_only(vars: &mut SessionVars, normalized_value: &str, original_value: &str, scope: ScopeFlag) -> ValResult {
    check_read_only(vars, normalized_value, original_value, scope, false)
}

// go: noop.go:L38 OfflineMode.Validation L1
fn v_offline_mode(vars: &mut SessionVars, normalized_value: &str, original_value: &str, scope: ScopeFlag) -> ValResult {
    check_read_only(vars, normalized_value, original_value, scope, true)
}

// go: noop.go:L176 CharacterSetFilesystem.Validation L1
fn v_character_set_filesystem(_vars: &mut SessionVars, normalized_value: &str, _o: &str, _scope: ScopeFlag) -> ValResult {
    check_character_set(normalized_value, CharacterSetFilesystem)
}

// go: noop.go:L239 SQLAutoIsNull.Validation L1
fn v_sql_auto_is_null(vars: &mut SessionVars, normalized_value: &str, original_value: &str, scope: ScopeFlag) -> ValResult {
    // checkSQLAutoIsNull requires TiDBEnableNoopFuncs != OFF for the same scope otherwise an error will be returned.
    // See also https://github.com/pingcap/tidb/issues/28230
    let err_msg = ErrFunctionsNoopImpl.fast_gen_by_args(vec![a("sql_auto_is_null")]);
    if tidb_opt_on(normalized_value) {
        if scope == ScopeSession && vars.noop_funcs_mode != OnInt {
            if vars.noop_funcs_mode == OffInt {
                return fail(Off, err_msg);
            }
            vars.stmt_ctx.append_warning(err_msg.clone());
        }
        if scope == ScopeGlobal {
            let val = match vars.global_vars_accessor.get_global_sys_var(TiDBEnableNoopFuncs) {
                Ok(v) => v,
                Err(_) => {
                    return fail(original_value, errUnknownSystemVariable.gen_with_stack_by_args(vec![a(TiDBEnableNoopFuncs)]));
                }
            };
            if val == Off {
                return fail(Off, err_msg);
            }
            if val == "WARN" {
                vars.stmt_ctx.append_warning(err_msg);
            }
        }
    }
    ok(normalized_value)
}

// go: noop.go:L441 SecureAuth.Validation L1
fn v_secure_auth(_vars: &mut SessionVars, normalized_value: &str, original_value: &str, _scope: ScopeFlag) -> ValResult {
    if tidb_opt_on(normalized_value) {
        return ok(On);
    }
    fail(normalized_value, ErrWrongValueForVar.gen_with_stack_by_args(vec![a(SecureAuth), a(original_value)]))
}
