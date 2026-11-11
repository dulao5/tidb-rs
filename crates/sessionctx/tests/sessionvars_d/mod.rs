//! Replay of the D05.2 PR D ops of the govec domain "sessionvars" (`methods`, `uservars`,
//! `paramlist`, `svmisc`; tracker govec/domains/sessionvars_d.go). Included by
//! `govec_sessionvars.rs`.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::Ordering::SeqCst;

use serde_json::Value;
use serde_json::json;
use tidb_base::Error;
use tidb_sessionctx::variable::plan_cache_params::{PlanCacheParamList, new_plan_cache_param_list};
use tidb_sessionctx::variable::session::SessionVars;
use tidb_sessionctx::variable::session_methods as sm;
use tidb_sessionctx::variable::session_state::{PlanReplayerTaskKey, TxnReadTS};
use tidb_sessionctx::variable::tidb_vars;
use tidb_sessionctx::variable::uservars::{
    UserVars, UserVarsReader, UserVarsReaderRef, new_user_vars,
};
use tidb_types::datum::Datum;
use tidb_types::etc::FieldType;

/// A `vec.GoString`: a JSON string, or `{"hex": ...}` for bytes that are not UTF-8.
fn go_string(v: &Value) -> Vec<u8> {
    match v {
        Value::String(s) => s.as_bytes().to_vec(),
        Value::Object(m) => hex::decode(m["hex"].as_str().unwrap()),
        _ => panic!("bad GoString {v}"),
    }
}

mod hex {
    pub fn decode(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }
    pub fn encode(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }
}

fn datum_in(p: &Value) -> Datum {
    let p = p.as_array().unwrap();
    let mut d = Datum::default();
    match p[0].as_str().unwrap() {
        "null" => d.set_null(),
        "i" => d.set_int64(p[1].as_i64().unwrap()),
        "u" => d.set_uint64(p[1].as_u64().unwrap()),
        "f" => d.set_float64(f64::from_bits(p[1].as_u64().unwrap())),
        "s" => d.set_string(go_string(&p[1]), p[2].as_str().unwrap().to_owned()),
        "b" => d.set_bytes(hex::decode(p[1].as_str().unwrap())),
        k => panic!("bad datum kind {k}"),
    }
    d
}

fn datum_out(d: &Datum) -> Value {
    let mut o = serde_json::Map::new();
    o.insert("k".into(), json!(d.kind()));
    match d.to_string() {
        Ok(s) => {
            o.insert("s".into(), json!(hex::encode(&s)));
        }
        Err(e) => {
            o.insert("s".into(), json!(""));
            o.insert("err".into(), json!(String::from_utf8_lossy(&e.error())));
        }
    }
    o.insert("coll".into(), json!(d.collation()));
    Value::Object(o)
}

fn ft_in(p: &Value) -> FieldType {
    let p = p.as_array().unwrap();
    tidb_types::field_type_builder::new_field_type_builder()
        .set_type(p[0].as_u64().unwrap() as u8)
        .set_flag(p[1].as_u64().unwrap() as usize)
        .set_flen(p[2].as_i64().unwrap() as isize)
        .set_decimal(p[3].as_i64().unwrap() as isize)
        .set_charset(p[4].as_str().unwrap())
        .set_collate(p[5].as_str().unwrap())
        .build_p()
}

fn ft_out(ft: &FieldType) -> Value {
    json!([
        ft.get_type(),
        ft.get_flag(),
        ft.get_flen(),
        ft.get_decimal(),
        ft.get_charset(),
        ft.get_collate()
    ])
}

/// Go marshals a nil slice as `null`.
fn list_or_null(v: Vec<Value>) -> Value {
    if v.is_empty() {
        Value::Null
    } else {
        Value::Array(v)
    }
}

fn f64s(f: f64) -> String {
    f.to_bits().to_string()
}

fn err_str(r: Result<(), Error>) -> Value {
    match r {
        Ok(()) => Value::Null,
        Err(e) => json!(String::from_utf8_lossy(&e.error())),
    }
}

fn mdl_len(vars: &SessionVars) -> i64 {
    match vars.state.txn_ctx.related_table_for_mdl() {
        None => -1,
        Some(m) => m.0.lock().len() as i64,
    }
}

