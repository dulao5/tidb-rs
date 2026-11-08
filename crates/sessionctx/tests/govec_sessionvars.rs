//! Replay of the govec domain "sessionvars" (block D05.2): Go TiDB v8.5.8 vectors.
//!
//! - `fresh`: the scalar fields of a fresh `NewSessionVars(nil)` against
//!   `SessionVarsFields::default()` (T2).
//! - `set`, `get`, `set_global`, `init`: the system variable access of `SessionVars` and the
//!   session hooks (T3/T4, T11, T12) on a fresh session with the test accessor holding every
//!   default, TimeZone UTC, the process-wide state reset and MemTotal 64 GiB — as the Go
//!   generator does. Outputs, errors and warnings must match by message.
//!
//! A record that exercises a hook still listed as pending (`session_hook_pending`) is
//! skipped and counted; records start to count as hooks land.
//!
//! Extra files can be replayed with `GOVEC_SESSIONVARS_FILES=a.jsonl.gz,b.jsonl.gz`.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::io::Read;
use std::sync::atomic::Ordering::SeqCst;

use serde_json::Value;
use serde_json::json;
use tidb_base::Error;
use tidb_sessionctx::variable::session::GlobalVarAccessor;
use tidb_sessionctx::variable::session::SessionVars;
use tidb_sessionctx::variable::session_fields::SCALAR_FIELDS;
use tidb_sessionctx::variable::session_fields::SessionVarsFields;
use tidb_sessionctx::variable::tidb_vars;
use tidb_sessionctx::variable::variable::ScopeGlobal;
use tidb_sessionctx::variable::*;

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

/// `variable.NewMockGlobalAccessor4Tests()`: every variable at its default.
struct MockGlobalAccessor {
    vals: HashMap<String, String>,
}

impl GlobalVarAccessor for MockGlobalAccessor {
    fn get_global_sys_var(&self, name: &str) -> Result<String, Error> {
        if let Some(v) = self.vals.get(name) {
            return Ok(v.clone());
        }
        // Go's map is keyed by the (lower-case) names: an exact lookup.
        match get_sys_var(name).filter(|sv| sv.name == name) {
            Some(sv) => Ok(sv.value.clone()),
            None => {
                Err(error::ErrUnknownSystemVar.gen_with_stack_by_args(vec![name.to_owned().into()]))
            }
        }
    }
    fn set_global_sys_var_only(
        &mut self,
        name: &str,
        value: &str,
        _update_local: bool,
    ) -> Result<(), Error> {
        if get_sys_var(name).is_none() {
            return Err(
                error::ErrUnknownSystemVar.gen_with_stack_by_args(vec![name.to_owned().into()])
            );
        }
        self.vals.insert(name.to_owned(), value.to_owned());
        Ok(())
    }
    fn get_tidb_table_value(&self, _name: &str) -> Result<String, Error> {
        panic!("not supported")
    }
}

