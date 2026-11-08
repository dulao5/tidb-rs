//! Replays the govec `exprctx` vectors (tidb-rs-tracker `govec/domains/exprctx.go`): the
//! expression context interfaces, the optional property providers and the static contexts of
//! block D00.1 part 2a against Go TiDB v8.5.8.
//!
//! `GOVEC_EXPRCTX_FILES` (comma-separated paths) replays extra-seed artifacts as well.

use std::any::Any;
use std::collections::HashMap;
use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::{Value, json};
use tidb_base::Error;
use tidb_base::contextutil::{
    IgnoreWarnHandler, PlanCacheTracker, SQLWarn, WarnAppender, WarnHandler, ignore_warn_appender,
    new_plan_cache_tracker, new_static_warn_handler,
};
use tidb_base::gostd::time::{fixed_zone, unix};
use tidb_base::mysql::SQLMode;
use tidb_sessionctx::errctx;
use tidb_sessionctx::exprctx::{
    BuildContext, BuildContextRef, EmptyParamValues, EvalContext as _, ExprContext as _,
    ExprContextRef, OptPropCurrentUser, OptPropsCnt, OptionalEvalPropDesc, OptionalEvalPropKey,
    OptionalEvalPropKeySet, OptionalEvalPropProvider, OptionalEvalPropProviderRef,
    PlanColumnIDAllocator, StaticConvertibleEvalContext, StaticConvertibleExprContext,
    ctx_with_handle_truncate_err_level, new_simple_plan_column_id_allocator,
    optionalPropertyDescList, with_constant_propagate_check, with_null_reject_check,
};
use tidb_sessionctx::expropt::get_prop_provider;
use tidb_sessionctx::exprstatic::{
    self as es, EvalContext, EvalCtxOption, ExprContext, ExprCtxOption,
};
use tidb_sessionctx::variable::session_state::MysqlRng;
use tidb_types::context::Flags;
use tidb_types::datum::new_int_datum;

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

fn vectors() -> String {
    format!(
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/exprctx.jsonl.gz",
        env!("CARGO_MANIFEST_DIR")
    )
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// vec.GoString's JSON form.
fn gs(b: &[u8]) -> Value {
    match std::str::from_utf8(b) {
        Ok(s) => json!(s),
        Err(_) => json!({ "hex": hex(b) }),
    }
}

fn gstr(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        _ => panic!("not a string: {v}"),
    }
}

fn ec_try(f: impl FnOnce() -> Value) -> Value {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| json!({ "panic": true }))
}

fn ec_err(e: &Error) -> Value {
    json!({ "err": gs(&e.error()) })
}

/// A provider of key k (Go uses the expropt provider types; only Desc is read).
struct DummyProvider(usize);

impl OptionalEvalPropProvider for DummyProvider {
    fn desc(&self) -> &'static OptionalEvalPropDesc {
        &optionalPropertyDescList[self.0]
    }
    fn as_any_arc(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
}

fn opts_of(v: &Value) -> Vec<Vec<Value>> {
    match v {
        Value::Array(a) => a.iter().map(|o| o.as_array().unwrap().clone()).collect(),
        _ => Vec::new(),
    }
}

