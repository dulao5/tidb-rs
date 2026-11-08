//! Replays the govec `stmtctx` vectors (tidb-rs-tracker, block D00.1 part 1): errctx,
//! pkg/util/context and the core of stmtctx. See govec/domains/stmtctx.go for the record
//! format; this file mirrors its step interpreter.
//!
//! Extra files can be replayed with `GOVEC_STMTCTX_FILES=a.jsonl.gz,b.jsonl.gz`.

use std::any::Any;
use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tidb_base::Error;
use tidb_base::contextutil::{
    self, IgnoreWarn, PlanCacheState, PlanCacheTracker, PlanCacheType, RangeFallbackHandler,
    SQLWarn, StaticWarnHandler, WarnAppender, WarnHandler, WarnHandlerExt,
};
use tidb_base::errno;
use tidb_base::gostd::time::fixed_zone;
use tidb_base::terror::ErrClass;
use tidb_sessionctx::errctx::{self, ErrGroup, Level, LevelMap};
use tidb_sessionctx::stmtctx::{
    self, ReferenceCount, ReservedRowIDAlloc, StatementContext, StmtCacheKey,
};
use tidb_types::context::Flags;

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

const CODES: &[u16] = &[
    errno::ErrTruncatedWrongValue,
    errno::ErrDataTooLong,
    errno::ErrTruncatedWrongValueForField,
    errno::ErrWarnDataOutOfRange,
    errno::ErrDataOutOfRange,
    errno::ErrBadNumber,
    errno::ErrWrongValueForType,
    errno::ErrDatetimeFunctionOverflow,
    errno::WarnDataTruncated,
    errno::ErrIncorrectDatetimeValue,
    errno::ErrBadNull,
    errno::ErrWarnNullToNotnull,
    errno::ErrNoDefaultForField,
    errno::ErrDivisionByZero,
    errno::ErrAutoincReadFailed,
    errno::ErrNoPartitionForGivenValue,
    errno::ErrRowDoesNotMatchGivenPartitionSet,
    errno::ErrDupEntry,
    errno::ErrUnknown,
    errno::ErrParse,
    errno::ErrBadField,
    errno::ErrInternal,
];

/// The class of each code (govec stmtctxClasses).
const CLASSES: &[isize] = &[
    2, 20, 19, 2, 20, 20, 6, 20, 2, 20, 2, 5, 19, 6, 1, 19, 19, 8, 6, 11, 2, 10,
];

fn mk_err(e: i64) -> Error {
    match e {
        -1 => tidb_base::errors::new("plain error"),
        -2 => tidb_base::errors::new(""),
        e if e >= 0 && (e as usize) < CODES.len() => {
            let code = CODES[e as usize];
            ErrClass(CLASSES[e as usize]).new_std_err(code, errno::MySQLErrName.get(&code).unwrap())
        }
        _ => panic!("stmtctx: bad error {e}"),
    }
}

fn err_out(err: &Error) -> Value {
    let code = if err.is_normalized() { err.code() } else { 0 };
    json!({"code": code, "msg": String::from_utf8_lossy(&err.error())})
}

fn res_out(r: Result<(), Error>) -> Value {
    match r {
        Ok(()) => Value::Null,
        Err(e) => err_out(&e),
    }
}

fn warns(ws: &[SQLWarn]) -> Value {
    Value::Array(
        ws.iter()
            .map(|w| json!([w.level.as_ref(), err_out(&w.err)]))
            .collect(),
    )
}

fn levels(raw: &Value) -> LevelMap {
    let mut l = LevelMap::default();
    for (i, v) in raw.as_array().unwrap().iter().enumerate() {
        if i < l.len() {
            l[i] = Level(v.as_u64().unwrap() as u8);
        }
    }
    l
}

fn levels_out(l: LevelMap) -> Value {
    Value::Array(l.iter().map(|v| json!(v.0)).collect())
}

struct Env {
    h: Arc<StaticWarnHandler>,
    ec: errctx::Context,
    pct: Arc<PlanCacheTracker>,
    rfh: RangeFallbackHandler,
    sc: StatementContext,
    rc: ReferenceCount,
    rr: ReservedRowIDAlloc,
    func_log: Arc<Mutex<Vec<Value>>>,
}