/// The variable-package atomics written by SetGlobal hooks (D05.2 PR B), restored to their
/// values before the first record (the generator's processGlobalsRestore).
fn restore_process_globals() {
    type Restore = Box<dyn Fn() + Send + Sync>;
    static SNAPSHOT: std::sync::LazyLock<Vec<Restore>> = std::sync::LazyLock::new(|| {
        let mut v: Vec<Restore> = Vec::new();
        macro_rules! keep {
            ($($a:ident),* $(,)?) => {$(
                let x = tidb_vars::$a.load(SeqCst);
                v.push(Box::new(move || tidb_vars::$a.store(x, SeqCst)));
            )*};
        }
        keep!(
            AccelerateUserCreationUpdate,
            ConnectAttrsSize,
            EnableBatchDML,
            EnableCheckConstraint,
            EnableFastReorg,
            EnableForeignKey,
            EnableHistoricalStatsForCapture,
            EnableInstancePlanCache,
            EnableNoopVariables,
            EnableRCReadCheckTS,
            EnableRowLevelChecksum,
            EnableTTLJob,
            EnableTmpStorageOnOOM,
            ExpensiveQueryTimeThreshold,
            ExpensiveTxnTimeThreshold,
            GCMaxWaitTime,
            IgnoreInlistPlanDigest,
            MaxUserConnectionsValue,
            MemQuotaBindingCache,
            MemoryUsageAlarmKeepRecordNum,
            PasswordHistory,
            PasswordReuseInterval,
            ProcessGeneralLog,
            QueryLogMaxLen,
            SchemaVersionCacheLimit,
            StatsLoadPseudoTimeout,
            StatsLoadSyncWait,
            TiFlashEnablePipelineMode,
            VarTiDBSuperReadOnly
        );
        let x = tidb_vars::DDLSlowOprThreshold.load(SeqCst);
        v.push(Box::new(move || {
            tidb_vars::DDLSlowOprThreshold.store(x, SeqCst)
        }));
        let x = tidb_vars::MemoryUsageAlarmRatio.load();
        v.push(Box::new(move || tidb_vars::MemoryUsageAlarmRatio.store(x)));
        let x = tidb_vars::OOMAction.load();
        v.push(Box::new(move || tidb_vars::OOMAction.store(x.clone())));
        keep!(
            HistoricalStatsDuration,
            AdvancerCheckPointLagLimit,
            EnablePProfSQLCPU,
            ForcePriority
        );
        let x = tidb_memory::tracker::EnableGCAwareMemoryTrack.load(SeqCst);
        v.push(Box::new(move || {
            tidb_memory::tracker::EnableGCAwareMemoryTrack.store(x, SeqCst)
        }));
        let x = tidb_memory::tracker::ServerMemoryLimitSessMinSize.load(SeqCst);
        v.push(Box::new(move || {
            tidb_memory::tracker::ServerMemoryLimitSessMinSize.store(x, SeqCst)
        }));
        let x = tidb_memory::tracker::ServerMemoryLimit.load(SeqCst);
        v.push(Box::new(move || {
            tidb_memory::tracker::ServerMemoryLimit.store(x, SeqCst)
        }));
        let x = tidb_memory::tracker::ServerMemoryLimitOriginText.load_full();
        v.push(Box::new(move || {
            tidb_memory::tracker::ServerMemoryLimitOriginText.store(x.clone())
        }));
        // The global config: INSTANCE hooks write it.
        let c = tidb_base::config::get_global_config_arc();
        v.push(Box::new(move || {
            tidb_base::config::store_global_config(c.clone())
        }));
        // D05.2 B3: state written by the B2 SetGlobal hooks in set_global_hook records.
        keep!(
            MaxPreparedStmtCountValue,
            ddlFlashbackConcurrency,
            ddlReorgWorkerCounter,
            ddlReorgBatchSize,
            DDLReorgMaxWriteSpeed,
            ddlErrorCountLimit,
            maxDeltaSchemaCount,
            ddlReorgRowFormat,
            TableCacheLease,
            AutoAnalyzePartitionBatchSize,
            PersistAnalyzeOptions,
            InstancePlanCacheMaxMemSize,
            MaxAutoAnalyzeTime,
            AutoAnalyzeConcurrency,
            EnableDistTask,
            EnableFastCreateTable,
            TxnEntrySizeLimit,
            DDLDiskQuota,
            TTLScanBatchSize,
            TTLDeleteBatchSize,
            TTLDeleteRateLimit,
            TTLScanWorkerCount,
            TTLDeleteWorkerCount,
            TTLRunningTasks
        );
        let x = tidb_vars::AnalyzeColumnOptions.load();
        v.push(Box::new(move || {
            tidb_vars::AnalyzeColumnOptions.store(x.clone())
        }));
        let x = tidb_vars::PreparedPlanCacheMemoryGuardRatio.load();
        v.push(Box::new(move || {
            tidb_vars::PreparedPlanCacheMemoryGuardRatio.store(x)
        }));
        let x = tidb_vars::InstancePlanCacheReservedPercentage.load();
        v.push(Box::new(move || {
            tidb_vars::InstancePlanCacheReservedPercentage.store(x)
        }));
        {
            use tidb_sessionctx::variable::hook_state as hs;
            macro_rules! keep_hs {
                ($($a:ident),* $(,)?) => {$(
                    let x = hs::$a.load(SeqCst);
                    v.push(Box::new(move || hs::$a.store(x, SeqCst)));
                )*};
            }
            keep_hs!(
                StoreLimit,
                TxnCommitBatchSize,
                CommitterConcurrency,
                RequireSecureTransport,
                TopSQLMaxStatementCount,
                TopSQLMaxCollect,
                GenerateBinaryPlan
            );
        }
        v.push(Box::new(|| {
            tidb_sessionctx::variable::ldap_state::LDAPSASLAuthImpl.reset();
            tidb_sessionctx::variable::ldap_state::LDAPSimpleAuthImpl.reset();
        }));
        v
    });
    for r in SNAPSHOT.iter() {
        r();
    }
}