fn eval_opts(v: &Value) -> Vec<EvalCtxOption> {
    let mut out: Vec<EvalCtxOption> = Vec::new();
    for o in opts_of(v) {
        match o[0].as_str().unwrap() {
            "warn" => match o[1].as_str().unwrap() {
                "static" => out.push(es::with_warn_handler(Some(Arc::new(
                    new_static_warn_handler(0),
                )))),
                "ignore" => out.push(es::with_warn_handler(Some(Arc::new(IgnoreWarnHandler)))),
                _ => out.push(es::with_warn_handler(None)),
            },
            "sqlmode" => out.push(es::with_sql_mode(SQLMode(o[1].as_i64().unwrap()))),
            "flags" => out.push(es::with_type_flags(Flags(o[1].as_u64().unwrap() as u16))),
            "loc" => {
                if o[1].is_null() {
                    out.push(es::with_location(None))
                } else {
                    out.push(es::with_location(Some(fixed_zone(
                        "Z",
                        o[1].as_i64().unwrap() as i32,
                    ))))
                }
            }
            "levels" => {
                let mut lm = [errctx::LevelError; 7];
                for (i, l) in o[1].as_array().unwrap().iter().enumerate() {
                    lm[i] = errctx::Level(l.as_u64().unwrap() as u8);
                }
                out.push(es::with_err_level_map(lm))
            }
            "db" => out.push(es::with_current_db(gstr(&o[1]))),
            "time" => {
                let (sec, nsec) = (o[1].as_i64().unwrap(), o[2].as_i64().unwrap());
                out.push(es::with_current_time(Arc::new(move || Ok(unix(sec, nsec)))))
            }
            "time_err" => out.push(es::with_current_time(Arc::new(|| {
                Err(tidb_base::errors::new("time error"))
            }))),
            "packet" => out.push(es::with_max_allowed_packet(o[1].as_u64().unwrap())),
            "week" => out.push(es::with_default_week_format_mode(gstr(&o[1]))),
            "div" => out.push(es::with_div_precision_increment(o[1].as_i64().unwrap())),
            "params" => {
                let ps: Vec<_> = o[1]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| new_int_datum(v.as_i64().unwrap()))
                    .collect();
                out.push(es::with_param_list(&ps))
            }
            "redact" => out.push(es::with_enable_redact_log(gstr(&o[1]))),
            "props" => {
                let ps: Vec<OptionalEvalPropProviderRef> = o[1]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|k| Arc::new(DummyProvider(k.as_u64().unwrap() as usize)) as _)
                    .collect();
                out.push(es::with_optional_property(ps))
            }
            other => panic!("eval_opts: {other}"),
        }
    }
    out
}

fn expr_opts(v: &Value, eval: Option<Arc<EvalContext>>) -> Vec<ExprCtxOption> {
    let mut out: Vec<ExprCtxOption> = Vec::new();
    if let Some(e) = eval {
        out.push(es::with_eval_ctx(e));
    }
    for o in opts_of(v) {
        match o[0].as_str().unwrap() {
            "charset" => out.push(es::with_charset(gstr(&o[1]), gstr(&o[2]))),
            "defcoll" => out.push(es::with_default_collation_for_utf8mb4(gstr(&o[1]))),
            "block" => out.push(es::with_block_encryption_mode(gstr(&o[1]))),
            "sysdate" => out.push(es::with_sys_date_is_now(o[1].as_bool().unwrap())),
            "noop" => out.push(es::with_noop_funcs_mode(o[1].as_i64().unwrap())),
            "connid" => out.push(es::with_connection_id(o[1].as_u64().unwrap())),
            "window" => out.push(es::with_windowing_use_high_precision(
                o[1].as_bool().unwrap(),
            )),
            "gcml" => out.push(es::with_group_concat_max_len(o[1].as_u64().unwrap())),
            "alloc" => out.push(es::with_column_id_allocator(Arc::new(
                new_simple_plan_column_id_allocator(o[1].as_i64().unwrap()),
            ))),
            "rng" => out.push(es::with_rng(Arc::new(Mutex::new(MysqlRng::new_with_seed(
                o[1].as_i64().unwrap(),
            ))))),
            "tracker" => {
                let t: PlanCacheTracker = new_plan_cache_tracker(Some(ignore_warn_appender()));
                if o[1].as_bool().unwrap() {
                    t.enable_plan_cache();
                }
                out.push(es::with_plan_cache_tracker(Arc::new(t)))
            }
            other => panic!("expr_opts: {other}"),
        }
    }
    out
}

fn has_opt(v: &Value, names: &[&str]) -> bool {
    opts_of(v)
        .iter()
        .any(|o| names.contains(&o[0].as_str().unwrap()))
}