fn user_vars_dump(uv: &UserVars) -> Value {
    let mut out = vec![];
    for n in ["a", "b", "cd", "A", "Cd"] {
        if let Some(d) = uv.get_user_var_val(n) {
            out.push(json!([n, datum_out(&d)]));
        }
    }
    list_or_null(out)
}

fn getters(sv: &mut SessionVars) -> Value {
    let engines: Vec<String> = sv
        .get_isolation_read_engines()
        .iter()
        .map(|e| e.name().to_owned())
        .collect();
    let mut engines = engines;
    engines.sort();
    let mut fix: Vec<String> = sv
        .get_optimizer_fix_control_map()
        .iter()
        .map(|(k, v)| format!("{k}:{v}"))
        .collect();
    fix.sort();
    // Sorted, as the generator (Go builds the list from a map: M-MAPORDER).
    let mut rf_types: Vec<&str> = sv
        .get_runtime_filter_types()
        .iter()
        .map(|t| sm::runtime_filter_type_string(*t))
        .collect();
    rf_types.sort();
    let tmp = tidb_model::TableInfo {
        temp_table_type: tidb_model::TempTableLocal,
        ..Default::default()
    };
    let plain = tidb_model::TableInfo::default();
    let factors = |f: &dyn Fn(Option<&tidb_model::TableInfo>) -> f64| {
        json!([f64s(f(None)), f64s(f(Some(&tmp))), f64s(f(Some(&plain)))])
    };
    let (cs, coll) = sv.get_charset_info();
    let read_ts = match &sv.state.txn_read_ts {
        Some(t) => json!([t.peak_txn_read_ts(), t.used()]),
        None => json!([null, null]),
    };
    let mut m = json!({
        "IndexLookupConcurrency": sv.index_lookup_concurrency(),
        "IndexLookupJoinConcurrency": sv.index_lookup_join_concurrency(),
        "DistSQLScanConcurrency": sv.dist_sql_scan_concurrency(),
        "AnalyzeDistSQLScanConcurrency": sv.analyze_dist_sql_scan_concurrency(),
        "HashJoinConcurrency": sv.hash_join_concurrency(),
        "ProjectionConcurrency": sv.projection_concurrency(),
        "HashAggPartialConcurrency": sv.hash_agg_partial_concurrency(),
        "HashAggFinalConcurrency": sv.hash_agg_final_concurrency(),
        "WindowConcurrency": sv.window_concurrency(),
        "MergeJoinConcurrency": sv.merge_join_concurrency(),
        "StreamAggConcurrency": sv.stream_agg_concurrency(),
        "IndexMergeIntersectionConcurrency": sv.index_merge_intersection_concurrency(),
        "IndexSerialScanConcurrency": sv.index_serial_scan_concurrency(),
        "UnionConcurrency": sv.union_concurrency(),
        "IsPlanReplayerCaptureEnabled": sv.is_plan_replayer_capture_enabled(),
        "IsMPPAllowed": sv.is_mpp_allowed(),
        "IsTiFlashCopBanned": sv.is_tiflash_cop_banned(),
        "IsMPPEnforced": sv.is_mpp_enforced(),
        "ChooseMppExchangeCompressionMode": sv.choose_mpp_exchange_compression_mode(),
        "IsPartialOrderedIndexForTopNEnabled": sv.is_partial_ordered_index_for_top_n_enabled(),
        "CheckAndGetTxnScope": sv.check_and_get_txn_scope(),
        "IsDynamicPartitionPruneEnabled": sv.is_dynamic_partition_prune_enabled(),
        "IsRowLevelChecksumEnabled": sv.is_row_level_checksum_enabled(),
        "GetAllowPreferRangeScan": sv.get_allow_prefer_range_scan(),
        "GetEnableIndexMerge": sv.get_enable_index_merge(),
        "GetEnablePseudoForOutdatedStats": sv.get_enable_pseudo_for_outdated_stats(),
        "IsReplicaReadClosestAdaptive": sv.is_replica_read_closest_adaptive(),
        "GetSplitRegionTimeout": sv.get_split_region_timeout(),
        "GetIsolationReadEngines": engines,
        "GetCharsetInfo": [cs, coll],
        "IsolationLevelForNewTxn": sv.isolation_level_for_new_txn(),
        "GetReadableTxnMode": sv.get_readable_txn_mode(),
        "GetPrevStmtDigest": sv.get_prev_stmt_digest(),
        "GetDivPrecisionIncrement": sv.get_div_precision_increment(),
        "GetCPUFactor": f64s(sv.get_cpu_factor()),
        "GetCopCPUFactor": f64s(sv.get_cop_cpu_factor()),
        "GetMemoryFactor": f64s(sv.get_memory_factor()),
        "GetDiskFactor": f64s(sv.get_disk_factor()),
        "GetConcurrencyFactor": f64s(sv.get_concurrency_factor()),
        "GetNetworkFactor": factors(&|t| sv.get_network_factor(t)),
        "GetScanFactor": factors(&|t| sv.get_scan_factor(t)),
        "GetDescScanFactor": factors(&|t| sv.get_desc_scan_factor(t)),
        "GetSeekFactor": factors(&|t| sv.get_seek_factor(t)),
        "EnableEvalTopNEstimationForStrMatch": sv.enable_eval_top_n_estimation_for_str_match(),
        "GetStrMatchDefaultSelectivity": f64s(sv.get_str_match_default_selectivity()),
        "GetNegateStrMatchDefaultSelectivity": f64s(sv.get_negate_str_match_default_selectivity()),
        "EnableForceInlineCTE": sv.enable_force_inline_cte(),
        "IsRuntimeFilterEnabled": sv.is_runtime_filter_enabled(),
        "GetRuntimeFilterTypes": rf_types,
        "GetRuntimeFilterMode": sm::runtime_filter_mode_string(sv.get_runtime_filter_mode()),
        "GetTiKVClientReadTimeout": sv.get_tikv_client_read_timeout(),
        "GetOptObjective": sv.get_opt_objective(),
        "UseLowResolutionTSO": sv.use_low_resolution_tso(),
        "GetOptimizerFixControlMap": fix,
        "GetForUpdateTS": sv.state.txn_ctx.get_for_update_ts(),
        "TxnReadTS": read_ts,
        "SnapshotInfoschema": sv.state.snapshot_infoschema,
        "ResourceGroupName": sv.resource_group_name,
        "PreparedStmtCount": sm::PreparedStmtCount.load(SeqCst),
        "PreparedStmts": sv.state.prepared_stmts.len(),
        "TxnCtx.Isolation": sv.state.txn_ctx.isolation,
        "MDL": mdl_len(sv),
        "UserVars": user_vars_dump(&sv.state.user_vars),
        "GetSessionVars": std::ptr::eq(sv.get_session_vars(), &*sv),
        "Location": sv.location().string(),
    });
    m["IsPessimisticReadConsistency"] = json!(sv.is_pessimistic_read_consistency());
    m["TxnCtx.Isolation.after"] = json!(sv.state.txn_ctx.isolation);
    m
}