/// Puts back the process-wide state the hooks write (the list is in the generator's
/// resetSysvarAtomics), so that no record depends on an earlier one.
fn reset_atomics() {
    // tidb_redact_log's SetSession hook stores the process-wide flag (Go
    // errors.RedactLogEnabled; upstream bug tidb-rs #62); its value after init is "".
    tidb_base::errors::redact_log_enabled_store("");
    tidb_vars::PasswordValidationLength.store(8, SeqCst);
    tidb_vars::PasswordValidationMixedCaseCount.store(1, SeqCst);
    tidb_vars::PasswordValidtaionNumberCount.store(1, SeqCst);
    tidb_vars::PasswordValidationSpecialCharCount.store(1, SeqCst);
    tidb_vars::GOGCTunerThreshold.store(tidb_vars::DefTiDBGOGCTunerThreshold);
    tidb_vars::RunAutoAnalyze.store(tidb_vars::DefTiDBEnableAutoAnalyze, SeqCst);
    tidb_vars::EnableAutoAnalyzePriorityQueue
        .store(tidb_vars::DefTiDBEnableAutoAnalyzePriorityQueue, SeqCst);
    // D05.2 B2: tidb_cloud_storage_uri / tidb_slow_log_rules SetGlobal hooks.
    tidb_vars::CloudStorageURI.store(String::new());
    *tidb_sessionctx::variable::slow_log_rules::GlobalSlowLogRules.write() = Default::default();
    restore_process_globals();
}

/// The callbacks other components register into the variable package, as the generator
/// has them: the executor's `GlobalAnalyzeMemoryTracker` limit (`-1` at start) behind
/// Get/SetMemQuotaAnalyze, and the generator's stub external timestamp (a process-wide
/// value, 0 at start). The others stay unregistered, as in the generator.
fn register_callbacks() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, AtomicU64};
    use tidb_sessionctx::variable::hook_state as hs;
    static ANALYZE_LIMIT: AtomicI64 = AtomicI64::new(-1);
    static EXTERNAL_TS: AtomicU64 = AtomicU64::new(0);
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        *hs::GetMemQuotaAnalyze.write() = Some(Arc::new(|| ANALYZE_LIMIT.load(SeqCst)));
        *hs::SetMemQuotaAnalyze.write() = Some(Arc::new(|v| ANALYZE_LIMIT.store(v, SeqCst)));
        *hs::GetExternalTimestamp.write() = Some(Arc::new(|| Ok(EXTERNAL_TS.load(SeqCst))));
        *hs::SetExternalTimestamp.write() = Some(Arc::new(|ts| {
            EXTERNAL_TS.store(ts, SeqCst);
            Ok(())
        }));
        // The generator's ValidateCloudStorageURI stub: a plain error for err://, a
        // *terror.Error for terr:// (the hook converts it to a plain error).
        *hs::ValidateCloudStorageURI.write() = Some(Arc::new(|uri: &str| {
            if uri.starts_with("err://") {
                return Err(tidb_base::errors::new(format!(
                    "unsupported storage: {uri}"
                )));
            }
            if uri.starts_with("terr://") {
                return Err(error::ErrWrongValueForVar.gen_with_stack_by_args(vec![
                    "tidb_cloud_storage_uri".into(),
                    uri.to_owned().into(),
                ]));
            }
            Ok(())
        }));
    });
}

fn fresh_vars() -> SessionVars {
    register_callbacks();
    reset_atomics();
    let mut vars = SessionVars::new(Box::new(MockGlobalAccessor {
        vals: HashMap::new(),
    }));
    vars.time_zone = Some(tidb_base::gostd::time::utc());
    vars
}