fn warns(ws: &[SQLWarn]) -> Value {
    Value::Array(
        ws.iter()
            .map(|w| json!([&*w.level, gs(&w.err.error())]))
            .collect(),
    )
}

fn time_dump(ctx: &dyn StaticConvertibleEvalContext, has_time: bool) -> Value {
    match ctx.current_time() {
        Err(e) => ec_err(&e),
        Ok(tm) if !has_time => json!({ "now": true, "loc": tm.location().string() }),
        Ok(tm) => {
            json!({ "unix": tm.unix(), "ns": tm.nanosecond(), "loc": tm.location().string() })
        }
    }
}

fn eval_dump(ctx: &dyn StaticConvertibleEvalContext, has_time: bool, warn: bool) -> Value {
    let (tc, ec) = (ctx.type_ctx(), ctx.err_ctx());
    let mut m = serde_json::Map::new();
    m.insert("sqlmode".into(), json!(ctx.sql_mode().0));
    m.insert("flags".into(), json!(tc.flags().0));
    m.insert("loc".into(), json!(ctx.location().string()));
    m.insert("tloc".into(), json!(tc.location().string()));
    m.insert(
        "levels".into(),
        json!(ec.level_map().iter().map(|l| l.0).collect::<Vec<_>>()),
    );
    m.insert("db".into(), gs(ctx.current_db().as_bytes()));
    m.insert("time".into(), time_dump(ctx, has_time));
    m.insert("packet".into(), json!(ctx.get_max_allowed_packet()));
    m.insert("redact".into(), json!(ctx.get_tidb_redact_log()));
    m.insert("week".into(), json!(ctx.get_default_week_format_mode()));
    m.insert("div".into(), json!(ctx.get_div_precision_increment()));
    let n = ctx.all_param_values().len() as i64;
    let params: Vec<Value> = (-1..=n)
        .map(|i| match ctx.get_param_value(i) {
            Ok(d) => json!(d.get_int64()),
            Err(e) => ec_err(&e),
        })
        .collect();
    m.insert("params".into(), Value::Array(params));
    m.insert("props".into(), json!(ctx.get_optional_prop_set().0));
    let prov: Vec<Value> = (-1..=OptPropsCnt as i64)
        .map(
            |k| match ctx.get_optional_prop_provider(OptionalEvalPropKey(k)) {
                Some(p) => json!(p.desc().key().0),
                None => Value::Null,
            },
        )
        .collect();
    m.insert("prov".into(), Value::Array(prov));
    m.insert(
        "uservar".into(),
        json!(ctx.get_user_vars_reader().get_user_var_val("a").is_some()),
    );
    if warn {
        tc.append_warning(tidb_base::errors::new("w-type"));
        ec.append_warning(tidb_base::errors::new("w-err"));
        ctx.append_warning(tidb_base::errors::new("w-ctx"));
        ctx.append_note(tidb_base::errors::new("n-ctx"));
        m.insert("wcount".into(), json!(ctx.warning_count()));
        m.insert("wcopy".into(), warns(&ctx.copy_warnings(Vec::new())));
        m.insert(
            "whcount".into(),
            json!(ctx.get_warn_handler().warning_count()),
        );
        m.insert("wtrunc".into(), warns(&ctx.truncate_warnings(1)));
        m.insert("wcount2".into(), json!(ctx.warning_count()));
    }
    Value::Object(m)
}

fn ptr<T: ?Sized>(a: &Arc<T>) -> *const () {
    Arc::as_ptr(a) as *const ()
}