fn key(c: &[Value]) -> PlanReplayerTaskKey {
    PlanReplayerTaskKey {
        sql_digest: c[1].as_str().unwrap().to_owned(),
        plan_digest: c[2].as_str().unwrap().to_owned(),
    }
}

fn stmt_out(r: Result<Arc<dyn std::any::Any + Send + Sync>, Error>) -> Value {
    match r {
        Ok(v) => json!([*v.downcast_ref::<u32>().unwrap(), Value::Null]),
        Err(e) => json!([Value::Null, String::from_utf8_lossy(&e.error())]),
    }
}

fn call(sv: &mut SessionVars, c: &[Value]) -> Value {
    let i = |k: usize| c[k].as_i64().unwrap();
    let b = |k: usize| c[k].as_bool().unwrap();
    let s = |k: usize| c[k].as_str().unwrap();
    match s(0) {
        "SetIndexLookupConcurrency" => sv.set_index_lookup_concurrency(i(1)),
        "SetIndexLookupJoinConcurrency" => sv.set_index_lookup_join_concurrency(i(1)),
        "SetDistSQLScanConcurrency" => sv.set_dist_sql_scan_concurrency(i(1)),
        "SetAnalyzeDistSQLScanConcurrency" => sv.set_analyze_dist_sql_scan_concurrency(i(1)),
        "SetHashJoinConcurrency" => sv.set_hash_join_concurrency(i(1)),
        "SetProjectionConcurrency" => sv.set_projection_concurrency(i(1)),
        "SetHashAggPartialConcurrency" => sv.set_hash_agg_partial_concurrency(i(1)),
        "SetHashAggFinalConcurrency" => sv.set_hash_agg_final_concurrency(i(1)),
        "SetWindowConcurrency" => sv.set_window_concurrency(i(1)),
        "SetMergeJoinConcurrency" => sv.set_merge_join_concurrency(i(1)),
        "SetStreamAggConcurrency" => sv.set_stream_agg_concurrency(i(1)),
        "SetIndexMergeIntersectionConcurrency" => sv.set_index_merge_intersection_concurrency(i(1)),
        "SetIndexSerialScanConcurrency" => sv.set_index_serial_scan_concurrency(i(1)),
        "SetAllowInSubqToJoinAndAgg" => sv.set_allow_in_subq_to_join_and_agg(b(1)),
        "SetAllowPreferRangeScan" => sv.set_allow_prefer_range_scan(b(1)),
        "SetEnableCascadesPlanner" => sv.set_enable_cascades_planner(b(1)),
        "SetEnableIndexMerge" => sv.set_enable_index_merge(b(1)),
        "SetEnablePseudoForOutdatedStats" => sv.set_enable_pseudo_for_outdated_stats(b(1)),
        "SetPrevStmtDigest" => sv.set_prev_stmt_digest(s(1)),
        "SetResourceGroupName" => sv.set_resource_group_name(s(1)),
        "SetNextPreparedStmtID" => sv.set_next_prepared_stmt_id(c[1].as_u64().unwrap() as u32),
        "GetNextPreparedStmtID" => return json!(sv.get_next_prepared_stmt_id()),
        "AllocNewPlanID" => return json!(sv.alloc_new_plan_id()),
        "AllocPlanColumnID" => return json!(sv.alloc_plan_column_id()),
        "SetPlanIDs" => {
            sv.state.plan_id.0.store(i(1) as i32, SeqCst);
            sv.state.plan_column_id.0.store(i(2), SeqCst);
        }
        "SetStringUserVar" => {
            sv.set_string_user_var(s(1), &go_string(&c[2]), s(3));
            return match sv
                .state
                .user_vars
                .get_user_var_val(&tidb_base::gostd::to_lower(s(1)))
            {
                Some(d) => datum_out(&d),
                None => Value::Null,
            };
        }
        "IsIsolation" => {
            let r = sv.is_isolation(s(1));
            return json!([r, sv.state.txn_ctx.isolation]);
        }
        "SetTxnIsolationLevelOneShotStateForNextTxn" => {
            sv.set_txn_isolation_level_one_shot_state_for_next_txn()
        }
        "SetInTxn" => sv.set_in_txn(b(1)),
        "TxnCtx" => {
            sv.state.txn_ctx.start_ts = c[1].as_u64().unwrap();
            sv.state.txn_ctx.is_pessimistic = b(2);
            sv.state.txn_ctx.isolation = s(3).to_owned();
        }
        "SetForUpdateTS" => sv.state.txn_ctx.set_for_update_ts(c[1].as_u64().unwrap()),
        "TxnCtx.Cleanup" => {
            sv.state.txn_ctx.is_staleness = true;
            sv.state.txn_ctx.enable_mdl = true;
            sv.state.txn_ctx.cleanup();
            return json!([
                sv.state.txn_ctx.is_staleness,
                sv.state.txn_ctx.enable_mdl,
                mdl_len(sv)
            ]);
        }
        "SetTxnReadTS" => {
            TxnReadTS::set_txn_read_ts(sv.state.txn_read_ts.as_mut(), c[1].as_u64().unwrap())
        }
        "UseTxnReadTS" => return json!(TxnReadTS::use_txn_read_ts(sv.state.txn_read_ts.as_mut())),
        "SetSnapshotInfoschema" => sv.state.snapshot_infoschema = true,
        "CleanupTxnReadTSIfUsed" => sv.cleanup_txn_read_ts_if_used(),
        "AddPlanReplayerFinishedTaskKey" => {
            if sv.state.plan_replayer_finished_task_key.is_none() {
                sv.check_plan_replayer_finished_task_key(&key(c));
            }
            sv.add_plan_replayer_finished_task_key(key(c));
            return json!(
                sv.state
                    .plan_replayer_finished_task_key
                    .as_ref()
                    .unwrap()
                    .len()
            );
        }
        "CheckPlanReplayerFinishedTaskKey" => {
            return json!(sv.check_plan_replayer_finished_task_key(&key(c)));
        }
        "MDL" => {
            sv.get_related_table_for_mdl().0.lock().insert(i(1), i(2));
            return json!(mdl_len(sv));
        }
        "ClearRelatedTableForMDL" => {
            sv.clear_related_table_for_mdl();
            return json!(mdl_len(sv));
        }
        "AddPreparedStmt" => {
            let id = c[1].as_u64().unwrap() as u32;
            return err_str(sv.add_prepared_stmt(id, Arc::new(id)));
        }
        "RemovePreparedStmt" => sv.remove_prepared_stmt(c[1].as_u64().unwrap() as u32),
        "WithdrawAllPreparedStmt" => sv.withdraw_all_prepared_stmt(),
        "NamePreparedStmt" => {
            sv.state
                .prepared_stmt_name_to_id
                .insert(s(1).to_owned(), c[2].as_u64().unwrap() as u32);
        }
        "GetPreparedStmtByID" => {
            return stmt_out(sv.get_prepared_stmt_by_id(c[1].as_u64().unwrap() as u32));
        }
        "GetPreparedStmtByName" => return stmt_out(sv.get_prepared_stmt_by_name(s(1))),
        "SetMaxPreparedStmtCount" => tidb_vars::MaxPreparedStmtCountValue.store(i(1), SeqCst),
        "SetPreparedStmtCount" => sm::PreparedStmtCount.store(i(1), SeqCst),
        "SetEnableAdaptiveReplicaRead" => return json!(sm::set_enable_adaptive_replica_read(b(1))),
        "RegisterScalarSubQ" => {
            sv.register_scalar_sub_q(Arc::new(i(1)));
            return json!(sv.state.map_scalar_sub_q.len());
        }
        "Field" => match s(1) {
            "InRestrictedSQL" => sv.in_restricted_sql = b(2),
            "ConnectionID" => sv.connection_id = c[2].as_u64().unwrap(),
            "RowEncoder.Enable" => sv.state.row_encoder_enable = b(2),
            "EnableRowLevelChecksum" => sv.enable_row_level_checksum = b(2),
            "TiKVClientReadTimeout" => sv.tikv_client_read_timeout = c[2].as_u64().unwrap(),
            f => panic!("bad field {f}"),
        },
        n => panic!("bad call {n}"),
    }
    Value::Null
}

