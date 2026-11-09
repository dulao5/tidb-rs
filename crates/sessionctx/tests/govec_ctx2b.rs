//! Replays the govec `ctx2b` vectors (tidb-rs-tracker `govec/domains/ctx2b.go`): block D00.1
//! part 2b — the expropt providers and readers, the optimizer debug trace, PlanCounterTp,
//! basicCtxType, the statement context's tracker init / detach and StatsLoadResult — against
//! Go TiDB v8.5.8.
//!
//! `GOVEC_CTX2B_FILES` (comma-separated paths) replays extra-seed artifacts as well.

use std::collections::BTreeMap;
use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tidb_base::Error;
use tidb_base::gostd::json::Value as GoValue;
use tidb_model::TableItemID;
use tidb_parser::auth::{RoleIdentity, UserIdentity};
use tidb_sessionctx::context::BasicCtxType;
use tidb_sessionctx::debugtrace::{OptimizerDebugTraceRoot, TraceValue};
use tidb_sessionctx::exprctx::{
    EvalContext, OptionalEvalPropProvider, OptionalEvalPropProviderRef,
};
use tidb_sessionctx::expropt::advisory_lock::{
    AdvisoryLockContext, AdvisoryLockPropReader, new_advisory_lock_prop_provider,
};
use tidb_sessionctx::expropt::current_user::{CurrentUserPropProvider, CurrentUserPropReader};
use tidb_sessionctx::expropt::ddlowner::{DDLOwnerInfoProvider, DDLOwnerPropReader};
use tidb_sessionctx::expropt::infoschema::{InfoSchemaPropProvider, InfoSchemaPropReader};
use tidb_sessionctx::expropt::kvstore::{KVStorePropProvider, KVStorePropReader};
use tidb_sessionctx::expropt::optional::RequireOptionalEvalProps;
use tidb_sessionctx::expropt::priv_::{
    PrivilegeChecker, PrivilegeCheckerPropReader, PrivilegeCheckerProvider,
};
use tidb_sessionctx::expropt::sequence::{
    SequenceOperator, SequenceOperatorPropReader, SequenceOperatorProvider,
};
use tidb_sessionctx::expropt::sqlexec::{
    SQLExecutor, SQLExecutorPropProvider, SQLExecutorPropReader,
};
use tidb_sessionctx::exprstatic::{new_eval_context, with_optional_property};
use tidb_sessionctx::planbase::PlanCounterTp;
use tidb_sessionctx::stmtctx::{StatsLoadResult, new_stmt_ctx};

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

fn vectors() -> String {
    format!(
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/ctx2b.jsonl.gz",
        env!("CARGO_MANIFEST_DIR")
    )
}

/// vec.GoString's JSON form (the replayed strings are valid UTF-8).
fn gs(b: &[u8]) -> Value {
    json!(String::from_utf8_lossy(b))
}

fn res(r: Result<Value, Error>) -> Value {
    match r {
        Ok(v) => json!({ "ok": v }),
        Err(e) => json!({ "err": gs(&e.error()) }),
    }
}

// --- expropt mocks (the same as the Go generator's) ---

struct Lock(u64);
impl AdvisoryLockContext for Lock {
    fn get_advisory_lock(&self, _: &str, _: i64) -> Result<(), Error> {
        Ok(())
    }
    fn is_used_advisory_lock(&self, _: &str) -> u64 {
        self.0
    }
    fn release_advisory_lock(&self, _: &str) -> bool {
        false
    }
    fn release_all_advisory_locks(&self) -> i64 {
        0
    }
}

struct Priv(bool);
impl PrivilegeChecker for Priv {
    fn request_verification(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: tidb_base::mysql::PrivilegeType,
    ) -> bool {
        self.0
    }
    fn request_dynamic_verification(&self, _: &str, _: bool) -> bool {
        self.0
    }
}

struct Seq(i64);
impl SequenceOperator for Seq {
    fn get_sequence_id(&self) -> i64 {
        self.0
    }
    fn get_sequence_next_val(&self) -> Result<i64, Error> {
        Ok(0)
    }
    fn set_sequence_val(&self, _: i64) -> Result<(i64, bool), Error> {
        Ok((0, false))
    }
}

struct Exec;
impl SQLExecutor for Exec {}

fn eval(has: bool, p: OptionalEvalPropProviderRef) -> Arc<dyn EvalContext> {
    if has {
        new_eval_context(vec![with_optional_property(vec![p])])
    } else {
        new_eval_context(vec![])
    }
}