fn expr_dump(ctx: &Arc<ExprContext>, warn: bool) -> Value {
    let mut m = serde_json::Map::new();
    let (cs, col) = ctx.get_charset_info();
    m.insert("cs".into(), json!(cs));
    m.insert("col".into(), json!(col));
    m.insert(
        "defcoll".into(),
        json!(ctx.get_default_collation_for_utf8mb4()),
    );
    m.insert("block".into(), json!(ctx.get_block_encryption_mode()));
    m.insert("sysdate".into(), json!(ctx.get_sysdate_is_now()));
    m.insert("noop".into(), json!(ctx.get_noop_funcs_mode()));
    m.insert("connid".into(), json!(ctx.connection_id()));
    m.insert(
        "window".into(),
        json!(ctx.get_windowing_use_high_precision()),
    );
    m.insert("gcml".into(), json!(ctx.get_group_concat_max_len()));
    m.insert("nr".into(), json!(ctx.is_in_null_reject_check()));
    m.insert("cp".into(), json!(ctx.is_constant_propagate_check()));
    m.insert("ro".into(), json!(ctx.is_readonly_user_var("a")));
    let st = ctx.get_static_eval_ctx();
    m.insert(
        "same_eval".into(),
        json!(
            ptr(&ctx.get_eval_ctx()) == ptr(&st)
                && ptr(&ctx.get_static_convertible_eval_context()) == ptr(&st)
        ),
    );
    m.insert(
        "same_tracker".into(),
        json!(Arc::ptr_eq(
            &ctx.get_plan_cache_tracker(),
            &ctx.get_plan_cache_tracker()
        )),
    );
    if warn {
        let mut ids = vec![ctx.get_last_plan_column_id()];
        for _ in 0..3 {
            ids.push(ctx.alloc_plan_column_id());
        }
        ids.push(ctx.get_last_plan_column_id());
        m.insert("ids".into(), json!(ids));
        m.insert("cache0".into(), json!(ctx.is_use_cache()));
        ctx.set_skip_plan_cache("some reason");
        m.insert("cache1".into(), json!(ctx.is_use_cache()));
        m.insert(
            "warns".into(),
            warns(&ctx.get_eval_ctx().copy_warnings(Vec::new())),
        );
    }
    Value::Object(m)
}