fn reset_globals() {
    sm::PreparedStmtCount.store(0, SeqCst);
    tidb_vars::MaxPreparedStmtCountValue.store(
        tidb_sessionctx::variable::tidb_defs::DefMaxPreparedStmtCount,
        SeqCst,
    );
    sm::set_enable_adaptive_replica_read(true);
}

fn uservars(steps: &[Value]) -> Value {
    let uv = new_user_vars();
    let mut clone: Option<UserVarsReaderRef> = None;
    let mut res = vec![];
    for c in steps {
        let c = c.as_array().unwrap();
        let mut readers: Vec<&dyn UserVarsReader> = vec![&*uv];
        if let Some(cl) = &clone {
            readers.push(&**cl);
        }
        let r = match c[0].as_str().unwrap() {
            "set" => {
                uv.set_user_var_val(c[1].as_str().unwrap(), datum_in(&c[2]));
                Value::Null
            }
            "unset" => {
                uv.unset_user_var(c[1].as_str().unwrap());
                Value::Null
            }
            "settype" => {
                uv.set_user_var_type(c[1].as_str().unwrap(), ft_in(&c[2]));
                Value::Null
            }
            "clone" => {
                clone = Some(uv.clone_reader());
                Value::Null
            }
            "get" => json!(
                readers
                    .iter()
                    .map(|r| match r.get_user_var_val(c[1].as_str().unwrap()) {
                        Some(d) => datum_out(&d),
                        None => Value::Null,
                    })
                    .collect::<Vec<_>>()
            ),
            "gettype" => json!(
                readers
                    .iter()
                    .map(|r| match r.get_user_var_type(c[1].as_str().unwrap()) {
                        Some(ft) => ft_out(&ft),
                        None => Value::Null,
                    })
                    .collect::<Vec<_>>()
            ),
            op => panic!("bad uservars op {op}"),
        };
        res.push(r);
    }
    list_or_null(res)
}

