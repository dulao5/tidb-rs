//! Replays the Go-generated vectors `tests/vectors/v8.5.8/<target>/sysvar.jsonl.gz` (tracker
//! govec domain "sysvar", block D05.1).
//!
//! Every record's `out` must be equal as JSON; errors and warnings must match by message;
//! `err.name == "panic"` records must panic in Rust too. Each validate / relaxed record runs on
//! a fresh session (the D05.2 stub) with the test accessor holding every default, the
//! process-wide state reset, TimeZone UTC and MemTotal 64 GiB — as the Go generator does.
//!
//! Extra files can be replayed with `GOVEC_SYSVAR_FILES=a.jsonl.gz,b.jsonl.gz` (other seeds).

use std::any::Any;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::io::Read;
use std::panic::AssertUnwindSafe;
use std::panic::catch_unwind;
use std::sync::atomic::Ordering::SeqCst;

use serde_json::Value;
use serde_json::json;
use tidb_base::Error;
use tidb_sessionctx::variable::removed;
use tidb_sessionctx::variable::sequence_state::SequenceState;
use tidb_sessionctx::variable::session::GlobalVarAccessor;
use tidb_sessionctx::variable::session::SessionVars;
use tidb_sessionctx::variable::statusvar;
use tidb_sessionctx::variable::tidb_vars;
use tidb_sessionctx::variable::varsutil;
use tidb_sessionctx::variable::*;

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

/// `variable.NewMockGlobalAccessor4Tests()`: every variable at its default (the registry's
/// value unless overridden; Go copies all defaults into its map).
///
/// Plus the generator's in-memory mysql.tidb (`tidbTableAccessor`: name -> (value, comment));
/// a missing row is session.getTableValue's "Result is empty" error.
struct MockGlobalAccessor {
    vals: HashMap<String, String>,
    tidb: TidbTable,
}

/// mysql.tidb of [`MockGlobalAccessor`], shared with the replay that dumps it.
type TidbTable = std::sync::Arc<std::sync::Mutex<BTreeMap<String, (String, String)>>>;

impl MockGlobalAccessor {
    fn new() -> MockGlobalAccessor {
        MockGlobalAccessor {
            vals: HashMap::new(),
            tidb: TidbTable::default(),
        }
    }
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
    fn get_tidb_table_value(&self, name: &str) -> Result<String, Error> {
        match self.tidb.lock().unwrap().get(name) {
            Some((v, _)) => Ok(v.clone()),
            None => Err(tidb_base::dbterror::exeerrors::ErrResultIsEmpty.clone()),
        }
    }
    fn set_tidb_table_value(
        &mut self,
        name: &str,
        value: &str,
        comment: &str,
    ) -> Result<(), Error> {
        self.tidb
            .lock()
            .unwrap()
            .insert(name.to_owned(), (value.to_owned(), comment.to_owned()));
        Ok(())
    }
}

fn reset_atomics() {
    tidb_vars::PasswordValidationLength.store(8, SeqCst);
    tidb_vars::PasswordValidationMixedCaseCount.store(1, SeqCst);
    tidb_vars::PasswordValidtaionNumberCount.store(1, SeqCst);
    tidb_vars::PasswordValidationSpecialCharCount.store(1, SeqCst);
    tidb_vars::GOGCTunerThreshold.store(tidb_vars::DefTiDBGOGCTunerThreshold);
    tidb_vars::RunAutoAnalyze.store(tidb_vars::DefTiDBEnableAutoAnalyze, SeqCst);
    tidb_vars::EnableAutoAnalyzePriorityQueue
        .store(tidb_vars::DefTiDBEnableAutoAnalyzePriorityQueue, SeqCst);
}

fn fresh_vars(c: &Value) -> SessionVars {
    let mut acc = MockGlobalAccessor::new();
    if let Some(g) = c["g"].as_object() {
        for (k, v) in g {
            acc.set_global_sys_var_only(k, v.as_str().unwrap(), false)
                .unwrap();
        }
    }
    let mut vars = SessionVars::new(Box::new(acc));
    vars.time_zone = Some(tidb_base::gostd::time::utc());
    vars.stmt_ctx_mut().stmt_type = c["stmt"].as_str().unwrap_or("").to_owned();
    if let Some(n) = c["noop"].as_i64() {
        vars.noop_funcs_mode = n;
    }
    if let Some(s) = c["s"].as_object() {
        for (k, v) in s {
            vars.set_system_var_for_test(k, v.as_str().unwrap());
        }
    }
    if c["tls"].as_bool() == Some(true) {
        vars.tls_connection_state = Some(());
    }
    if c["nompp"].as_bool() == Some(true) {
        vars.set_allow_mpp_execution(false);
    }
    vars
}