fn msg(e: &Error) -> Value {
    Value::String(String::from_utf8_lossy(&e.error()).into_owned())
}

fn err_or_null(r: &Result<(), Error>) -> Value {
    match r {
        Ok(()) => Value::Null,
        Err(e) => msg(e),
    }
}

fn fields(vars: &SessionVars) -> BTreeMap<String, String> {
    vars.scalar_strings()
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect()
}

fn systems(vars: &SessionVars) -> BTreeMap<String, String> {
    vars.systems()
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// The generator's nonScalarDump: non-scalar SessionVars fields SetSession hooks write
/// (D05.2 T6, B3).
fn non_scalar(vars: &SessionVars) -> BTreeMap<String, String> {
    let keys = |m: &std::collections::BTreeSet<tidb_base::kv::StoreType>| {
        let mut v: Vec<i64> = m.iter().map(|k| i64::from(k.0)).collect();
        v.sort_unstable();
        format!(
            "[{}]",
            v.iter()
                .map(|x| x.to_string())
                .collect::<Vec<_>>()
                .join(" ")
        )
    };
    let mut fix: Vec<String> = vars
        .state
        .optimizer_fix_control
        .iter()
        .map(|(k, v)| format!("{k}:{v}"))
        .collect();
    fix.sort();
    let skip: Vec<String> = vars
        .state
        .analyze_skip_column_types
        .iter()
        .cloned()
        .collect();
    let tz = vars
        .time_zone
        .as_ref()
        .map_or("<nil>".to_owned(), |l| l.string().to_owned());
    let mut m = BTreeMap::new();
    m.insert(
        "kv_vars.backoff_lock_fast".to_owned(),
        vars.state.kv_vars.backoff_lock_fast.to_string(),
    );
    m.insert(
        "kv_vars.back_off_weight".to_owned(),
        vars.state.kv_vars.back_off_weight.to_string(),
    );
    m.insert(
        "isolation_read_engines".to_owned(),
        keys(&vars.state.isolation_read_engines),
    );
    m.insert(
        "allow_fallback_to_tikv".to_owned(),
        keys(&vars.state.allow_fallback_to_tikv),
    );
    m.insert(
        "partition_prune_mode".to_owned(),
        vars.state.partition_prune_mode.clone(),
    );
    m.insert(
        "txn_scope".to_owned(),
        format!(
            "{}/{}",
            vars.state.txn_scope.get_var_value(),
            vars.state.txn_scope.get_txn_scope()
        ),
    );
    m.insert(
        "stats_load_sync_wait".to_owned(),
        vars.state.stats_load_sync_wait.to_string(),
    );
    m.insert("optimizer_fix_control".to_owned(), fix.join(","));
    m.insert("analyze_skip_column_types".to_owned(), skip.join(","));
    m.insert("time_zone".to_owned(), tz);
    m
}

fn diff(before: &BTreeMap<String, String>, after: &BTreeMap<String, String>) -> Value {
    let mut out = serde_json::Map::new();
    for (k, v) in after {
        if before.get(k) != Some(v) {
            out.insert(k.clone(), json!(v));
        }
    }
    for k in before.keys() {
        if !after.contains_key(k) {
            out.insert(k.clone(), json!("<deleted>"));
        }
    }
    Value::Object(out)
}

/// The variables whose read-back depends on the wall clock or the session's random
/// generator, or on the host (build target, config paths): the generator records their
/// read-back values as "<volatile>".
fn volatile(name: &str) -> bool {
    matches!(
        name,
        "timestamp"
            | "rand_seed1"
            | "rand_seed2"
            | "version_compile_os"
            | "version_compile_machine"
            | "tidb_config"
            // GetGlobal converts int(rate.Inf): platform-dependent (M-F2I, tidb-rs #27).
            | "tidb_slow_log_max_per_sec"
    )
}

/// Whether reading `name` back goes through a GetGlobal hook (Go's entry has one and the
/// variable has no session scope).
fn global_hook_read(name: &str) -> bool {
    get_sys_var(name)
        .is_some_and(|sv| sv.go_hooks & HOOK_GET_GLOBAL != 0 && !sv.has_session_scope())
}

/// The names a SetSession reaches: the variable and its aliases.
fn with_aliases(name: &str) -> Vec<String> {
    match get_sys_var(name) {
        Some(sv) => std::iter::once(sv.name.to_owned())
            .chain(sv.aliases.iter().map(|a| (*a).to_owned()))
            .collect(),
        None => vec![],
    }
}

/// Whether the record exercises a pending hook.
fn pending(op: &str, args: &[Value]) -> bool {
    let p = |n: &str, k: &str| session_hook_pending(n, k);
    match op {
        "set" => {
            let name = args[1].as_str().unwrap();
            // The read-back goes through GetSession or, for a variable without session
            // scope, GetGlobal (recorded as "<global-hook>", so a pending GetGlobal does
            // not matter).
            with_aliases(name).iter().any(|n| p(n, "SetSession"))
                || p(&gostd_lower(name), "GetSession")
                || (global_hook_read(name) && p(&gostd_lower(name), "GetGlobal"))
        }
        "get" => {
            let name = args[0].as_str().unwrap();
            ["GetSession", "GetStateValue", "GetGlobal"]
                .iter()
                .any(|k| p(&gostd_lower(name), k))
        }
        "set_global_hook" => {
            let name = gostd_lower(args[0].as_str().unwrap());
            p(&name, "SetGlobal") || p(&name, "GetGlobal")
        }
        "set_global" => p(&gostd_lower(args[0].as_str().unwrap()), "SetGlobal"),
        "set_global_hook2" => {
            let name = gostd_lower(args[0].as_str().unwrap());
            p(&name, "SetGlobal") || p(&name, "GetGlobal")
        }
        "init" => init_pairs(args).iter().any(|pair| {
            with_aliases(pair[0].as_str().unwrap())
                .iter()
                .any(|n| p(n, "SetSession"))
        }),
        _ => false,
    }
}

/// The (name, value) list of an `init` record. An empty list is `[]`, or `null` as Go
/// writes a nil slice (`"args":[null]`, before the generator skipped empty groups).
fn init_pairs(args: &[Value]) -> &[Value] {
    args.first()
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn gostd_lower(s: &str) -> String {
    tidb_base::gostd::to_lower(s)
}

/// Returns (out, err, warnings) like the Go exec.
fn exec(op: &str, args: &[Value]) -> (Value, Option<Error>, Vec<Error>) {
    match op {
        "fresh" => {
            let m: serde_json::Map<String, Value> = SessionVarsFields::default()
                .scalar_strings()
                .into_iter()
                .map(|(k, v)| (k.to_owned(), json!(v)))
                .collect();
            (Value::Object(m), None, vec![])
        }
        "set" => {
            let mut vars = fresh_vars();
            vars.stmt_ctx.stmt_type = "Set".to_owned();
            let (kind, name, val) = (
                args[0].as_str().unwrap(),
                args[1].as_str().unwrap(),
                args[2].as_str().unwrap(),
            );
            let (f0, s0, n0) = (fields(&vars), systems(&vars), non_scalar(&vars));
            let mut res = serde_json::Map::new();
            let r = match kind {
                "set" => vars.set_system_var(name, val),
                "relaxed" => vars.set_system_var_with_relaxed_validation(name, val),
                "noval" => vars.set_system_var_without_validation(name, val),
                _ => {
                    let (old, err) = vars.set_system_var_with_old_val_as_ret(name, val);
                    let old = if volatile(name) {
                        "<volatile>".to_owned()
                    } else {
                        old
                    };
                    res.insert("old".into(), json!(old));
                    err
                }
            };
            res.insert("fields".into(), diff(&f0, &fields(&vars)));
            res.insert("sys".into(), diff(&s0, &systems(&vars)));
            res.insert("ns".into(), diff(&n0, &non_scalar(&vars)));
            if r.is_ok() {
                match vars.get_session_or_global_system_var(name) {
                    Ok(v) => {
                        let v = if volatile(name) {
                            "<volatile>".to_owned()
                        } else {
                            v
                        };
                        res.insert("get".into(), json!(v));
                        res.insert("get_err".into(), Value::Null);
                    }
                    Err(e) => {
                        res.insert("get".into(), json!(""));
                        res.insert("get_err".into(), msg(&e));
                    }
                }
            }
            let warns = vars
                .stmt_ctx
                .get_warnings()
                .into_iter()
                .map(|w| w.err)
                .collect();
            (Value::Object(res), r.err(), warns)
        }
        "get" => {
            let mut vars = fresh_vars();
            let name = args[0].as_str().unwrap();
            let split = |r: Result<String, Error>| match r {
                Ok(v) => (json!(v), Value::Null),
                Err(e) => (json!(""), msg(&e)),
            };
            let (sess, sess_err) = split(vars.get_session_or_global_system_var(name));
            let (glob, glob_err) = split(vars.get_global_system_var(name));
            let (state, keep, state_err) = match vars.get_session_states_system_var(name) {
                Ok((v, k)) => (json!(v), json!(k), Value::Null),
                Err(e) => (json!(""), json!(false), msg(&e)),
            };
            let (sess, glob, state) = if volatile(name) {
                (
                    json!("<volatile>"),
                    json!("<volatile>"),
                    json!("<volatile>"),
                )
            } else {
                (sess, glob, state)
            };
            let out = json!({"sess": sess, "sess_err": sess_err, "glob": glob, "glob_err": glob_err,
                "state": state, "keep": keep, "state_err": state_err});
            let warns = vars
                .stmt_ctx
                .get_warnings()
                .into_iter()
                .map(|w| w.err)
                .collect();
            (out, None, warns)
        }
        "set_global" => {
            let mut vars = fresh_vars();
            vars.stmt_ctx.stmt_type = "Set".to_owned();
            let (name, val) = (args[0].as_str().unwrap(), args[1].as_str().unwrap());
            let (f0, s0) = (fields(&vars), systems(&vars));
            let mut res = serde_json::Map::new();
            // INSTANCE scope: the previous process-wide value, put back afterwards (as the
            // generator does) so that later records start from the same state.
            let sv = tidb_sessionctx::variable::variable::get_sys_var(name);
            let prev = sv
                .as_ref()
                .filter(|sv| sv.has_instance_scope())
                .and_then(|sv| sv.get_global_from_hook(&mut vars).ok());
            let r = vars.set_global_sys_var(name, val);
            if r.is_ok()
                && let Some(sv) = sv.as_ref()
                && sv.has_instance_scope()
                && sv.get_global.is_some()
                && !volatile(name)
            {
                let after = sv.get_global_from_hook(&mut vars);
                res.insert("after".into(), json!(after.clone().unwrap_or_default()));
                res.insert("after_err".into(), err_or_null(&after.map(|_| ())));
            }
            if let (Some(sv), Some(prev)) = (sv.as_ref(), prev) {
                let _ = sv.set_global_from_hook(&mut fresh_vars(), &prev, false);
            }
            let err = match r {
                // The read-only phase's "not supported" (owner issue #58), not another 1235.
                Err(e) if String::from_utf8_lossy(&e.error()).contains("SET GLOBAL on tidb-rs") => {
                    res.insert("persist".into(), json!(true));
                    None
                }
                Err(e) => Some(e),
                Ok(()) => None,
            };
            res.insert("fields".into(), diff(&f0, &fields(&vars)));
            res.insert("sys".into(), diff(&s0, &systems(&vars)));
            let warns = vars
                .stmt_ctx
                .get_warnings()
                .into_iter()
                .map(|w| w.err)
                .collect();
            (Value::Object(res), err, warns)
        }
        "set_global_hook2" => {
            let mut vars = fresh_vars();
            vars.stmt_ctx.stmt_type = "Set".to_owned();
            let name = args[0].as_str().unwrap();
            let mut steps = Vec::new();
            for val in args[1].as_array().unwrap() {
                let val = val.as_str().unwrap();
                let sv = get_sys_var(name).unwrap();
                let mut res = serde_json::Map::new();
                let (v, r) = sv.validate(&mut vars, val, ScopeGlobal);
                let r = r.and_then(|()| sv.set_global_from_hook(&mut vars, &v, false));
                if r.is_ok() && sv.get_global.is_some() {
                    let after = sv.get_global_from_hook(&mut vars);
                    res.insert("after".into(), json!(after.clone().unwrap_or_default()));
                    res.insert("after_err".into(), err_or_null(&after.map(|_| ())));
                }
                res.insert("err".into(), err_or_null(&r));
                steps.push(Value::Object(res));
            }
            let warns = vars
                .stmt_ctx
                .get_warnings()
                .into_iter()
                .map(|w| w.err)
                .collect();
            (Value::Array(steps), None, warns)
        }
        "set_global_hook" => {
            let mut vars = fresh_vars();
            vars.stmt_ctx.stmt_type = "Set".to_owned();
            let (name, val) = (args[0].as_str().unwrap(), args[1].as_str().unwrap());
            let sv = get_sys_var(name).unwrap();
            let mut res = serde_json::Map::new();
            let (v, r) = sv.validate(&mut vars, val, ScopeGlobal);
            let r = r.and_then(|()| sv.set_global_from_hook(&mut vars, &v, false));
            if r.is_ok() && sv.get_global.is_some() {
                let after = sv.get_global_from_hook(&mut vars);
                res.insert("after".into(), json!(after.clone().unwrap_or_default()));
                res.insert("after_err".into(), err_or_null(&after.map(|_| ())));
            }
            let warns = vars
                .stmt_ctx
                .get_warnings()
                .into_iter()
                .map(|w| w.err)
                .collect();
            // The hook changed process-wide state: put it back (the generator defers the reset).
            reset_atomics();
            (Value::Object(res), r.err(), warns)
        }
        "init" => {
            let mut vars = fresh_vars();
            vars.stmt_ctx.stmt_type = "Set".to_owned();
            let errs: Vec<Value> = init_pairs(args)
                .iter()
                .map(|p| {
                    err_or_null(
                        &vars.set_system_var(p[0].as_str().unwrap(), p[1].as_str().unwrap()),
                    )
                })
                .collect();
            let out = json!({"errs": if errs.is_empty() { Value::Null } else { json!(errs) },
                "fields": fields(&vars), "sys": systems(&vars)});
            let warns = vars
                .stmt_ctx
                .get_warnings()
                .into_iter()
                .map(|w| w.err)
                .collect();
            (out, None, warns)
        }
        _ => panic!("unknown op {op}"),
    }
}

/// The generator's canonical form of a tidb_slow_log_rules value: conditions of every rule
/// sorted, then the rules (Go's RawRules follows map order; slow_log_rules.rs M-MAPORDER).
fn canon_slow_log_rules(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    let mut rules: Vec<String> = s
        .split(';')
        .map(|r| {
            let mut conds: Vec<&str> = r.split(',').collect();
            conds.sort_unstable();
            conds.join(",")
        })
        .collect();
    rules.sort_unstable();
    rules.join(";")
}

/// Applies [`canon_slow_log_rules`] to the read-backs of a tidb_slow_log_rules record, as the
/// generator does.
fn canon_slow_log_rules_out(op: &str, args: &[Value], out: &mut Value) {
    let name = match op {
        "set" => args[1].as_str(),
        "get" | "set_global" | "set_global_hook" => args[0].as_str(),
        _ => None,
    };
    if name != Some("tidb_slow_log_rules") {
        return;
    }
    for k in ["get", "sess", "glob", "state", "old", "after"] {
        if let Some(Value::String(v)) = out.get_mut(k) {
            *v = canon_slow_log_rules(v);
        }
    }
}

fn check(rec: &Value) -> Option<String> {
    let op = rec["op"].as_str().unwrap();
    let args = rec["args"].as_array().map(|a| a.as_slice()).unwrap_or(&[]);
    let (mut out, err, warns) = exec(op, args);
    canon_slow_log_rules_out(op, args, &mut out);
    if op == "fresh" && SessionVarsFields::default().scalar_strings().len() != SCALAR_FIELDS.len() {
        return Some("duplicate field names".to_owned());
    }
    if out != rec["out"] {
        return Some(format!("out: got {out}, want {}", rec["out"]));
    }
    let got_err = err.as_ref().map(msg);
    let want_err = &rec["err"];
    let want = if want_err.is_null() {
        None
    } else {
        Some(want_err["msg"].clone())
    };
    if got_err != want {
        return Some(format!("err: got {got_err:?}, want {want:?}"));
    }
    let got_w: Vec<Value> = warns.iter().map(msg).collect();
    let want_w: Vec<Value> = rec["warnings"]
        .as_array()
        .map(|a| a.iter().map(|w| w["msg"].clone()).collect())
        .unwrap_or_default();
    if got_w != want_w {
        return Some(format!("warnings: got {got_w:?}, want {want_w:?}"));
    }
    None
}

/// Replays one file: (records checked, records skipped as pending).
fn replay(path: &str) -> (usize, usize) {
    let mut gz = flate2::read::GzDecoder::new(std::fs::File::open(path).expect("vectors"));
    let mut text = String::new();
    gz.read_to_string(&mut text).unwrap();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "sessionvars");
    // memory.MemTotal = 64 GiB (the generator's value; tidb_server_memory_limit percentages).
    tidb_memory::meminfo::set_mem_hooks(|| Ok(64 << 30), || Ok(0));
    let mut fails = vec![];
    let (mut n, mut skipped) = (0, 0);
    for line in lines {
        let rec: Value = serde_json::from_str(line).unwrap();
        let op = rec["op"].as_str().unwrap();
        let args = rec["args"].as_array().map(|a| a.as_slice()).unwrap_or(&[]);
        // Go panicked inside a GetGlobal hook (the test accessor panics on
        // GetTiDBTableValue): counted with the pending hooks while GetGlobal is pending.
        let go_panicked_in_get_global = rec["err"]["name"] == "panic"
            && args
                .iter()
                .filter_map(|a| a.as_str())
                .any(|n| session_hook_pending(&gostd_lower(n), "GetGlobal"));
        // A SET GLOBAL record that reads the stored value back through a pending GetGlobal.
        let after_through_pending_get_global = op == "set_global"
            && rec["out"].get("after").is_some()
            && session_hook_pending(&gostd_lower(args[0].as_str().unwrap()), "GetGlobal");
        if pending(op, args) || go_panicked_in_get_global || after_through_pending_get_global {
            skipped += 1;
            continue;
        }
        n += 1;
        if let Some(f) = check(&rec) {
            fails.push(format!("{op} {}: {f}", rec["args"]));
        }
    }
    assert!(
        fails.is_empty(),
        "{} mismatches of {n}:\n{}",
        fails.len(),
        fails[..fails.len().min(60)].join("\n")
    );
    (n, skipped)
}

/// The cap on the committed vectors' skipped records (see `govec_sessionvars`): after PR B2
/// only the 332 records of the five tidb_gc_* variables (232 before the special float inputs of PR C) are skipped (their SetGlobal /
/// GetGlobal hooks read and write mysql.tidb, which comes with PR C / T10). PR C lowers it to 0.
const MAX_SKIPPED: usize = 332;

#[test]
fn govec_sessionvars() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/vectors/v8.5.8");
    let path = format!("{dir}/{GO_TARGET}/sessionvars.jsonl.gz");
    if !std::path::Path::new(&path).exists() {
        panic!("no vectors for {GO_TARGET}: {path}");
    }
    let (n, skipped) = replay(&path);
    eprintln!("sessionvars: {n} records checked, {skipped} skipped (pending hooks)");
    assert!(n >= 1);
    // Ratchet on the records skipped because they exercise a pending hook: lowered by
    // each PR of the D05.2 series as hooks land, 0 after PR B (Lucky's review of #57), so
    // that a pending entry cannot stay forever.
    assert!(
        skipped <= MAX_SKIPPED,
        "{skipped} records skipped as pending, more than the cap {MAX_SKIPPED}"
    );
    if let Ok(files) = std::env::var("GOVEC_SESSIONVARS_FILES") {
        for f in files.split(',').filter(|f| !f.is_empty()) {
            let (n, skipped) = replay(f);
            eprintln!("{f}: {n} checked, {skipped} skipped");
        }
    }
}
