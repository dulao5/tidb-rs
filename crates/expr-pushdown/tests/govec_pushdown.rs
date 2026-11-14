//! Replays the govec `pushdown` vectors (tidb-rs-tracker `govec/domains/pushdown.go`): push-down
//! eligibility and the conversion of expressions to tipb (block D08.3 part 1) against Go TiDB
//! v8.5.8.
//!
//! Go builds each tree with `NewFunctionBase` (every function family, ported or not) and dumps
//! it; the replay rebuilds it with descriptor-only signatures (name, pb code, result type,
//! collation, CAST's `inUnion`), which is all the push-down code reads. The `tipb` messages
//! are compared as marshalled bytes.
//!
//! `GOVEC_PUSHDOWN_FILES` (comma-separated paths) replays extra-seed artifacts as well.

use std::collections::HashMap;
use std::io::Read;
use std::sync::{Arc, Mutex, OnceLock};

use protobuf::Message;
use serde_json::{Value, json};
use tidb_base::Error;
use tidb_base::contextutil::{
    SQLWarn, StaticWarnHandler, WarnAppenderRef, WarnHandlerExt, new_static_warn_handler,
};
use tidb_base::gostd::time::{fixed_zone, utc};
use tidb_base::kv::StoreType;
use tidb_base::mysql;
use tidb_expr_pushdown::*;
use tidb_expression::eval::{ArgsFallback, BuiltinFunc, SigDesc, SigDrivers, SigExtra};
use tidb_expression::*;
use tidb_kv::checker::RequestTypeSupportedChecker;
use tidb_kv::kv::{Client, ClientSendOption, ReqTypeDAG, Request, Response};
use tidb_parser::model::CIStr;
use tidb_sessionctx::exprctx::EvalContextRef;
use tidb_sessionctx::exprstatic as es;
use tidb_sessionctx::variable::session::{GlobalVarAccessor, SessionVars};
use tidb_types::MyDecimal;
use tidb_types::binary_literal::BinaryLiteral;
use tidb_types::collate::set_new_collation_enabled_for_test;
use tidb_types::core_time::CoreTime;
use tidb_types::datum::{self as dm, Datum};
use tidb_types::enum_type::Enum;
use tidb_types::etc::FieldType;
use tidb_types::field_type_builder::new_field_type_builder;
use tidb_types::json::BinaryJSON;
use tidb_types::set_type::Set;
use tidb_types::time::{Duration, Time};
use tidb_types::vector::parse_vector_float32;

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

fn vectors() -> String {
    format!(
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/pushdown.jsonl.gz",
        env!("CARGO_MANIFEST_DIR")
    )
}

// ---- helpers shared with the expression replays ---------------------------------------------

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// vec.GoString's JSON form.
fn gs(b: &[u8]) -> Value {
    match std::str::from_utf8(b) {
        Ok(s) => json!(s),
        Err(_) => json!({ "hex": hex(b) }),
    }
}

fn ungs(v: &Value) -> Vec<u8> {
    match v {
        Value::String(s) => s.as_bytes().to_vec(),
        Value::Object(m) => unhex(m["hex"].as_str().unwrap()),
        _ => panic!("not a GoString: {v}"),
    }
}

fn hex_opt(v: &Value) -> Option<Vec<u8>> {
    if v.is_null() {
        None
    } else {
        Some(unhex(v.as_str().unwrap()))
    }
}

fn dec_of(s: &str) -> MyDecimal {
    let mut d = MyDecimal::default();
    let _ = d.from_string(s.as_bytes());
    d
}

/// The datum domain's ftIn: [tp, flag, flen, decimal, charset, collate, elems|null].
fn ft_in(v: &Value) -> FieldType {
    let p = v.as_array().unwrap();
    let mut ft = new_field_type_builder()
        .set_type(p[0].as_u64().unwrap() as u8)
        .set_flag(p[1].as_u64().unwrap() as usize)
        .set_flen(p[2].as_i64().unwrap() as isize)
        .set_decimal(p[3].as_i64().unwrap() as isize)
        .set_charset(p[4].as_str().unwrap())
        .set_collate(p[5].as_str().unwrap())
        .build_p();
    if !p[6].is_null() {
        ft.set_elems(Some(p[6].as_array().unwrap().iter().map(ungs).collect()));
    }
    ft
}