fn msg(e: &Error) -> Value {
    Value::String(String::from_utf8_lossy(&e.error()).into_owned())
}

fn scope_of(s: &str) -> ScopeFlag {
    match s {
        "session" => ScopeSession,
        "global" => ScopeGlobal,
        "instance" => ScopeInstance,
        _ => ScopeNone,
    }
}

fn registry_out(sv: &SysVar) -> Value {
    let val = match sv.name {
        "version_compile_os" => {
            assert_eq!(sv.value, go_goos());
            "<GOOS>".to_owned()
        }
        "version_compile_machine" => {
            assert_eq!(sv.value, go_goarch());
            "<GOARCH>".to_owned()
        }
        _ => sv.value.clone(),
    };
    json!({
        "scope": sv.scope.0, "value": val, "type": sv.typ.0, "min": sv.min_value, "max": sv.max_value.to_string(),
        "acnb": sv.auto_convert_negative_bool, "ro": sv.read_only, "pv": sv.possible_values, "ae": sv.allow_empty,
        "aea": sv.allow_empty_all, "aav": sv.allow_auto_value, "hint": sv.is_hint_updatable_verified, "hidden": sv.hidden,
        "aliases": sv.aliases, "dep": sv.depended, "noop": sv.is_noop, "gcn": sv.global_config_name,
        "skip_init": sv.skip_init(), "skip_cache": sv.skip_sysvar_cache(), "none": sv.has_none_scope(),
        "sess": sv.has_session_scope(), "glob": sv.has_global_scope(), "inst": sv.has_instance_scope(),
        "h_val": sv.validation.is_some(), "h_ss": sv.go_hooks & HOOK_SET_SESSION != 0,
        "h_sg": sv.go_hooks & HOOK_SET_GLOBAL != 0, "h_gs": sv.go_hooks & HOOK_GET_SESSION != 0,
        "h_gg": sv.go_hooks & HOOK_GET_GLOBAL != 0, "h_gsv": sv.go_hooks & HOOK_GET_STATE_VALUE != 0,
        "h_rdp": sv.go_hooks & HOOK_REQUIRE_DYNAMIC_PRIVILEGES != 0,
    })
}

/// Go's `strconv.FormatFloat(f, 'g', -1, 64)`.
fn fmt_g(f: f64) -> String {
    tidb_base::gostd::strconv::format_float(f, b'g', -1, 64)
}