fn new_env() -> Env {
    let h = Arc::new(contextutil::new_static_warn_handler(0));
    let ec = errctx::new_context(h.clone());
    let pct = Arc::new(contextutil::new_plan_cache_tracker(Some(h.clone())));
    let rfh = contextutil::new_range_fallback_handler(pct.clone(), h.clone());
    Env {
        h,
        ec,
        pct,
        rfh,
        sc: stmtctx::new_stmt_ctx(),
        rc: ReferenceCount::default(),
        rr: ReservedRowIDAlloc::default(),
        func_log: Arc::new(Mutex::new(vec![])),
    }
}

fn sc_dump(sc: &StatementContext) -> Value {
    let (ec, wc) = sc.num_error_warnings();
    json!({
        "flags": sc.type_flags().0, "levels": levels_out(sc.err_levels()), "pd": sc.push_down_flags(),
        "warns": warns(&sc.get_warnings()), "extra": warns(&sc.get_extra_warnings()),
        "wcount": sc.warning_count(), "nerr": [ec, wc],
        "rows": [sc.affected_rows(), sc.found_rows(), sc.record_rows(), sc.deleted_rows(),
            sc.updated_rows(), sc.copied_rows(), sc.touched_rows()],
        "msg": sc.get_message(), "tz": sc.time_zone().string(),
    })
}

fn any_i64(v: &Arc<dyn Any + Send + Sync>) -> Value {
    json!(*v.downcast_ref::<i64>().unwrap())
}