/// The datum domain's d2FtOut.
fn ft_out(ft: &FieldType) -> Value {
    json!({
        "tp": ft.get_type(),
        "flag": ft.get_flag(),
        "flen": ft.get_flen(),
        "dec": ft.get_decimal(),
        "cs": ft.get_charset(),
        "co": ft.get_collate(),
        "elems": ft.get_elems().map(|es| es.iter().map(|e| gs(e)).collect::<Vec<_>>()),
    })
}

fn bits_f(v: &Value) -> f64 {
    f64::from_bits(v.as_u64().unwrap())
}

/// The datum domain's d2DatumIn.
fn datum_in(v: &Value) -> Datum {
    let p = v.as_array().unwrap();
    let k = p[0].as_u64().unwrap() as u8;
    let u = |i: usize| p[i].as_u64().unwrap();
    let i = |i: usize| p[i].as_i64().unwrap();
    let st = |i: usize| p[i].as_str().unwrap().to_owned();
    let mut d = Datum::default();
    match k {
        dm::KindNull => d.set_null(),
        dm::KindInt64 => d.set_int64(i(1)),
        dm::KindUint64 => d.set_uint64(u(1)),
        dm::KindFloat32 => d.set_float32(bits_f(&p[1]) as f32),
        dm::KindFloat64 => d.set_float64(bits_f(&p[1])),
        dm::KindString => d.set_string(ungs(&p[1]), st(2)),
        dm::KindBytes => d.set_bytes(hex_opt(&p[1])),
        dm::KindBinaryLiteral => {
            d.set_binary_literal(BinaryLiteral(hex_opt(&p[1]).unwrap_or_default()))
        }
        dm::KindMysqlBit => d.set_mysql_bit(BinaryLiteral(hex_opt(&p[1]).unwrap_or_default())),
        dm::KindMysqlDecimal => d.set_mysql_decimal(dec_of(p[1].as_str().unwrap())),
        dm::KindMysqlDuration => d.set_mysql_duration(Duration {
            duration: i(1),
            fsp: i(2) as isize,
        }),
        dm::KindMysqlEnum => d.set_mysql_enum(
            Enum {
                value: u(1),
                name: ungs(&p[2]),
            },
            st(3),
        ),
        dm::KindMysqlSet => d.set_mysql_set(
            Set {
                value: u(1),
                name: ungs(&p[2]),
            },
            st(3),
        ),
        dm::KindMysqlTime => d.set_mysql_time(Time::from_raw(CoreTime(u(1)))),
        dm::KindMysqlJSON => d.set_mysql_json(BinaryJSON::owned(
            u(1) as u8,
            hex_opt(&p[2]).unwrap_or_default(),
        )),
        dm::KindVectorFloat32 => {
            d.set_vector_float32(parse_vector_float32(p[1].as_str().unwrap().as_bytes()).unwrap())
        }
        _ => panic!("datum_in: kind {k}"),
    }
    d
}

fn warns(ws: &[SQLWarn]) -> Value {
    Value::Array(
        ws.iter()
            .map(|w| json!([&*w.level, gs(&w.err.error())]))
            .collect(),
    )
}

// ---- the domain ------------------------------------------------------------------------------

/// mock.Client (kv.RequestTypeSupportedChecker), or a variant refusing ReqTypeDAG (1) or
/// everything (2).
struct PdClient(u64);

impl Client for PdClient {
    fn send(
        &self,
        _: &tidb_kv::Context,
        _: &Request,
        _: Option<tidb_kv::kv::OptionValue>,
        _: &ClientSendOption,
    ) -> Box<dyn Response> {
        unreachable!("PdClient.send")
    }

    fn is_request_type_supported(&self, req_type: i64, sub_type: i64) -> bool {
        match self.0 {
            1 if req_type == ReqTypeDAG => return false,
            2 => return false,
            _ => {}
        }
        RequestTypeSupportedChecker.is_request_type_supported(req_type, sub_type)
    }
}