fn rng_dump(ctx: &Arc<ExprContext>, seeded: bool) -> Value {
    if !seeded {
        return Value::Null;
    }
    let r = ctx.rng();
    let mut g = r.lock();
    json!([g.r#gen(), g.r#gen()])
}

fn exec(op: &str, p: &[Value]) -> Value {
    match op {
        "keyset" => {
            let (set, key) = (
                OptionalEvalPropKeySet(p[0].as_u64().unwrap()),
                OptionalEvalPropKey(p[1].as_i64().unwrap()),
            );
            json!({
                "as": ec_try(|| json!(key.as_prop_key_set().0)),
                "str": ec_try(|| json!(key.string())),
                "desc": ec_try(|| json!([key.desc().key().0])),
                "add": ec_try(|| json!(set.add(key).0)),
                "rm": ec_try(|| json!(set.remove(key).0)),
                "has": ec_try(|| json!(set.contains(key))),
                "empty": set.is_empty(),
                "full": set.is_full(),
            })
        }
        "alloc" => {
            let a = new_simple_plan_column_id_allocator(p[0].as_i64().unwrap());
            let ids: Vec<i64> = (0..p[1].as_u64().unwrap())
                .map(|_| a.alloc_plan_column_id())
                .collect();
            json!({ "ids": ids, "last": a.get_last_plan_column_id() })
        }
        "eval" => {
            let ctx = es::new_eval_context(eval_opts(&p[0]));
            let has_time = has_opt(&p[0], &["time", "time_err"]);
            let mut m = serde_json::Map::new();
            m.insert("ctx".into(), eval_dump(&*ctx, has_time, false));
            if !p[1].is_null() {
                let nctx = ctx.apply(eval_opts(&p[1]));
                let ht = has_time || has_opt(&p[1], &["time", "time_err"]);
                m.insert("apply".into(), eval_dump(&*nctx, ht, false));
                m.insert("new_id".into(), json!(nctx.ctx_id() != ctx.ctx_id()));
                m.insert("apply_warn".into(), eval_dump(&*nctx, ht, true));
            }
            m.insert("warn".into(), eval_dump(&*ctx, has_time, true));
            Value::Object(m)
        }
        "expr" => {
            let ev = if p[0].is_null() {
                None
            } else {
                Some(es::new_eval_context(eval_opts(&p[0])))
            };
            let ctx = es::new_expr_context(expr_opts(&p[1], ev));
            let seeded = has_opt(&p[1], &["rng"]);
            let mut m = serde_json::Map::new();
            m.insert("ctx".into(), expr_dump(&ctx, false));
            m.insert("rng".into(), rng_dump(&ctx, seeded));
            if !p[2].is_null() {
                let nctx = ctx.apply(expr_opts(&p[2], None));
                m.insert("apply".into(), expr_dump(&nctx, false));
                m.insert(
                    "apply_rng".into(),
                    rng_dump(&nctx, seeded || has_opt(&p[2], &["rng"])),
                );
                m.insert("apply_warn".into(), expr_dump(&nctx, true));
            }
            m.insert("warn".into(), expr_dump(&ctx, true));
            Value::Object(m)
        }
        "truncate" => {
            let mut lm = [errctx::LevelError; 7];
            for (i, l) in p[1].as_array().unwrap().iter().enumerate() {
                lm[i] = errctx::Level(l.as_u64().unwrap() as u8);
            }
            let ev = es::new_eval_context(vec![
                es::with_type_flags(Flags(p[0].as_u64().unwrap() as u16)),
                es::with_err_level_map(lm),
            ]);
            let ctx = es::new_expr_context(vec![es::with_eval_ctx(Arc::clone(&ev))]);
            let base: BuildContextRef = Arc::clone(&ctx) as BuildContextRef;
            let level = errctx::Level(p[2].as_u64().unwrap() as u8);
            let nctx = ctx_with_handle_truncate_err_level(Arc::clone(&base), level);
            let nev = nctx.get_eval_ctx();
            let (ntc, nec) = (nev.type_ctx(), nev.err_ctx());
            ntc.append_warning(tidb_base::errors::new("w1"));
            nev.append_warning(tidb_base::errors::new("w2"));
            let ectx: ExprContextRef = Arc::clone(&ctx) as ExprContextRef;
            json!({
                "same": ptr(&nctx) == ptr(&base),
                "flags": ntc.flags().0,
                "levels": nec.level_map().iter().map(|l| l.0).collect::<Vec<_>>(),
                "id_same": nev.ctx_id() == ev.ctx_id(),
                "sqlmode": nev.sql_mode().0,
                "cs": nctx.get_charset_info().0,
                "warns": warns(&ev.copy_warnings(Vec::new())),
                "nr": [with_null_reject_check(Arc::clone(&ectx)).is_in_null_reject_check(),
                       with_null_reject_check(Arc::clone(&ectx)).is_constant_propagate_check()],
                "cp": [with_constant_propagate_check(Arc::clone(&ectx)).is_constant_propagate_check(),
                       with_constant_propagate_check(Arc::clone(&ectx)).is_in_null_reject_check()],
                "nr_gcml": with_null_reject_check(Arc::clone(&ectx)).get_group_concat_max_len(),
            })
        }
        "load" => {
            let mut sys = HashMap::new();
            let mut has_time = false;
            for kv in p[0].as_array().unwrap() {
                let kv = kv.as_array().unwrap();
                let (k, v) = (gstr(&kv[0]), gstr(&kv[1]));
                if k == "timestamp" {
                    has_time = true;
                }
                sys.insert(k, v);
            }
            let base = es::new_expr_context(vec![es::with_eval_ctx(es::new_eval_context(vec![
                es::with_current_time(Arc::new(|| Ok(unix(1600000000, 5)))),
            ]))]);
            let mut m = serde_json::Map::new();
            match base.load_system_vars(&sys) {
                Err(e) => {
                    m.insert("err".into(), gs(&e.error()));
                }
                Ok(nctx) => {
                    m.insert("expr".into(), expr_dump(&nctx, false));
                    m.insert(
                        "eval".into(),
                        eval_dump(&*nctx.get_static_eval_ctx(), true, false),
                    );
                }
            }
            let ev = es::new_eval_context(Vec::new());
            match ev.load_system_vars(&sys) {
                Err(e) => {
                    m.insert("eerr".into(), gs(&e.error()));
                }
                Ok(nev) => {
                    m.insert("eval2".into(), eval_dump(&*nev, has_time, false));
                }
            }
            Value::Object(m)
        }
        "static" => {
            let ev = es::new_eval_context(eval_opts(&p[0]));
            let ctx = es::new_expr_context(expr_opts(&p[1], Some(Arc::clone(&ev))));
            let sctx = es::make_expr_context_static(&*ctx);
            let last = vec![
                ctx.get_last_plan_column_id(),
                sctx.get_last_plan_column_id(),
                sctx.alloc_plan_column_id(),
                ctx.get_last_plan_column_id(),
            ];
            let mut m = serde_json::Map::new();
            m.insert("expr".into(), expr_dump(&sctx, false));
            m.insert(
                "eval".into(),
                eval_dump(
                    &*sctx.get_static_eval_ctx(),
                    has_opt(&p[0], &["time", "time_err"]),
                    false,
                ),
            );
            m.insert(
                "new_id".into(),
                json!(sctx.get_static_eval_ctx().ctx_id() != ev.ctx_id()),
            );
            m.insert(
                "same_tracker".into(),
                json!(Arc::ptr_eq(
                    &sctx.get_plan_cache_tracker(),
                    &ctx.get_plan_cache_tracker()
                )),
            );
            m.insert(
                "same_rng".into(),
                json!(Arc::ptr_eq(&sctx.rng(), &ctx.rng())),
            );
            m.insert("last".into(), json!(last));
            sctx.get_eval_ctx()
                .append_warning(tidb_base::errors::new("w-static"));
            m.insert("shared_warn".into(), json!(ev.warning_count()));
            Value::Object(m)
        }
        "getprop" => {
            let ctx = es::new_eval_context(Vec::new());
            let err = get_prop_provider::<DummyProvider>(&*ctx, OptPropCurrentUser)
                .err()
                .unwrap();
            let perr = EmptyParamValues.get_param_value(0).err().unwrap();
            json!({ "err": gs(&err.error()), "param": gs(&perr.error()) })
        }
        _ => panic!("exprctx: unknown op {op}"),
    }
}

fn replay(path: &str) {
    let mut gz = flate2::read::GzDecoder::new(std::fs::File::open(path).expect("vectors"));
    let mut text = String::new();
    gz.read_to_string(&mut text).unwrap();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "exprctx");
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let mut fails = vec![];
    let mut n = 0;
    for line in lines {
        n += 1;
        let rec: Value = serde_json::from_str(line).unwrap();
        let args = rec["args"].as_array().cloned().unwrap_or_default();
        let got = exec(rec["op"].as_str().unwrap(), &args);
        // Both sides through serde_json's parser (its float parsing is not exact).
        let got: Value = serde_json::from_str(&got.to_string()).unwrap();
        if got != rec["out"] {
            fails.push(format!("{line}\n    got {got}"));
        }
    }
    std::panic::set_hook(prev);
    let shown: Vec<_> = fails.iter().take(20).map(|s| s.as_str()).collect();
    assert!(
        fails.is_empty(),
        "{path}: {} of {n} records differ:\n{}",
        fails.len(),
        shown.join("\n")
    );
    assert_eq!(n as u64, header["records"].as_u64().unwrap());
}

#[test]
fn govec_exprctx() {
    replay(&vectors());
    if let Ok(files) = std::env::var("GOVEC_EXPRCTX_FILES") {
        for f in files.split(',').filter(|f| !f.is_empty()) {
            replay(f);
        }
    }
}

// Silence unused-import lints for traits used only through method resolution.
#[allow(dead_code)]
fn _uses(_: &dyn WarnAppender, _: &dyn WarnHandler, _: &dyn BuildContext) {}