/// (out, err, warnings)
fn exec(op: &str, args: &[Value]) -> (Value, Option<Error>, Vec<Error>) {
    let s = |i: usize| args[i].as_str().unwrap();
    match op {
        "registry" => (registry_out(&get_sys_var(s(0)).unwrap()), None, vec![]),
        "validate" | "relaxed" => {
            reset_atomics();
            let sv = get_sys_var(s(0)).unwrap();
            let mut vars = fresh_vars(&args[3]);
            let scope = scope_of(s(2));
            let (v, err) = if op == "validate" {
                sv.validate(&mut vars, s(1), scope)
            } else {
                (
                    sv.validate_with_relaxed_validation(&mut vars, s(1), scope),
                    Ok(()),
                )
            };
            let mut res = json!({ "v": v });
            if s(0).starts_with("validate_password.") {
                res["pwlen"] = json!(tidb_vars::PasswordValidationLength.load(SeqCst));
                res["pwlen_g"] = json!(
                    vars.global_vars_accessor
                        .get_global_sys_var(names::ValidatePasswordLength)
                        .unwrap_or_default()
                );
            }
            if s(0) == names::ForeignKeyChecks {
                res["fkc"] = json!(vars.foreign_key_checks);
            }
            let warns = vars
                .stmt_ctx()
                .get_warnings()
                .into_iter()
                .map(|w| w.err)
                .collect();
            (res, err.err(), warns)
        }
        "set_sys_var" => {
            let old = get_sys_var(s(0)).unwrap().value.clone();
            set_sys_var(s(0), s(1));
            let got = get_sys_var(s(0)).unwrap().value.clone();
            set_sys_var(s(0), &old);
            (json!(got), None, vec![])
        }
        "native_type" => {
            let sv = get_sys_var(s(0)).unwrap();
            let (d, tp, flag) = sv.get_native_val_type(s(1));
            let dv = match d.kind() {
                tidb_types::datum::KindUint64 => json!(d.get_uint64().to_string()),
                tidb_types::datum::KindInt64 => json!(d.get_int64()),
                _ => json!(String::from_utf8_lossy(d.get_string())),
            };
            (
                json!({"kind": d.kind(), "d": dv, "tp": tp, "flag": flag}),
                None,
                vec![],
            )
        }
        "scope_string" => (
            json!(ScopeFlag(args[0].as_u64().unwrap() as u8).to_string()),
            None,
            vec![],
        ),
        "opt" => {
            let v = s(1);
            let def = args[2].as_i64().unwrap();
            let out = match s(0) {
                "on" => json!(varsutil::tidb_opt_on(v)),
                "on_off_warn" => json!(varsutil::tidb_opt_on_off_warn(v)),
                "clustered" => json!(varsutil::tidb_opt_enable_clustered(v).0),
                "assertion" => json!(varsutil::tidb_opt_assertion_level(v).0),
                "positive_int32" => json!(varsutil::tidb_opt_positive_int32(v, def as isize)),
                "int" => json!(varsutil::tidb_opt_int(v, def as isize)),
                "int64" => json!(varsutil::tidb_opt_int64(v, def)),
                "uint64" => json!(varsutil::tidb_opt_uint64(v, def as u64).to_string()),
                "float64" => json!(fmt_g(varsutil::tidb_opt_float64(v, def as f64))),
                "tf_onoff" => json!(varsutil::true_false_to_on_off(v)),
                "onoff_tf" => json!(varsutil::on_off_to_true_false(v)),
                "bool" => json!(varsutil::bool_to_on_off(varsutil::tidb_opt_on(v))),
                "int32_bool" => {
                    let (n, e) = tidb_types::gostd::parse_int(v.as_bytes(), 32);
                    if e.is_err() {
                        Value::Null
                    } else {
                        json!(varsutil::int32_to_bool_str(n as i32))
                    }
                }
                f => panic!("unknown opt {f}"),
            };
            (out, None, vec![])
        }
        "byte_size" => {
            let (b, bs) = varsutil::parse_byte_size(s(0));
            let (p, ps) = varsutil::parse_percentage(s(0));
            (
                json!({"b": b.to_string(), "bs": bs, "p": p.to_string(), "ps": ps}),
                None,
                vec![],
            )
        }
        "skip_types" => {
            let (v, err) = varsutil::valid_analyze_skip_column_types(s(0));
            let parsed: Vec<String> = varsutil::parse_analyze_skip_column_types(s(0))
                .into_iter()
                .collect();
            (json!({"v": v, "parsed": parsed}), err.err(), vec![])
        }
        "removed" => (
            json!(removed::is_removed_sys_var(s(0))),
            removed::check_sys_var_is_removed(s(0)).err(),
            vec![],
        ),
        "sequence" => {
            let ss = SequenceState::new();
            let sorted = |m: HashMap<i64, i64>| {
                let mut v: Vec<(i64, i64)> = m.into_iter().collect();
                v.sort();
                json!(
                    v.into_iter()
                        .map(|(k, v)| json!([k, v]))
                        .collect::<Vec<_>>()
                )
            };
            let mut outs = vec![];
            for st in args[0].as_array().map(|a| a.to_vec()).unwrap_or_default() {
                let st: Vec<i64> = st
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|x| x.as_i64().unwrap())
                    .collect();
                match st[0] {
                    0 => {
                        ss.update_state(st[1], st[2]);
                        outs.push(Value::Null);
                    }
                    1 => {
                        let (v, is_null, e) = ss.get_last_value(st[1]);
                        outs.push(json!([v, is_null, e.is_ok()]));
                    }
                    2 => outs.push(sorted(ss.get_all_states())),
                    3 => {
                        ss.set_all_states(&HashMap::from([(st[1], st[2]), (st[1] + 1, st[2] * 2)]));
                        outs.push(Value::Null);
                    }
                    _ => unreachable!(),
                }
            }
            let steps = if outs.is_empty() {
                Value::Null
            } else {
                json!(outs)
            };
            (
                json!({"steps": steps, "final": sorted(ss.get_all_states())}),
                None,
                vec![],
            )
        }
        "order_dep" => {
            let names: Vec<String> = args[0]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| x.as_str().unwrap().to_owned())
                .collect();
            // Go's map dedups the names.
            let mut uniq: Vec<&str> = vec![];
            for n in &names {
                if !uniq.contains(&n.as_str()) {
                    uniq.push(n);
                }
            }
            let got = order_by_dependency(uniq.iter().copied());
            let ndep = got
                .iter()
                .take_while(|n| get_sys_var(n).is_some_and(|sv| sv.depended))
                .count();
            let mut dep: Vec<&str> = got[..ndep].to_vec();
            let mut rest: Vec<&str> = got[ndep..].to_vec();
            assert!(
                rest.iter()
                    .all(|n| !get_sys_var(n).is_some_and(|sv| sv.depended))
            );
            dep.sort_unstable();
            rest.sort_unstable();
            let dep = if dep.is_empty() {
                Value::Null
            } else {
                json!(dep)
            };
            (json!({"dep": dep, "rest": rest}), None, vec![])
        }
        "ddl_atomic" => {
            let v = args[1].as_i64().unwrap();
            macro_rules! sg {
                ($get:ident, $set:ident, $t:ty) => {{
                    let before = varsutil::$get() as i64;
                    varsutil::$set(v as $t);
                    let after = varsutil::$get() as i64;
                    varsutil::$set(before as $t);
                    json!([before, after])
                }};
            }
            let out = match s(0) {
                "reorg_worker" => sg!(
                    get_ddl_reorg_worker_counter,
                    set_ddl_reorg_worker_counter,
                    i32
                ),
                "flashback" => sg!(
                    get_ddl_flashback_concurrency,
                    set_ddl_flashback_concurrency,
                    i32
                ),
                "reorg_batch" => sg!(get_ddl_reorg_batch_size, set_ddl_reorg_batch_size, i32),
                "err_limit" => sg!(get_ddl_error_count_limit, set_ddl_error_count_limit, i64),
                "row_format" => sg!(get_ddl_reorg_row_format, set_ddl_reorg_row_format, i64),
                "delta_schema" => sg!(get_max_delta_schema_count, set_max_delta_schema_count, i64),
                w => panic!("{w}"),
            };
            (out, None, vec![])
        }
        "defaults" => {
            let (m, d) = tidb_vars::defaults_for_test(s(0));
            (json!({"mem_limit_default": m, "dur": d}), None, vec![])
        }
        "status_vars" => {
            let mut vars = SessionVars::new(Box::new(MockGlobalAccessor::new()));
            if args[0].as_bool() == Some(true) {
                vars.tls_connection_state = Some(());
            }
            let m = statusvar::get_status_vars(Some(&vars)).unwrap();
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let rows: Vec<Value> = keys
                .iter()
                .map(|k| json!([k, m[*k].scope.0, m[*k].value.to_string()]))
                .collect();
            (json!(rows), None, vec![])
        }
        _ => panic!("unknown op {op}"),
    }
}