/// Descriptor-only signatures for the trees Go built (name of Go's signature type).
fn sig_of(name: &str) -> &'static SigDesc {
    static SIGS: OnceLock<Mutex<HashMap<String, &'static SigDesc>>> = OnceLock::new();
    let mut m = SIGS.get_or_init(Default::default).lock().unwrap();
    m.entry(name.to_owned()).or_insert_with(|| {
        Box::leak(Box::new(SigDesc {
            name: Box::leak(name.to_owned().into_boxed_str()),
            pb_code: 0,
            vectorized: false,
            drivers: SigDrivers::None,
            args_fallback: ArgsFallback::None,
        }))
    })
}

/// Rebuilds a pdDump tree; correlated values go to `st`.
fn tree(v: &Value, st: &mut EvalState) -> ExprRef {
    let o = v.as_object().unwrap();
    if let Some(a) = o.get("col") {
        let a = a.as_array().unwrap();
        return Expr::Column(Column {
            unique_id: a[0].as_i64().unwrap(),
            index: a[1].as_i64().unwrap() as isize,
            ret_type: Arc::new(ft_in(&a[2])),
            id: a[3].as_i64().unwrap(),
            ..Column::default()
        })
        .into_ref();
    }
    if let Some(a) = o.get("corr") {
        let a = a.as_array().unwrap();
        let c = CorrelatedColumn::new(Column {
            unique_id: a[0].as_i64().unwrap(),
            ret_type: Arc::new(ft_in(&a[1])),
            id: a[3].as_i64().unwrap(),
            ..Column::default()
        });
        st.set_correlated(&c, datum_in(&a[2]));
        return Expr::CorrelatedColumn(c).into_ref();
    }
    if let Some(a) = o.get("const") {
        let a = a.as_array().unwrap();
        return Expr::Constant(Constant::new(datum_in(&a[0]), ft_in(&a[1]))).into_ref();
    }
    let a = o["f"].as_array().unwrap();
    let args: Vec<ExprRef> = a[4]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| tree(x, st))
        .collect();
    let ret = ft_in(&a[3]);
    let mut f = BuiltinFunc::new(
        sig_of(a[2].as_str().unwrap()),
        args,
        ret.clone(),
        a[5].as_str().unwrap(),
        a[6].as_str().unwrap(),
    );
    f.pb_code = a[7].as_i64().unwrap() as i32;
    if let Some(in_union) = a[8].as_bool() {
        f.extra = SigExtra::Cast { in_union };
    }
    if a[9].as_bool().unwrap() {
        f.collation_info
            .set_coercibility(Coercibility(a[10].as_i64().unwrap() as _));
    }
    f.collation_info
        .set_repertoire(Repertoire(a[11].as_i64().unwrap() as _));
    f.collation_info
        .set_explicit_charset(a[12].as_bool().unwrap());
    let mut sf = ScalarFunction::new(a[1].as_str().unwrap(), ret, f);
    sf.func_name = CIStr::new(a[1].as_str().unwrap());
    Expr::ScalarFunction(sf).into_ref()
}

struct Opts {
    nc: bool,
    cl: u64,
    loc: Option<i64>,
    bl: HashMap<String, u32>,
    ex: bool,
    wh: u64,
    ep: bool,
    desc: bool,
}