fn step(e: &mut Env, s: &[Value]) -> Value {
    let op = s[0].as_str().unwrap();
    let i = |k: usize| s[k].as_i64().unwrap();
    let st = |k: usize| s[k].as_str().unwrap().to_owned();
    match op {
        "h_warn" => e.h.append_warning(mk_err(i(1))),
        "h_note" => e.h.append_note(mk_err(i(1))),
        "h_error" => e.h.append_error(mk_err(i(1))),
        "h_warns" => {
            let ws: Vec<SQLWarn> = (0..i(1))
                .map(|_| SQLWarn {
                    level: contextutil::WarnLevelWarning.into(),
                    err: mk_err(i(2)),
                })
                .collect();
            e.h.append_warnings(&ws);
        }
        "h_bulk" => {
            for _ in 0..i(1) {
                e.h.append_warning(mk_err(i(2)));
            }
            return json!(e.h.warning_count());
        }
        "h_truncate" => return warns(&e.h.truncate_warnings(i(1))),
        "h_copy" => return warns(&e.h.copy_warnings(vec![])),
        "h_set" => {
            let ws: Vec<SQLWarn> = (0..i(1))
                .map(|_| SQLWarn {
                    level: contextutil::WarnLevelNote.into(),
                    err: mk_err(i(2)),
                })
                .collect();
            e.h.set_warnings(ws);
        }
        "h_count" => return json!(e.h.warning_count()),
        "h_nerr" => {
            let (a, b) = e.h.num_error_warnings();
            return json!([a, b]);
        }
        "h_reset" => e.h.reset(),
        "h_get" => return warns(&e.h.get_warnings()),
        "h_clone" => {
            let h2 = contextutil::new_static_warn_handler_with_handler(Some(&*e.h));
            h2.append_note(mk_err(-1));
            return json!([warns(&h2.get_warnings()), e.h.warning_count()]);
        }
        "h_clone_nil" => {
            return json!(contextutil::new_static_warn_handler_with_handler(None).warning_count());
        }
        "ignore" => {
            let w = &IgnoreWarn;
            w.append_warning(mk_err(i(1)));
            w.append_note(mk_err(i(1)));
            return json!([
                w.warning_count(),
                w.truncate_warnings(0).len(),
                w.copy_warnings(vec![]).len()
            ]);
        }
        "ec_new" => e.ec = errctx::new_context_with_levels(levels(&s[1]), e.h.clone()),
        "ec_func" => {
            let log = e.func_log.clone();
            let f = contextutil::new_func_warn_appender_for_test(move |level, err| {
                log.lock().unwrap().push(json!([level, err_out(&err)]));
            });
            e.ec = errctx::new_context_with_levels(levels(&s[1]), Arc::new(f));
        }
        "ec_flog" => return Value::Array(e.func_log.lock().unwrap().clone()),
        "ec_strict" => e.ec = e.ec.with_strict_err_group_level(),
        "ec_level" => e.ec = e.ec.with_err_group_level(ErrGroup(i(1)), Level(i(2) as u8)),
        "ec_levels" => e.ec = e.ec.with_err_group_levels(levels(&s[1])),
        "ec_map" => return levels_out(e.ec.level_map()),
        "ec_for" => return json!(e.ec.level_for_group(ErrGroup(i(1))).0),
        "ec_handle" => return res_out(e.ec.handle_error(mk_err(i(1)))),
        "ec_alias" => {
            return res_out(e.ec.handle_error_with_alias(mk_err(i(1)), mk_err(i(2)), mk_err(i(3))));
        }
        "ec_warn" => e.ec.append_warning(mk_err(i(1))),
        "ec_note" => e.ec.append_note(mk_err(i(1))),
        "ec_nowarn" => return res_out(errctx::StrictNoWarningContext.handle_error(mk_err(i(1)))),
        "pct_skip" => e.pct.set_skip_plan_cache(&st(1)),
        "pct_warn" => e.pct.warn_skip_plan_cache(&st(1)),
        "pct_type" => e.pct.set_cache_type(PlanCacheType(i(1))),
        "pct_force" => e.pct.set_force_plan_cache(i(1) != 0),
        "pct_always" => e.pct.set_always_warn_skip_cache(i(1) != 0),
        "pct_enable" => e.pct.enable_plan_cache(),
        "pct_state" => {
            let s = e.pct.save();
            return json!([
                s.use_cache,
                s.cache_type.0,
                s.plan_cache_unqualified,
                s.force_plan_cache,
                s.always_warn_skip_cache,
                e.pct.use_cache(),
                e.pct.plan_cache_unqualified()
            ]);
        }
        "pct_restore" => e.pct.restore(PlanCacheState {
            use_cache: i(1) != 0,
            cache_type: PlanCacheType(i(2)),
            plan_cache_unqualified: st(3),
            force_plan_cache: i(4) != 0,
            always_warn_skip_cache: i(5) != 0,
        }),
        "rfh" => e.rfh.record_range_fallback(i(1)),
        "sc_new" => e.sc = stmtctx::new_stmt_ctx(),
        "sc_new_tz" => {
            e.sc = stmtctx::new_stmt_ctx_with_time_zone(fixed_zone(&st(1), i(2) as i32));
        }
        "sc_reset" => return json!(e.sc.reset()),
        "sc_tz" => e.sc.set_time_zone(fixed_zone(&st(1), i(2) as i32)),
        "sc_flags" => e.sc.set_type_flags(Flags(i(1) as u16)),
        "sc_levels" => e.sc.set_err_levels(levels(&s[1])),
        "sc_pb" => {
            e.sc.init_from_pb_flag_and_tz(i(1) as u64, fixed_zone(&st(2), i(3) as i32))
        }
        "sc_kind" => {
            let m = i(1);
            let sc = &mut e.sc;
            sc.in_insert_stmt = m & 1 != 0;
            sc.in_update_stmt = m & 2 != 0;
            sc.in_delete_stmt = m & 4 != 0;
            sc.in_select_stmt = m & 8 != 0;
            sc.in_load_data_stmt = m & 16 != 0;
            sc.in_restricted_sql = m & 32 != 0;
            sc.in_show_warning = m & 64 != 0;
            sc.in_handle_foreign_key_trigger = m & 128 != 0;
            sc.use_dynamic_prune_mode = m & 256 != 0;
        }
        "sc_dump" => return sc_dump(&e.sc),
        "sc_errctx" => {
            let ec = e.sc.err_ctx();
            let tc = e.sc.type_ctx();
            return json!([
                levels_out(ec.level_map()),
                e.sc.err_group_level(ErrGroup(i(1))).0,
                tc.flags().0,
                stmtctx::type_ctx_or_default(Some(&e.sc)).flags().0,
                e.sc.use_dynamic_partition_prune()
            ]);
        }
        "sc_trunc" => return res_out(e.sc.handle_truncate(Err(mk_err(i(1))))),
        "sc_handle" => return res_out(e.sc.handle_error(mk_err(i(1)))),
        "sc_alias" => {
            return res_out(e.sc.handle_error_with_alias(mk_err(i(1)), mk_err(i(2)), mk_err(i(3))));
        }
        "sc_tc_warn" => e.sc.type_ctx().append_warning(mk_err(i(1))),
        "sc_warn" => e.sc.append_warning(mk_err(i(1))),
        "sc_note" => e.sc.append_note(mk_err(i(1))),
        "sc_error" => e.sc.append_error(mk_err(i(1))),
        "sc_warns" => e.sc.append_warnings(&[SQLWarn {
            level: contextutil::WarnLevelError.into(),
            err: mk_err(i(1)),
        }]),
        "sc_xwarn" => e.sc.append_extra_warning(mk_err(i(1))),
        "sc_xnote" => e.sc.append_extra_note(mk_err(i(1))),
        "sc_xerror" => e.sc.append_extra_error(mk_err(i(1))),
        "sc_xset" => e.sc.set_extra_warnings(vec![SQLWarn {
            level: contextutil::WarnLevelNote.into(),
            err: mk_err(i(1)),
        }]),
        "sc_set" => e.sc.set_warnings(vec![SQLWarn {
            level: contextutil::WarnLevelError.into(),
            err: mk_err(i(1)),
        }]),
        "sc_truncw" => return warns(&e.sc.truncate_warnings(i(1))),
        "sc_copyw" => return warns(&e.sc.copy_warnings(vec![])),
        "sc_rows" => {
            let n = i(2) as u64;
            match i(1) {
                0 => e.sc.add_affected_rows(n),
                1 => e.sc.set_affected_rows(n),
                2 => e.sc.add_found_rows(n),
                3 => e.sc.add_record_rows(n),
                4 => e.sc.add_deleted_rows(n),
                5 => e.sc.add_updated_rows(n),
                6 => e.sc.add_copied_rows(n),
                7 => e.sc.add_touched_rows(n),
                _ => {}
            }
        }
        "sc_msg" => e.sc.set_message(st(1)),
        "sc_hint" => e.sc.set_hint_warning(&st(1)),
        "sc_hint_err" => e.sc.set_hint_warning_from_error(mk_err(i(1))),
        "sc_varhint" => {
            e.sc.add_set_var_hint_restore(st(1), st(2));
            return json!(e.sc.set_var_hint_restore);
        }
        "sc_plan" => {
            e.sc.set_binary_plan(st(1));
            e.sc.set_encoded_plan(st(2));
            if i(3) != 0 {
                e.sc.set_plan_hint(st(1));
                e.sc.set_index_force();
            }
            if i(4) != 0 {
                e.sc.set_use_chunk_alloc();
            } else {
                e.sc.clear_use_chunk_alloc();
            }
            let (h, ok) = e.sc.get_plan_hint();
            return json!([
                e.sc.get_binary_plan(),
                e.sc.get_encoded_plan(),
                h,
                ok,
                e.sc.get_index_force(),
                e.sc.get_use_chunk_alloc_status()
            ]);
        }
        "sc_alt" => {
            match i(1) {
                0 => e.sc.reset_alternative_logical_plan_signals(),
                1 => e.sc.mark_alternative_logical_plan_decorrelated_apply(),
                2 => e.sc.mark_alternative_logical_plan_same_order_index_join(),
                3 => e.sc.mark_alternative_logical_plan_prefer_correlate(),
                _ => {}
            }
            return json!([
                e.sc.alternative_logical_plan_decorrelated_apply,
                e.sc.alternative_logical_plan_same_order_index_join,
                e.sc.alternative_logical_plan_prefer_correlate
            ]);
        }
        "sc_cache" => {
            let v =
                e.sc.get_or_store_stmt_cache(StmtCacheKey(i(1)), Arc::new(i(2)));
            return any_i64(&v);
        }
        "sc_cache_eval" => {
            let mut calls = 0;
            let (val, fail) = (i(2), i(3));
            let r = e.sc.get_or_evaluate_stmt_cache(StmtCacheKey(i(1)), || {
                calls += 1;
                if fail != 0 {
                    return Err(mk_err(fail - 1));
                }
                Ok(Arc::new(val) as Arc<dyn Any + Send + Sync>)
            });
            return match r {
                Ok(v) => json!([any_i64(&v), null, calls]),
                Err(err) => json!([null, err_out(&err), calls]),
            };
        }
        "sc_cache_del" => e.sc.reset_in_stmt_cache(StmtCacheKey(i(1))),
        "sc_cache_reset" => e.sc.reset_stmt_cache(),
        "sc_stale_set" => {
            let (v, fail) = (i(1) as u64, i(2));
            e.sc.set_stale_tso_provider_if_not_exist(Box::new(move || {
                if fail != 0 {
                    return Err(mk_err(fail - 1));
                }
                Ok(v)
            }));
        }
        "sc_stale_get" => {
            return match e.sc.get_stale_tso() {
                Ok(v) => json!([v, null]),
                Err(err) => json!([0, err_out(&err)]),
            };
        }
        "sc_retry" => e.sc.reset_for_retry(),
        "sc_pct" => {
            let pct = e.sc.plan_cache_tracker.clone();
            pct.enable_plan_cache();
            pct.set_cache_type(PlanCacheType(i(1)));
            pct.set_skip_plan_cache(&st(2));
            e.sc.range_fallback_handler.record_range_fallback(i(3));
            return json!([pct.use_cache(), pct.plan_cache_unqualified()]);
        }
        "rc" => {
            let r = match i(1) {
                0 => e.rc.try_increase(),
                1 => {
                    e.rc.decrease();
                    false
                }
                2 => e.rc.try_freeze(),
                _ => {
                    e.rc.un_freeze();
                    false
                }
            };
            return json!([r, e.rc.0.load(Ordering::SeqCst)]);
        }
        "rr_reset" => {
            e.rr.reset(i(1), i(2));
            return json!(e.rr.exhausted());
        }
        "rr_consume" => {
            let (id, ok) = e.rr.consume();
            return json!([id, ok, e.rr.exhausted()]);
        }
        _ => panic!("stmtctx: unknown step {op:?}"),
    }
    Value::Null
}