fn desc(p: &dyn OptionalEvalPropProvider) -> Value {
    let k = p.desc().key();
    json!([k.0, k.string()])
}

fn expropt(key: i64, pl: &Value) -> Value {
    let has = !pl.is_null();
    let mut m = serde_json::Map::new();
    let p: OptionalEvalPropProviderRef;
    match key {
        0 => {
            let user = pl.get(0).filter(|u| !u.is_null()).map(|u| UserIdentity {
                username: u[0].as_str().unwrap().into(),
                hostname: u[1].as_str().unwrap().into(),
                ..Default::default()
            });
            let roles: Vec<RoleIdentity> = pl
                .get(1)
                .and_then(|r| r.as_array())
                .map(|rs| {
                    rs.iter()
                        .map(|r| RoleIdentity {
                            username: r[0].as_str().unwrap().into(),
                            hostname: r[1].as_str().unwrap().into(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            let pv = CurrentUserPropProvider(Arc::new(move || (user.clone(), roles.clone())));
            p = Arc::new(pv);
            let r = CurrentUserPropReader;
            m.insert("keys".into(), json!(r.required_optional_eval_props().0));
            let ctx = eval(has, p.clone());
            m.insert(
                "user".into(),
                res(r.current_user(&*ctx).map(|u| match u {
                    None => Value::Null,
                    Some(u) => json!([u.username, u.hostname]),
                })),
            );
            m.insert(
                "roles".into(),
                res(r.active_roles(&*ctx).map(|rs| {
                    Value::Array(rs.iter().map(|x| json!([x.username, x.hostname])).collect())
                })),
            );
        }
        7 => {
            let v = pl.as_bool().unwrap_or(false);
            p = Arc::new(DDLOwnerInfoProvider(Arc::new(move || v)));
            let r = DDLOwnerPropReader;
            m.insert("keys".into(), json!(r.required_optional_eval_props().0));
            m.insert(
                "owner".into(),
                res(r.is_ddl_owner(&*eval(has, p.clone())).map(|b| json!(b))),
            );
        }
        2 => {
            let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
            let s2 = seen.clone();
            p = Arc::new(InfoSchemaPropProvider(Arc::new(move |is_domain| {
                s2.lock().unwrap().push(json!(is_domain));
                None
            })));
            let r = InfoSchemaPropReader;
            m.insert("keys".into(), json!(r.required_optional_eval_props().0));
            let ctx = eval(has, p.clone());
            m.insert(
                "sess".into(),
                res(r
                    .get_session_info_schema(&*ctx)
                    .map(|is| json!(is.is_none()))),
            );
            m.insert(
                "dom".into(),
                res(r
                    .get_domain_info_schema(&*ctx)
                    .map(|is| json!(is.is_none()))),
            );
            let s = seen.lock().unwrap().clone();
            m.insert(
                "seen".into(),
                if s.is_empty() {
                    Value::Null
                } else {
                    Value::Array(s)
                },
            );
        }
        3 => {
            p = Arc::new(KVStorePropProvider(Arc::new(|| unreachable!("no store"))));
            let r = KVStorePropReader;
            m.insert("keys".into(), json!(r.required_optional_eval_props().0));
            m.insert(
                "store".into(),
                res(r
                    .get_kv_store(&*eval(false, p.clone()))
                    .map(|_| Value::Null)),
            );
        }
        6 => {
            let tag = pl.as_u64().unwrap_or(0);
            p = Arc::new(new_advisory_lock_prop_provider(Arc::new(Lock(tag))));
            let r = AdvisoryLockPropReader;
            m.insert("keys".into(), json!(r.required_optional_eval_props().0));
            m.insert(
                "lock".into(),
                res(r
                    .advisory_lock_ctx(&*eval(has, p.clone()))
                    .map(|c| json!(c.is_used_advisory_lock("x")))),
            );
        }
        8 => {
            let tag = pl.as_bool().unwrap_or(false);
            p = Arc::new(PrivilegeCheckerProvider(Arc::new(move || {
                Arc::new(Priv(tag))
            })));
            let r = PrivilegeCheckerPropReader;
            m.insert("keys".into(), json!(r.required_optional_eval_props().0));
            m.insert(
                "priv".into(),
                res(r
                    .get_privilege_checker(&*eval(has, p.clone()))
                    .map(|c| json!(c.request_dynamic_verification("p", false)))),
            );
        }
        5 => {
            let id = pl.get(0).and_then(|v| v.as_i64()).unwrap_or(0);
            let emsg = pl.get(1).and_then(|v| v.as_str()).map(str::to_owned);
            let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
            let s2 = seen.clone();
            p = Arc::new(SequenceOperatorProvider(Arc::new(
                move |db: &str, name: &str| {
                    s2.lock().unwrap().extend([json!(db), json!(name)]);
                    match &emsg {
                        Some(e) => Err(tidb_base::errors::new(e.clone())),
                        None => Ok(Arc::new(Seq(id)) as Arc<dyn SequenceOperator>),
                    }
                },
            )));
            let r = SequenceOperatorPropReader;
            m.insert("keys".into(), json!(r.required_optional_eval_props().0));
            m.insert(
                "seq".into(),
                res(r
                    .get_sequence_operator(&*eval(has, p.clone()), "db1", "seq<&>")
                    .map(|s| json!(s.get_sequence_id()))),
            );
            let s = seen.lock().unwrap().clone();
            m.insert(
                "seen".into(),
                if s.is_empty() {
                    Value::Null
                } else {
                    Value::Array(s)
                },
            );
        }
        4 => {
            let emsg = pl.get(1).and_then(|v| v.as_str()).map(str::to_owned);
            p = Arc::new(SQLExecutorPropProvider(Arc::new(move || match &emsg {
                Some(e) => Err(tidb_base::errors::new(e.clone())),
                None => Ok(Arc::new(Exec) as Arc<dyn SQLExecutor>),
            })));
            let r = SQLExecutorPropReader;
            m.insert("keys".into(), json!(r.required_optional_eval_props().0));
            m.insert(
                "exec".into(),
                res(r
                    .get_sql_executor(&*eval(has, p.clone()))
                    .map(|_| json!(true))),
            );
        }
        _ => return json!({ "skip": true }),
    }
    m.insert("desc".into(), desc(&*p));
    Value::Object(m)
}

// --- debugtrace ---

fn dt_value(v: &Value) -> TraceValue {
    let a = v.as_array().unwrap();
    match a[0].as_str().unwrap() {
        "n" => TraceValue::Json(GoValue::Null),
        "b" => TraceValue::Json(GoValue::Bool(a[1].as_bool().unwrap())),
        "i" => TraceValue::Json(GoValue::Int(a[1].as_i64().unwrap() as i128)),
        // Floats travel as strings (Go strconv.FormatFloat 'g' / NaN / ±Inf); Rust's parser is
        // correctly rounded (serde_json's default one is not).
        "f" => TraceValue::Json(GoValue::Number(match a[1].as_str().unwrap() {
            "NaN" => f64::NAN,
            "+Inf" => f64::INFINITY,
            "-Inf" => f64::NEG_INFINITY,
            s => s.parse().unwrap(),
        })),
        "s" => TraceValue::Json(GoValue::String(a[1].as_str().unwrap().to_owned())),
        "a" => TraceValue::Array(a[1].as_array().unwrap().iter().map(dt_value).collect()),
        "m" => {
            let mut m = BTreeMap::new();
            for kv in a[1].as_array().unwrap() {
                m.insert(kv[0].as_str().unwrap().to_owned(), dt_value(&kv[1]));
            }
            TraceValue::Map(m)
        }
        t => panic!("tag {t}"),
    }
}

/// The names Go's `runtime.FuncForPC` gives the generator's helpers.
const DT_ENTER: [&str; 3] = [
    "github.com/dulao5/tidb-rs-tracker/govec/domains.dtEnter0",
    "github.com/dulao5/tidb-rs-tracker/govec/domains.dtEnter1",
    "github.com/dulao5/tidb-rs-tracker/govec/domains.dtEnter2",
];

fn debugtrace(script: &[Value]) -> Value {
    catch_unwind(AssertUnwindSafe(|| {
        let mut root = OptimizerDebugTraceRoot::new();
        let mut enc = vec![];
        for st in script {
            let st = st.as_array().unwrap();
            match st[0].as_str().unwrap() {
                "step" => root.append_step_to_current_context(dt_value(&st[1])),
                "named" => root.append_step_with_name_to_current_context(
                    dt_value(&st[2]),
                    st[1].as_str().unwrap(),
                ),
                "enter" => root.enter_context(DT_ENTER[st[1].as_u64().unwrap() as usize]),
                "leave" => root.leave_context(),
                "record" => root.record_any_values_with_names(
                    st[1].as_array().unwrap().iter().map(dt_value).collect(),
                ),
                "encode" => enc.push(json!({ "out": root.encode_json_common(&dt_value(&st[1])) })),
                op => panic!("op {op}"),
            }
        }
        json!({ "enc": enc, "root": { "out": root.marshal_json() } })
    }))
    .unwrap_or_else(|_| json!({ "panic": true }))
}

fn exec(op: &str, p: &[Value]) -> Value {
    match op {
        "plancounter" => {
            let mut c = PlanCounterTp(p[0].as_i64().unwrap());
            let mut m = serde_json::Map::new();
            m.insert("force".into(), json!(c.is_force()));
            m.insert("empty".into(), json!(c.empty()));
            let mut steps = vec![];
            for x in p[1].as_array().unwrap() {
                c.dec(x.as_i64().unwrap());
                steps.push(json!([c.0, c.empty(), c.is_force()]));
            }
            m.insert("steps".into(), Value::Array(steps));
            Value::Object(m)
        }
        "basicctx" => json!(BasicCtxType(p[0].as_i64().unwrap()).string()),
        "statsload" => {
            let r = StatsLoadResult {
                item: TableItemID {
                    table_id: p[0].as_i64().unwrap(),
                    id: p[1].as_i64().unwrap(),
                    is_index: p[2].as_bool().unwrap(),
                    ..Default::default()
                },
                error: p[3].as_str().map(|s| tidb_base::errors::new(s.to_owned())),
            };
            json!({ "has": r.has_error(), "msg": gs(&r.error_msg()) })
        }
        "tracker" => {
            let mut sc = new_stmt_ctx();
            sc.init_mem_tracker(p[0].as_i64().unwrap() as isize, p[1].as_i64().unwrap());
            sc.init_disk_tracker(p[2].as_i64().unwrap() as isize, p[3].as_i64().unwrap());
            let parent = tidb_memory::tracker::new_tracker(-100, -1);
            let dparent = tidb_memory::tracker::new_tracker(-101, -1);
            let mt = sc.mem_tracker.clone().unwrap();
            let dt = sc.disk_tracker.clone().unwrap();
            mt.attach_to(&parent).unwrap();
            dt.attach_to(&dparent).unwrap();
            let n = p[4].as_i64().unwrap();
            mt.consume(n).unwrap();
            dt.consume(n / 2).unwrap();
            let mut m = serde_json::Map::new();
            m.insert("mlabel".into(), json!(mt.label()));
            m.insert("mlimit".into(), json!(mt.get_bytes_limit()));
            m.insert("dlabel".into(), json!(dt.label()));
            m.insert("dlimit".into(), json!(dt.get_bytes_limit()));
            m.insert("pc".into(), json!(parent.bytes_consumed()));
            m.insert("dpc".into(), json!(dparent.bytes_consumed()));
            if p[5].as_bool().unwrap() {
                sc.detach_mem_disk_tracker();
                m.insert("pc2".into(), json!(parent.bytes_consumed()));
                m.insert("dpc2".into(), json!(dparent.bytes_consumed()));
                m.insert("mc2".into(), json!(mt.bytes_consumed()));
                m.insert("dc2".into(), json!(dt.bytes_consumed()));
            }
            Value::Object(m)
        }
        "expropt" => expropt(p[0].as_i64().unwrap(), &p[1]),
        "debugtrace" => debugtrace(p[0].as_array().unwrap()),
        _ => panic!("ctx2b: unknown op {op}"),
    }
}

fn replay(path: &str) {
    let mut gz = flate2::read::GzDecoder::new(std::fs::File::open(path).expect("vectors"));
    let mut text = String::new();
    gz.read_to_string(&mut text).unwrap();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "ctx2b");
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let mut fails = vec![];
    let mut n = 0;
    for line in lines {
        n += 1;
        let rec: Value = serde_json::from_str(line).unwrap();
        let args = rec["args"].as_array().cloned().unwrap_or_default();
        let got = exec(rec["op"].as_str().unwrap(), &args);
        if got != rec["out"] {
            fails.push(format!("{line}\n    got {got}"));
        }
    }
    std::panic::set_hook(prev);
    let shown: Vec<_> = fails.iter().take(30).map(|s| s.as_str()).collect();
    assert!(
        fails.is_empty(),
        "{path}: {} of {n} records differ:\n{}",
        fails.len(),
        shown.join("\n")
    );
    assert_eq!(n as u64, header["records"].as_u64().unwrap());
}

#[test]
fn govec_ctx2b() {
    replay(&vectors());
    if let Ok(files) = std::env::var("GOVEC_CTX2B_FILES") {
        for f in files.split(',').filter(|f| !f.is_empty()) {
            replay(f);
        }
    }
}