fn panic_msg(p: Box<dyn Any + Send>) -> String {
    if let Some(s) = p.downcast_ref::<String>() {
        return s.clone();
    }
    if let Some(s) = p.downcast_ref::<&str>() {
        return (*s).to_owned();
    }
    "?".to_owned()
}

fn check(rec: &Value) -> Option<String> {
    let op = rec["op"].as_str().unwrap();
    let args = rec["args"].as_array().unwrap();
    let want_err = &rec["err"];
    let got = catch_unwind(AssertUnwindSafe(|| exec(op, args)));
    if want_err["name"] == "panic" {
        return match got {
            Err(_) => None,
            Ok((out, err, _)) => Some(format!("Go panics, Rust returned {out} / {err:?}")),
        };
    }
    let (out, err, warns) = match got {
        Ok(v) => v,
        Err(p) => return Some(format!("Rust panicked: {}", panic_msg(p))),
    };
    if out != rec["out"] {
        return Some(format!("out: got {out}, want {}", rec["out"]));
    }
    let got_err = err.as_ref().map(msg);
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

fn replay(path: &str) {
    let mut gz = flate2::read::GzDecoder::new(std::fs::File::open(path).expect("vectors"));
    let mut text = String::new();
    gz.read_to_string(&mut text).unwrap();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "sysvar");
    // memory.MemTotal = 64 GiB (the generator's value; tidb_server_memory_limit percentages).
    tidb_memory::meminfo::set_mem_hooks(|| Ok(64 << 30), || Ok(0));
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let mut fails = vec![];
    let mut n = 0;
    for line in lines {
        n += 1;
        let rec: Value = serde_json::from_str(line).unwrap();
        if let Some(why) = check(&rec) {
            let mut l = line.to_owned();
            l.truncate(400);
            let mut why = why;
            why.truncate(1200);
            fails.push(format!("{l}\n    {why}"));
        }
    }
    std::panic::set_hook(prev);
    assert!(
        fails.is_empty(),
        "{path}: {} of {n} records differ:\n{}",
        fails.len(),
        fails
            .iter()
            .take(40)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert_eq!(n as u64, header["records"].as_u64().unwrap());
}

#[test]
fn govec_sysvar() {
    replay(&format!(
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/sysvar.jsonl.gz",
        env!("CARGO_MANIFEST_DIR")
    ));
    if let Ok(extra) = std::env::var("GOVEC_SYSVAR_FILES") {
        for f in extra.split(',').filter(|f| !f.is_empty()) {
            replay(f);
        }
    }
}