fn panic_msg(p: Box<dyn Any + Send>) -> String {
    if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else if let Some(s) = p.downcast_ref::<&str>() {
        (*s).to_owned()
    } else {
        "?".to_owned()
    }
}

fn script(steps: &[Value]) -> Value {
    let mut e = new_env();
    let mut out = vec![];
    for s in steps {
        match catch_unwind(AssertUnwindSafe(|| step(&mut e, s.as_array().unwrap()))) {
            Ok(v) => out.push(v),
            Err(p) => {
                out.push(json!({"panic": panic_msg(p)}));
                break;
            }
        }
    }
    Value::Array(out)
}

/// Runs one record: (out, err).
fn exec(op: &str, args: &[Value]) -> (Value, Value) {
    match op {
        "script" => (script(args[0].as_array().unwrap()), Value::Null),
        "sqlwarn_marshal" => {
            let w = SQLWarn {
                level: args[0].as_str().unwrap().to_owned().into(),
                err: mk_err(args[1].as_i64().unwrap()),
            };
            let b = w.marshal_json().unwrap();
            (json!(String::from_utf8(b).unwrap()), Value::Null)
        }
        "sqlwarn_unmarshal" => {
            let mut w = SQLWarn {
                level: "".into(),
                err: mk_err(-2),
            };
            match w.unmarshal_json(args[0].as_str().unwrap().as_bytes()) {
                Ok(()) => (json!([w.level.as_ref(), err_out(&w.err)]), Value::Null),
                Err(e) => (Value::Null, json!(String::from_utf8_lossy(&e.error()))),
            }
        }
        "push_down" => {
            let f = Flags(args[0].as_u64().unwrap() as u16);
            (
                json!(stmtctx::push_down_flags_with_type_flags_and_err_levels(
                    f,
                    levels(&args[1])
                )),
                Value::Null,
            )
        }
        "resolve" => (
            json!(
                errctx::resolve_err_level(args[0].as_bool().unwrap(), args[1].as_bool().unwrap()).0
            ),
            Value::Null,
        ),
        _ => panic!("stmtctx: unknown op {op}"),
    }
}

