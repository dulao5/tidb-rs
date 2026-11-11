//! Replay of the D05.2 PR F ops of the govec domain "sessionvars" (`redact`, `lazytext`,
//! `lru`, `stmtcache`, `parsecfg`, `slowwarn`, `matchers`, `sessstates`; tracker
//! govec/domains/sessionvars_f.go). Included by `govec_sessionvars.rs`.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;
use serde_json::json;
use tidb_base::Error;
use tidb_base::errors;
use tidb_base::gofmt::Arg;
use tidb_base::redact;
use tidb_sessionctx::kvcache::{Key, new_simple_lru_cache};
use tidb_sessionctx::sessionstates::{HypoIndexes, SessionStates};
use tidb_sessionctx::variable::lazy_text::{LazyStmtText, ParseParam};
use tidb_sessionctx::variable::plan_cache_params::new_plan_cache_param_list;
use tidb_sessionctx::variable::slow_log::{self as sl, MatchValue};
use tidb_sessionctx::variable::slow_log_rules::Threshold;
use tidb_types::datum::Datum;

fn go_string_in(v: &Value) -> Vec<u8> {
    match v {
        Value::String(s) => s.as_bytes().to_vec(),
        Value::Object(m) => {
            let s = m["hex"].as_str().unwrap();
            (0..s.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
                .collect()
        }
        _ => panic!("bad GoString {v}"),
    }
}

/// `vec.GoString` encoding.
fn go_string(b: &[u8]) -> Value {
    match std::str::from_utf8(b) {
        Ok(s) => json!(s),
        Err(_) => json!({ "hex": b.iter().map(|x| format!("{x:02x}")).collect::<String>() }),
    }
}

fn err_str(r: Result<(), Error>) -> Value {
    match r {
        Ok(()) => Value::Null,
        Err(e) => json!(String::from_utf8_lossy(&e.error())),
    }
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap()
}

#[derive(Debug)]
struct K(String);

impl Key for K {
    fn hash(&self) -> Vec<u8> {
        self.0.as_bytes().to_vec()
    }
}

fn threshold(p: &Value) -> Threshold {
    let p = p.as_array().unwrap();
    match s(&p[0]) {
        "u64" => Threshold::U64(p[1].as_u64().unwrap()),
        "i64" => Threshold::I64(p[1].as_i64().unwrap()),
        "f64" => Threshold::F64(p[1].as_f64().unwrap()),
        "bool" => Threshold::Bool(p[1].as_bool().unwrap()),
        "str" => Threshold::Str(s(&p[1]).to_owned()),
        k => panic!("threshold kind {k}"),
    }
}

fn warn_err(kind: &str, msg: &str) -> Error {
    match kind {
        "terror" => tidb_base::dbterror::ClassTypes
            .new_std(tidb_base::mysql::ErrTruncatedWrongValue)
            .gen_with_stack_by_args(vec![
                Arg::go_string(b"DOUBLE".to_vec()),
                Arg::go_string(msg.as_bytes().to_vec()),
            ]),
        "terror2" => tidb_base::terror::ClassOptimizer
            .new_std(tidb_base::mysql::ErrUnknown)
            .gen_with_stack("%s", vec![Arg::go_string(msg.as_bytes().to_vec())]),
        // errors.Trace: no wrapper in Rust (M-ERR); Cause is the error itself in both.
        "wrapped" => tidb_base::dbterror::ClassTypes
            .new_std(tidb_base::mysql::ErrDataTooLong)
            .gen_with_stack_by_args(vec![Arg::go_string(msg.as_bytes().to_vec())]),
        _ => Error::plain(msg.to_owned()),
    }
}

fn i64_value(v: &Arc<dyn std::any::Any + Send + Sync>) -> Value {
    json!(*v.downcast_ref::<i64>().unwrap())
}

fn states_dump(st: &SessionStates) -> Value {
    let uv: BTreeMap<_, _> = st
        .user_vars
        .iter()
        .flatten()
        .map(|(k, v)| (k.clone(), json!(v.get_int64())))
        .collect();
    let ut: BTreeMap<_, _> = st
        .user_var_types
        .iter()
        .flatten()
        .map(|(k, v)| (k.clone(), json!(v.get_type())))
        .collect();
    let warns: Vec<Value> = st
        .warnings
        .iter()
        .map(|w| json!([w.level, String::from_utf8_lossy(&w.err.error())]))
        .collect();
    let seq: BTreeMap<String, i64> = st
        .sequence_latest_values
        .iter()
        .flatten()
        .map(|(k, v)| (k.to_string(), *v))
        .collect();
    json!({
        "uvNil": st.user_vars.is_none(), "uv": uv, "ut": ut, "psid": st.prepared_stmt_id,
        "status": st.status, "db": st.current_db, "txn": st.last_txn_info,
        "q": st.last_query_info.as_ref().map(|q| json!([q.start_ts, q.err_msg])),
        "d": st.last_ddl_info.as_ref().map(|d| json!([d.seq_num, d.query])),
        "found": st.last_found_rows, "fpc": st.found_in_plan_cache, "fb": st.found_in_binding,
        "seq": if st.sequence_latest_values.is_none() { Value::Null } else { json!(seq) },
        "rg": st.resource_group_name, "hypo": st.hypo_indexes.is_some(),
        "hypoTF": st.hypo_tiflash_replicas.is_some(), "ar": st.last_affected_rows,
        "lid": st.last_insert_id, "warns": if warns.is_empty() { Value::Null } else { json!(warns) },
    })
}

pub fn exec_f(op: &str, args: &[Value]) -> Option<(Value, Option<Error>)> {
    Some(match op {
        "redact" => {
            let input = go_string_in(&args[0]);
            let mut out = serde_json::Map::new();
            for mode in ["MARKER", "OFF", "ON", "bogus"] {
                let mut b = Vec::new();
                redact::write_redact(&mut b, &input, mode);
                errors::redact_log_enabled_store(if mode == "bogus" { "OFF" } else { mode });
                // Stringer: fmt.Sprint of the redacted Stringer is String(mode, s.String()).
                let stringer = match std::str::from_utf8(&input) {
                    Ok(text) => redact::stringer(mode, text).to_string().into_bytes(),
                    Err(_) => redact::string_bytes(mode, &input),
                };
                let string = match std::str::from_utf8(&input) {
                    Ok(text) => redact::string(mode, text).into_bytes(),
                    Err(_) => redact::string_bytes(mode, &input),
                };
                out.insert(
                    mode.to_owned(),
                    json!([
                        go_string(&string),
                        go_string(&stringer),
                        go_string(&b),
                        go_string(&redact::value(&input)),
                        redact::key(&input),
                        redact::need_redact()
                    ]),
                );
            }
            redact::init_redact(true);
            out.insert("init".to_owned(), json!(redact::need_redact()));
            redact::init_redact(false);
            for remove in [false, true] {
                let mut w = Vec::new();
                let r = redact::de_redact(remove, &input[..], &mut w, "|");
                out.insert(format!("de{remove}"), json!([go_string(&w), r.is_err()]));
            }
            errors::redact_log_enabled_store("OFF");
            (Value::Object(out), None)
        }
        "lazytext" => {
            let mut t = LazyStmtText::default();
            let params = args[2].as_array().map(|ps| {
                let mut p = new_plan_cache_param_list();
                let ds: Vec<Datum> = ps
                    .iter()
                    .map(|v| tidb_types::datum::new_int_datum(v.as_i64().unwrap()))
                    .collect();
                p.append(&ds);
                p.set_for_non_prep_cache(args[3].as_bool().unwrap());
                p
            });
            t.update(s(&args[0]), &go_string_in(&args[1]), params.as_ref());
            let first = LazyStmtText::string(Some(&mut t));
            if let Some(text) = args[4].as_str() {
                t.set_text(text.as_bytes());
            }
            let second = LazyStmtText::string(Some(&mut t));
            (
                json!([
                    go_string(&first),
                    go_string(&second),
                    go_string(&LazyStmtText::string(None))
                ]),
                None,
            )
        }
        "lru" => {
            let mut c = new_simple_lru_cache::<K, i64>(args[0].as_u64().unwrap() as usize, 0.0, 0);
            let evicted = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
            let ev = Arc::clone(&evicted);
            c.set_on_evict(Box::new(move |k: &K, v: &i64| {
                ev.lock().unwrap().push(json!([k.0, v]))
            }));
            let mut res = Vec::new();
            for o in args[1].as_array().unwrap() {
                let o = o.as_array().unwrap();
                res.push(match s(&o[0]) {
                    "put" => {
                        c.put(K(s(&o[1]).to_owned()), o[2].as_i64().unwrap());
                        Value::Null
                    }
                    "get" => match c.get(&K(s(&o[1]).to_owned())) {
                        Some(v) => json!([v, true]),
                        None => json!([null, false]),
                    },
                    "delete" => {
                        c.delete(&K(s(&o[1]).to_owned()));
                        Value::Null
                    }
                    "deleteAll" => {
                        c.delete_all();
                        Value::Null
                    }
                    "size" => json!(c.size()),
                    "keys" => {
                        let ks: Vec<&str> = c.keys().iter().map(|k| k.0.as_str()).collect();
                        if ks.is_empty() {
                            Value::Null
                        } else {
                            json!(ks)
                        }
                    }
                    "values" => json!(c.values()),
                    "setCapacity" => err_str(c.set_capacity(o[1].as_u64().unwrap() as usize)),
                    "removeOldest" => match c.remove_oldest() {
                        Some((k, v)) => json!([k.0, v, true]),
                        None => json!([null, null, false]),
                    },
                    other => panic!("lru op {other}"),
                });
            }
            let ev = std::mem::take(&mut *evicted.lock().unwrap());
            (
                json!({"res": res, "evicted": if ev.is_empty() { Value::Null } else { json!(ev) }}),
                None,
            )
        }
        "stmtcache" => {
            let mut vars = super::fresh_vars();
            vars.session_plan_cache_size = args[0].as_u64().unwrap();
            let mut res = Vec::new();
            for o in args[1].as_array().unwrap() {
                let o = o.as_array().unwrap();
                let k = s(&o[1]).as_bytes();
                res.push(match s(&o[0]) {
                    "add" => {
                        vars.add_non_prepared_plan_cache_stmt(k, Arc::new(o[2].as_i64().unwrap()));
                        Value::Null
                    }
                    "get" => vars
                        .get_non_prepared_plan_cache_stmt(k)
                        .map_or(Value::Null, |v| i64_value(&v)),
                    "dset" => {
                        vars.set_prepare_stmt_dedup_cache(k, Arc::new(o[2].as_i64().unwrap()));
                        Value::Null
                    }
                    "dget" => vars
                        .get_prepare_stmt_dedup_cache(k)
                        .map_or(Value::Null, |v| i64_value(&v)),
                    other => panic!("stmtcache op {other}"),
                });
            }
            (json!(res), None)
        }
        "parsecfg" => {
            let mut vars = super::fresh_vars();
            let sets: Vec<Value> = args[0]
                .as_array()
                .into_iter()
                .flatten()
                .map(|p| err_str(vars.set_system_var(s(&p[0]), s(&p[1]))))
                .collect();
            let cfg = vars.build_parser_config();
            let params: Vec<String> = vars
                .get_parse_params()
                .into_iter()
                .map(|p| match p {
                    ParseParam::CharsetConnection(v) => format!("parser.CharsetConnection:{v}"),
                    ParseParam::CollationConnection(v) => format!("parser.CollationConnection:{v}"),
                    ParseParam::CharsetClient(v) => format!("parser.CharsetClient:{v}"),
                })
                .collect();
            (
                json!({"set": if sets.is_empty() { Value::Null } else { json!(sets) },
                    "cfg": [cfg.enable_window_function, cfg.enable_strict_double_type_check, cfg.skip_position_recording],
                    "params": params}),
                None,
            )
        }
        "slowwarn" => {
            let sc = tidb_sessionctx::stmtctx::new_stmt_ctx();
            for w in args[0].as_array().into_iter().flatten() {
                let e = warn_err(s(&w[1]), s(&w[2]));
                match s(&w[0]) {
                    "warn" => sc.append_warning(e),
                    "note" => sc.append_note(e),
                    "error" => sc.append_error(e),
                    "extra" => sc.append_extra_warning(e),
                    "extranote" => sc.append_extra_note(e),
                    k => panic!("warn kind {k}"),
                }
            }
            let out: Vec<Value> = sl::collect_warnings_for_slow_log(&sc)
                .into_iter()
                .map(|w| json!([w.level, go_string(&w.message), w.is_extra]))
                .collect();
            (
                if out.is_empty() {
                    Value::Null
                } else {
                    json!(out)
                },
                None,
            )
        }
        "matchers" => {
            let t = threshold(&args[0]);
            let v = args[1].as_array().unwrap();
            let (eq, ge) = match s(&v[0]) {
                "int" => (
                    sl::match_equal(&t, &MatchValue::Int(v[1].as_i64().unwrap())),
                    Value::Null,
                ),
                "i64" => {
                    let mv = MatchValue::I64(v[1].as_i64().unwrap());
                    (sl::match_equal(&t, &mv), json!(sl::match_ge(&t, &mv)))
                }
                "u64" => {
                    let mv = MatchValue::U64(v[1].as_u64().unwrap());
                    (sl::match_equal(&t, &mv), json!(sl::match_ge(&t, &mv)))
                }
                "f64" => {
                    let mv = MatchValue::F64(v[1].as_f64().unwrap());
                    (sl::match_equal(&t, &mv), json!(sl::match_ge(&t, &mv)))
                }
                "bool" => (
                    sl::match_equal(&t, &MatchValue::Bool(v[1].as_bool().unwrap())),
                    Value::Null,
                ),
                "str" => (sl::match_equal(&t, &MatchValue::Str(s(&v[1]))), Value::Null),
                k => panic!("value kind {k}"),
            };
            let (u, ok) = match sl::uint64_from_non_negative(args[2].as_i64().unwrap()) {
                Some(u) => (u, true),
                None => (0, false),
            };
            let ps = match sl::parse_string(s(&args[3])).unwrap() {
                Threshold::Str(p) => p,
                _ => unreachable!(),
            };
            (
                json!([eq, ge, sl::match_zero(&t), [u, ok], [ps, null]]),
                None,
            )
        }
        "sessstates" => {
            let mut vars = super::fresh_vars();
            let m = args[0].as_object().unwrap();
            if let Some(uv) = m.get("uv").and_then(Value::as_object) {
                for (k, v) in uv {
                    vars.set_user_var_val(k, tidb_types::datum::new_int_datum(v.as_i64().unwrap()));
                }
            }
            if let Some(ut) = m.get("ut").and_then(Value::as_array) {
                for k in ut {
                    vars.state.user_vars.set_user_var_type(
                        s(k),
                        tidb_types::field_type::new_field_type(tidb_base::mysql::TypeLonglong),
                    );
                }
            }
            let status = m.get("status").and_then(Value::as_u64).unwrap_or(0) as u16;
            vars.set_status_flag(status, true);
            if let Some(v) = m.get("db") {
                vars.current_db = s(v).to_owned();
            }
            if let Some(v) = m.get("txn") {
                vars.last_txn_info = s(v).to_owned();
            }
            if let Some(v) = m.get("qstart") {
                vars.last_query_info.start_ts = v.as_u64().unwrap();
            }
            if let Some(v) = m.get("qerr") {
                vars.last_query_info.err_msg = s(v).to_owned();
            }
            if let Some(v) = m.get("ddlseq") {
                vars.last_ddl_info.seq_num = v.as_u64().unwrap();
            }
            if let Some(v) = m.get("ddlq") {
                vars.last_ddl_info.query = s(v).to_owned();
            }
            vars.last_found_rows = m.get("found").and_then(Value::as_u64).unwrap_or(0);
            vars.prev_found_in_plan_cache = m.get("pfc").and_then(Value::as_bool).unwrap_or(false);
            vars.prev_found_in_binding = m.get("pfb").and_then(Value::as_bool).unwrap_or(false);
            if let Some(v) = m.get("rg") {
                vars.resource_group_name = s(v).to_owned();
            }
            vars.set_next_prepared_stmt_id(
                m.get("psid").and_then(Value::as_u64).unwrap_or(0) as u32
            );
            if let Some(seq) = m.get("seq").and_then(Value::as_object) {
                let mm: HashMap<i64, i64> = seq
                    .iter()
                    .map(|(k, v)| (k.parse().unwrap(), v.as_i64().unwrap()))
                    .collect();
                vars.sequence_state.set_all_states(&mm);
            }
            if m.get("hypo").and_then(Value::as_bool).unwrap_or(false) {
                let mut h: HypoIndexes = HashMap::new();
                h.entry("db".into())
                    .or_default()
                    .entry("t".into())
                    .or_default()
                    .insert("i".into(), Default::default());
                vars.hypo_indexes = Some(h);
                vars.hypo_tiflash_replicas = Some(HashMap::from([(
                    "db".to_owned(),
                    ["t".to_owned()].into_iter().collect(),
                )]));
            }
            let ar = m.get("ar").and_then(Value::as_u64).unwrap_or(0);
            vars.stmt_ctx().set_affected_rows(ar);
            vars.stmt_ctx_mut().prev_affected_rows = ar as i64;
            vars.stmt_ctx_mut().prev_last_insert_id =
                m.get("lid").and_then(Value::as_u64).unwrap_or(0);
            if let Some(ws) = m.get("warns").and_then(Value::as_array) {
                for w in ws {
                    vars.stmt_ctx()
                        .append_warning(Error::plain(s(w).to_owned()));
                }
            }
            let mut st = SessionStates::default();
            let enc = vars.encode_session_states(&mut st);
            let mut other = super::fresh_vars();
            let dec = other.decode_session_states(&st);
            let mut dec_st = SessionStates::default();
            other.encode_session_states(&mut dec_st).unwrap();
            let mut d = states_dump(&dec_st);
            let dm = d.as_object_mut().unwrap();
            dm.insert("vfpc".into(), json!(other.found_in_plan_cache));
            dm.insert("vfb".into(), json!(other.found_in_binding));
            dm.insert("affected".into(), json!(other.stmt_ctx().affected_rows()));
            (
                json!({"enc": states_dump(&st), "dec": d, "err": [err_str(enc), err_str(dec)]}),
                None,
            )
        }
        _ => return None,
    })
}