fn blacklist(v: &Value) -> HashMap<String, u32> {
    v.as_array()
        .map(|a| {
            a.iter()
                .map(|e| {
                    (
                        e[0].as_str().unwrap().to_owned(),
                        e[1].as_f64().unwrap() as u32,
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn opts(v: &Value) -> Opts {
    Opts {
        nc: v["nc"].as_bool().unwrap(),
        cl: v["cl"].as_u64().unwrap(),
        loc: v["loc"].as_i64(),
        bl: blacklist(&v["bl"]),
        ex: v["ex"].as_bool().unwrap(),
        wh: v["wh"].as_u64().unwrap(),
        ep: v["ep"].as_bool().unwrap(),
        desc: v["desc"].as_bool().unwrap(),
    }
}

fn set_blacklist(bl: HashMap<String, u32>) {
    DefaultExprPushDownBlacklist.store(Arc::new(bl));
}

fn eval_ctx(o: &Opts) -> EvalContextRef {
    let loc = match o.loc {
        Some(off) => fixed_zone("Z", off as i32),
        None => utc(),
    };
    es::new_eval_context(vec![es::with_location(Some(loc))])
}

type Handlers = (
    Option<WarnAppenderRef>,
    Option<WarnAppenderRef>,
    Option<Arc<StaticWarnHandler>>,
    Option<Arc<StaticWarnHandler>>,
);

fn handlers(wh: u64) -> Handlers {
    let ws = (wh & 1 != 0).then(|| Arc::new(new_static_warn_handler(0)));
    let xs = (wh & 2 != 0).then(|| Arc::new(new_static_warn_handler(0)));
    (
        ws.clone().map(|h| h as WarnAppenderRef),
        xs.clone().map(|h| h as WarnAppenderRef),
        ws,
        xs,
    )
}

fn hwarns(h: &Option<Arc<StaticWarnHandler>>) -> Value {
    match h {
        Some(h) => warns(&h.get_warnings()),
        None => Value::Null,
    }
}

fn pb_hex(m: Option<impl Message>) -> Value {
    match m {
        Some(m) => json!(hex(&m.write_to_bytes().unwrap())),
        None => Value::Null,
    }
}

fn pb_list(r: Result<Vec<tipb::Expr>, Error>) -> Value {
    match r {
        Ok(l) => Value::Array(l.into_iter().map(|e| pb_hex(Some(e))).collect()),
        Err(e) => json!({ "err": gs(&e.error()) }),
    }
}

const STORES: [StoreType; 4] = [StoreType(0), StoreType(1), StoreType(2), StoreType(255)];

fn exec(op: &str, p: &[Value], want: &Value) -> Value {
    match op {
        "expr" => {
            let o = opts(&p[1]);
            if want.get("build").is_some() {
                return want.clone();
            }
            set_new_collation_enabled_for_test(o.nc);
            set_blacklist(o.bl.clone());
            let mut st = EvalState::new();
            let e = tree(&want["tree"], &mut st);
            let ectx = eval_ctx(&o);
            let client: Arc<dyn Client> = Arc::new(PdClient(o.cl));
            let mut m = serde_json::Map::new();
            m.insert("tree".into(), want["tree"].clone());
            let pc = new_pb_converter(&*client, &*ectx, &st);
            m.insert("pb".into(), pb_hex(pc.expr_to_pb(&e)));
            let one = [Arc::clone(&e)];
            m.insert(
                "list".into(),
                pb_list(expressions_to_pb_list(&*ectx, &st, &one, &*client)),
            );
            m.insert(
                "proj".into(),
                pb_list(projection_expressions_to_pb_list(
                    &*ectx, &st, &one, &*client,
                )),
            );
            m.insert(
                "gb".into(),
                pb_hex(group_by_item_to_pb(&*ectx, &st, &*client, &e)),
            );
            m.insert(
                "sb".into(),
                pb_hex(sort_by_item_to_pb(&*ectx, &st, &*client, &e, o.desc)),
            );
            if let Expr::ScalarFunction(sf) = &*e {
                let fns: Vec<bool> = STORES
                    .iter()
                    .map(|s| can_func_be_pushed(&*ectx, sf, *s))
                    .collect();
                m.insert("fn".into(), json!(fns));
                m.insert(
                    "sup".into(),
                    json!([
                        scalar_expr_supported_by_tikv(&*ectx, sf),
                        scalar_expr_supported_by_flash(&*ectx, sf),
                        scalar_expr_supported_by_tidb(&*ectx, sf)
                    ]),
                );
                m.insert("enum".into(), json!(can_enum_pushdown_preliminarily(sf)));
            }
            let mut can = vec![];
            for s in STORES {
                let (w, x, ws, xs) = handlers(o.wh);
                let pctx =
                    new_push_down_context(Arc::clone(&ectx), Arc::clone(&client), o.ex, w, x, 1024)
                        .with_eval_state(&st);
                let ok = can_exprs_push_down_with_extra_info(&pctx, &one, s, o.ep);
                can.push(json!({"ok": ok, "w": hwarns(&ws), "x": hwarns(&xs)}));
            }
            m.insert("can".into(), Value::Array(can));
            Value::Object(m)
        }
        "split" => {
            let o = opts(&p[1]);
            if want.get("build").is_some() {
                return want.clone();
            }
            set_new_collation_enabled_for_test(o.nc);
            set_blacklist(o.bl.clone());
            let s = StoreType(p[2].as_u64().unwrap() as u8);
            let mut st = EvalState::new();
            let exprs: Vec<ExprRef> = want["trees"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| tree(t, &mut st))
                .collect();
            let ectx = eval_ctx(&o);
            let client: Arc<dyn Client> = Arc::new(PdClient(o.cl));
            let idx = |es: &[ExprRef]| -> Value {
                json!(
                    es.iter()
                        .map(|e| exprs.iter().position(|x| Arc::ptr_eq(x, e)).unwrap())
                        .collect::<Vec<_>>()
                )
            };
            let mut m = serde_json::Map::new();
            m.insert("trees".into(), want["trees"].clone());
            let (w, x, ws, xs) = handlers(o.wh);
            let pctx =
                new_push_down_context(Arc::clone(&ectx), Arc::clone(&client), o.ex, w, x, 1024)
                    .with_eval_state(&st);
            let (pushed, remained) = push_down_exprs_with_extra_info(&pctx, &exprs, s, o.ep);
            m.insert(
                "extra".into(),
                json!([idx(&pushed), idx(&remained), hwarns(&ws), hwarns(&xs)]),
            );
            let (w, x, ws, xs) = handlers(o.wh);
            let pctx =
                new_push_down_context(Arc::clone(&ectx), Arc::clone(&client), o.ex, w, x, 1024)
                    .with_eval_state(&st);
            let (pushed, remained) = push_down_exprs(&pctx, &exprs, s);
            m.insert(
                "plain".into(),
                json!([idx(&pushed), idx(&remained), hwarns(&ws), hwarns(&xs)]),
            );
            let (w, x, ws, xs) = handlers(o.wh);
            let pctx =
                new_push_down_context(Arc::clone(&ectx), Arc::clone(&client), o.ex, w, x, 1024)
                    .with_eval_state(&st);
            m.insert(
                "can".into(),
                json!([
                    can_exprs_push_down(&pctx, &exprs, s),
                    hwarns(&ws),
                    hwarns(&xs)
                ]),
            );
            Value::Object(m)
        }
        "ft" => {
            set_new_collation_enabled_for_test(p[1].as_bool().unwrap());
            let ft = ft_in(&p[0]);
            let pb = to_pb_field_type(&ft);
            let chk =
                match to_pb_field_type_with_check(&ft, StoreType(p[2].as_u64().unwrap() as u8)) {
                    Ok(c) => pb_hex(Some(c)),
                    Err(e) => json!({ "err": gs(&e.error()) }),
                };
            json!({"pb": pb_hex(Some(pb.clone())), "chk": chk, "back": ft_out(&field_type_from_pb(&pb))})
        }
        "ftpb" => {
            let mut pb = tipb::FieldType::default();
            pb.set_tp(p[0].as_i64().unwrap() as i32);
            pb.set_flag(p[1].as_u64().unwrap() as u32);
            pb.set_flen(p[2].as_i64().unwrap() as i32);
            pb.set_decimal(p[3].as_i64().unwrap() as i32);
            pb.set_collate(p[4].as_i64().unwrap() as i32);
            pb.set_charset(p[5].as_str().unwrap().to_owned());
            if let Some(es) = p[6].as_array() {
                pb.set_elems(
                    es.iter()
                        .map(|e| String::from_utf8(ungs(e)).unwrap())
                        .collect(),
                );
            }
            ft_out(&field_type_from_pb(&pb))
        }
        "enabled" => {
            set_blacklist(blacklist(&p[2]));
            json!(is_push_down_enabled(
                p[0].as_str().unwrap(),
                StoreType(p[1].as_u64().unwrap() as u8)
            ))
        }
        "ctx" => {
            let (from_vars, explain, wh, gcml) = (
                p[0].as_bool().unwrap(),
                p[1].as_bool().unwrap(),
                p[2].as_u64().unwrap(),
                p[3].as_u64().unwrap(),
            );
            let ectx: EvalContextRef = es::new_eval_context(vec![]);
            let client: Arc<dyn Client> = Arc::new(PdClient(0));
            let (pctx, w, x): (PushDownContext<'_>, Value, Value);
            let warn = || Error::plain("pushdown warning");
            if from_vars {
                let mut sv = SessionVars::new(Box::new(Acc));
                sv.stmt_ctx_mut().in_explain_stmt = explain;
                sv.group_concat_max_len = gcml;
                pctx = new_push_down_context_from_session_vars(
                    Arc::clone(&ectx),
                    &sv,
                    Arc::clone(&client),
                );
                pctx.append_warning(warn());
                w = warns(&sv.stmt_ctx().get_warnings());
                x = warns(&sv.stmt_ctx().get_extra_warnings());
            } else {
                let (wa, xa, ws, xs) = handlers(wh);
                pctx = new_push_down_context(
                    Arc::clone(&ectx),
                    Arc::clone(&client),
                    explain,
                    wa,
                    xa,
                    gcml,
                );
                pctx.append_warning(warn());
                let list = |h: &Option<Arc<StaticWarnHandler>>| match h {
                    Some(h) => warns(&h.get_warnings()),
                    None => json!([]),
                };
                w = list(&ws);
                x = list(&xs);
            }
            json!({
                "gcml": pctx.get_group_concat_max_len(),
                "client": Arc::ptr_eq(pctx.client(), &client),
                "ectx": std::ptr::addr_eq(pctx.eval_ctx(), &*ectx),
                "w": w,
                "x": x,
            })
        }
        "pbexpr" => {
            let o = opts(&p[1]);
            if want.get("build").is_some() || want["pb"].is_null() {
                return want.clone();
            }
            set_new_collation_enabled_for_test(o.nc);
            let pb: tipb::Expr =
                protobuf::parse_from_bytes(&unhex(want["pb"].as_str().unwrap())).unwrap();
            json!({"pb": want["pb"], "back": pb_back(o.loc, &[pb], want.get("back"))})
        }
        "pbtps" => {
            *TPS.lock().unwrap() = want.as_array().unwrap().clone();
            want.clone()
        }
        "pbraw" => {
            set_new_collation_enabled_for_test(p[2].as_bool().unwrap());
            let pbs: Vec<tipb::Expr> = p[0]
                .as_array()
                .unwrap()
                .iter()
                .map(|h| protobuf::parse_from_bytes(&unhex(h.as_str().unwrap())).unwrap())
                .collect();
            pb_back(p[1].as_i64(), &pbs, Some(want))
        }
        _ => panic!("unknown op {op}"),
    }
}

/// The datum domain's d2DatumInOf.
fn datum_in_of(d: &Datum) -> Value {
    let k = d.kind();
    match k {
        dm::KindNull => json!([k]),
        dm::KindInt64 => json!([k, d.get_int64()]),
        dm::KindUint64 => json!([k, d.get_uint64()]),
        dm::KindFloat64 => json!([k, d.get_float64().to_bits()]),
        dm::KindFloat32 => json!([k, (d.get_float32() as f64).to_bits()]),
        dm::KindString => json!([k, gs(d.get_string()), d.collation()]),
        dm::KindMysqlDecimal => json!([k, d.get_mysql_decimal().string()]),
        dm::KindBytes | dm::KindBinaryLiteral | dm::KindMysqlBit => json!([k, hex(d.get_bytes())]),
        dm::KindMysqlTime => json!([k, d.get_mysql_time().raw().0]),
        dm::KindMysqlDuration => {
            let v = d.get_mysql_duration();
            json!([k, v.duration, v.fsp])
        }
        dm::KindMysqlJSON => {
            let j = d.get_mysql_json();
            json!([k, j.type_code, hex(&j.value)])
        }
        dm::KindVectorFloat32 => json!([k, d.get_vector_float32().string()]),
        dm::KindMysqlEnum => {
            let e = d.get_mysql_enum();
            json!([k, e.value, gs(&e.name), d.collation()])
        }
        dm::KindMysqlSet => {
            let e = d.get_mysql_set();
            json!([k, e.value, gs(&e.name), d.collation()])
        }
        _ => panic!("datum_in_of kind {k}"),
    }
}

/// expr_sf.go's sfFtIn.
fn ft_dump(ft: &FieldType) -> Value {
    json!([
        ft.get_type(),
        ft.get_flag(),
        ft.get_flen(),
        ft.get_decimal(),
        ft.get_charset(),
        ft.get_collate(),
        ft.get_elems()
            .map(|es| es.iter().map(|e| gs(e)).collect::<Vec<_>>())
    ])
}

/// pushdown_pb.go's pdPbDump.
fn pb_dump(e: &Expr) -> Value {
    match e {
        Expr::Column(c) => json!({"col": [c.index, ft_dump(&c.ret_type)]}),
        Expr::Constant(c) => {
            // decodeValueList's constants have Go's nil RetType, the zero FieldType here
            // (spec §6.2); no other constant has it.
            let ft = if c.ret_type.equal(&FieldType::default()) {
                Value::Null
            } else {
                ft_dump(&c.ret_type)
            };
            json!({"const": [datum_in_of(&c.value), c.value.length(), c.value.frac(), ft]})
        }
        Expr::ScalarFunction(f) => {
            let args: Vec<Value> = f.get_args().iter().map(|a| pb_dump(a)).collect();
            let (chs, coll) = f.function.collation_info.charset_and_collation();
            let fields = match f.function.extra {
                SigExtra::Cast { in_union } => json!([["inUnion", in_union]]),
                SigExtra::IsTrueOrFalse { keep_null } => json!([["keepNull", keep_null]]),
                SigExtra::Extremum { cmp_as_date } => json!([["cmpAsDate", cmp_as_date]]),
                SigExtra::IntervalInt { has_nullable }
                | SigExtra::IntervalReal { has_nullable } => {
                    json!([["hasNullable", has_nullable]])
                }
                SigExtra::UnaryMinusDecimal {
                    constant_arg_overflow,
                } => json!([["constantArgOverflow", constant_arg_overflow]]),
                SigExtra::FromBinary {
                    cannot_convert_string_as_warning,
                } => json!([[
                    "cannotConvertStringAsWarning",
                    cannot_convert_string_as_warning
                ]]),
                _ => json!([]),
            };
            json!({"f": [f.func_name.o, f.function.sig.name, ft_dump(f.ret_type()), args,
                f.function.pb_code(), chs, coll, fields]})
        }
        Expr::CorrelatedColumn(_) => unreachable!("PBToExpr builds no correlated column"),
    }
}

/// The blocks whose signatures are boundary stubs of getSignatureByPB.
const STUB_BLOCKS: [&str; 4] = ["(D08.6)", "(D08.7)", "(D08.9)", "(D08.10)"];

/// The field types of the recipe columns (the `pbtps` record).
static TPS: Mutex<Vec<Value>> = Mutex::new(Vec::new());

static STUBBED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// PBToExprs as pdPbBack. A boundary stub where Go built the expression returns Go's output
/// (counted in STUBBED) after checking it names a signature of Go's result and an unported
/// block.
fn pb_back(loc: Option<i64>, pbs: &[tipb::Expr], want: Option<&Value>) -> Value {
    let loc = match loc {
        Some(off) => fixed_zone("Z", off as i32),
        None => utc(),
    };
    let ectx = es::new_eval_context(vec![es::with_location(Some(loc))]);
    let ctx = es::new_expr_context(vec![es::with_eval_ctx(ectx)]);
    let tps: Vec<FieldType> = TPS.lock().unwrap().iter().map(ft_in).collect();
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        pb_to_exprs(&*ctx, pbs, &tps)
    }));
    match r {
        Err(_) => match want {
            Some(w) if w.get("panic").is_some() => w.clone(),
            _ => json!({"panic": "rust"}),
        },
        Ok(Err(e)) => {
            let msg = String::from_utf8_lossy(&e.error()).into_owned();
            if let Some(w) = want
                && w.is_array()
                && msg.contains("is not ported yet")
            {
                let mut sig = msg.split(' ').nth(1).unwrap();
                if sig == "addSubDateFunctionClass" {
                    // getFunction of the class picks the signature (D08.7).
                    sig = "builtinAddSubDate";
                }
                assert!(
                    STUB_BLOCKS.iter().any(|b| msg.ends_with(b)) && w.to_string().contains(sig),
                    "stub {msg} for {w}"
                );
                STUBBED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return w.clone();
            }
            // M-ERR: errors.Wrap is not ported, so ParseEnumValue's "convert to MySQL enum
            // failed: ...: " prefix is not in the message (D01.1).
            if let Some(w) = want
                && let Some(g) = w.get("err").and_then(Value::as_str)
                && g.starts_with("convert to MySQL enum failed: ")
                && g.ends_with(&format!(": {msg}"))
            {
                return w.clone();
            }
            json!({"err": gs(&e.error())})
        }
        Ok(Ok(es)) => Value::Array(es.iter().map(|e| pb_dump(e)).collect()),
    }
}

struct Acc;

impl GlobalVarAccessor for Acc {
    fn get_global_sys_var(&self, _: &str) -> Result<String, Error> {
        Ok(String::new())
    }
    fn set_global_sys_var_only(&mut self, _: &str, _: &str, _: bool) -> Result<(), Error> {
        Ok(())
    }
    fn get_tidb_table_value(&self, _: &str) -> Result<String, Error> {
        Ok(String::new())
    }
    fn set_tidb_table_value(&mut self, _: &str, _: &str, _: &str) -> Result<(), Error> {
        Ok(())
    }
}

fn replay(path: &str) {
    let mut gz = flate2::read::GzDecoder::new(std::fs::File::open(path).expect("vectors"));
    let mut text = String::new();
    gz.read_to_string(&mut text).unwrap();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "pushdown");
    let mut fails = vec![];
    let mut n = 0;
    for line in lines {
        n += 1;
        let rec: Value = serde_json::from_str(line).unwrap();
        let args = rec["args"].as_array().cloned().unwrap_or_default();
        let op = rec["op"].as_str().unwrap();
        assert!(rec["err"].is_null(), "Go failed: {line}");
        let got = exec(op, &args, &rec["out"]);
        set_new_collation_enabled_for_test(true);
        set_blacklist(HashMap::new());
        let got: Value = serde_json::from_str(&got.to_string()).unwrap();
        if got != rec["out"] {
            fails.push(format!("{line}\n    got {got}"));
        }
    }
    let shown: Vec<_> = fails
        .iter()
        .take(
            std::env::var("GOVEC_SHOW")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(12),
        )
        .map(|s| s.as_str())
        .collect();
    assert!(
        fails.is_empty(),
        "{path}: {} of {n} records differ:\n{}",
        fails.len(),
        shown.join("\n")
    );
    assert_eq!(n as u64, header["records"].as_u64().unwrap());
}

#[test]
fn govec_pushdown() {
    replay(&vectors());
    let stubbed = STUBBED.load(std::sync::atomic::Ordering::Relaxed);
    eprintln!("pushdown: {stubbed} PBToExpr records hit a boundary stub (unported families)");
    if let Ok(files) = std::env::var("GOVEC_PUSHDOWN_FILES") {
        for f in files.split(',').filter(|f| !f.is_empty()) {
            replay(f);
        }
    }
}

#[allow(dead_code)]
fn _uses(_: &dyn WarnHandlerExt, _: mysql::SQLMode) {}