fn paramlist_step(p: &mut PlanCacheParamList, c: &[Value]) -> Value {
    match c[0].as_str().unwrap() {
        "append" => {
            let ds: Vec<Datum> = c[1].as_array().unwrap().iter().map(datum_in).collect();
            p.append(&ds);
            Value::Null
        }
        "reset" => {
            p.reset();
            Value::Null
        }
        "nonprep" => {
            p.set_for_non_prep_cache(c[1].as_bool().unwrap());
            Value::Null
        }
        "get" => datum_out(&p.get_param_value(c[1].as_i64().unwrap())),
        "all" => list_or_null(p.all_param_values().iter().map(datum_out).collect()),
        "string" => {
            let s = p.string();
            match String::from_utf8(s.clone()) {
                Ok(s) => json!(s),
                Err(_) => json!({"hex": hex::encode(&s)}),
            }
        }
        op => panic!("bad paramlist op {op}"),
    }
}

/// Executes a PR D op; `None` for the other ops.
pub fn exec_d(op: &str, args: &[Value]) -> Option<(Value, Option<Error>)> {
    Some(match op {
        "methods" => {
            reset_globals();
            let mut vars = super::fresh_vars();
            let none = vec![];
            let sets: Vec<Value> = args[0]
                .as_array()
                .unwrap_or(&none)
                .iter()
                .map(|p| {
                    err_str(vars.set_system_var(p[0].as_str().unwrap(), p[1].as_str().unwrap()))
                })
                .collect();
            let calls: Vec<Value> = args[1]
                .as_array()
                .unwrap_or(&none)
                .iter()
                .map(|c| call(&mut vars, c.as_array().unwrap()))
                .collect();
            let out = json!({"set": list_or_null(sets), "calls": list_or_null(calls),
                "get": getters(&mut vars)});
            reset_globals();
            (out, None)
        }
        "uservars" => (uservars(args[0].as_array().unwrap()), None),
        "paramlist" => {
            let mut p = new_plan_cache_param_list();
            let mut res = vec![];
            for c in args[0].as_array().unwrap() {
                let c = c.as_array().unwrap();
                match std::panic::catch_unwind(AssertUnwindSafe(|| paramlist_step(&mut p, c))) {
                    Ok(v) => res.push(v),
                    Err(e) => {
                        let msg = e
                            .downcast_ref::<String>()
                            .cloned()
                            .unwrap_or_else(|| e.downcast_ref::<&str>().unwrap().to_string());
                        return Some((Value::Null, Some(tidb_base::errors::new(msg))));
                    }
                }
            }
            (list_or_null(res), None)
        }
        "svmisc" => {
            let s = |k: usize| args[k].as_str().unwrap();
            let out = match s(0) {
                "secure" => json!(
                    sm::ConnectionInfo {
                        connection_type: s(1).to_owned(),
                        ..Default::default()
                    }
                    .is_secure_transport()
                ),
                "rftype" => json!(sm::runtime_filter_type_string(args[1].as_i64().unwrap())),
                "rfmode" => json!(sm::runtime_filter_mode_string(args[1].as_i64().unwrap())),
                "preagg" => {
                    let (m, ok) = sm::to_tipb_tiflash_pre_agg_mode(s(1));
                    json!([m as i32, ok])
                }
                "dedup" => json!(hex::encode(&sm::prepare_dedup_cache_key(
                    &go_string(&args[1]),
                    s(2),
                    s(3),
                    s(4),
                    tidb_base::mysql::SQLMode(args[5].as_i64().unwrap() as _),
                ))),
                "weak" => json!(sm::read_consistency_level_is_weak(s(1))),
                "rewrite" => {
                    let mut r = sm::RewritePhaseInfo {
                        duration_rewrite: 1,
                        duration_preprocess_sub_query: 2,
                        preprocess_sub_queries: 3,
                    };
                    r.reset();
                    json!([
                        r.duration_rewrite,
                        r.duration_preprocess_sub_query,
                        r.preprocess_sub_queries
                    ])
                }
                "adaptive" => {
                    let mut outs = vec![];
                    for x in args[1].as_array().unwrap() {
                        outs.push(json!(sm::set_enable_adaptive_replica_read(
                            x.as_bool().unwrap()
                        )));
                        outs.push(json!(sm::is_adaptive_replica_read_enabled()));
                    }
                    sm::set_enable_adaptive_replica_read(true);
                    list_or_null(outs)
                }
                k => panic!("bad svmisc {k}"),
            };
            (out, None)
        }
        _ => return None,
    })
}