fn replay(path: &str) -> usize {
    let mut gz = flate2::read::GzDecoder::new(std::fs::File::open(path).expect("vectors"));
    let mut text = String::new();
    gz.read_to_string(&mut text).unwrap();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "stmtctx");
    let (mut n, mut fails) = (0, vec![]);
    for line in lines {
        let rec: Value = serde_json::from_str(line).unwrap();
        let op = rec["op"].as_str().unwrap();
        let args = rec["args"].as_array().unwrap();
        let (out, err) = exec(op, args);
        // Go's error is recorded as {"name","msg"}; only the message is compared.
        let want_err = if rec["err"].is_null() {
            Value::Null
        } else {
            rec["err"]["msg"].clone()
        };
        n += 1;
        if out != rec["out"] || err != want_err {
            fails.push(format!(
                "{op} {} ({}):\n  got  {out} / {err}\n  want {} / {want_err}",
                rec["args"], rec["src"], rec["out"]
            ));
        }
    }
    assert!(
        fails.is_empty(),
        "{} mismatches of {n}:\n{}",
        fails.len(),
        fails[..fails.len().min(20)].join("\n")
    );
    n
}

#[test]
fn govec_stmtctx() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/vectors/v8.5.8");
    let path = format!("{dir}/{GO_TARGET}/stmtctx.jsonl.gz");
    let n = replay(&path);
    eprintln!("stmtctx: {n} records");
    if let Ok(files) = std::env::var("GOVEC_STMTCTX_FILES") {
        for f in files.split(',').filter(|f| !f.is_empty()) {
            replay(f);
        }
    }
}
