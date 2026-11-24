//! Replays the govec `aggdesc` vectors, part B (tidb-rs-tracker `govec/domains/aggdesc.go`):
//! the push-down decisions and the tipb conversions of the aggregate function descriptors
//! (block D08.11, `agg_to_pb.go` and the push-down checks of `aggregation.go`), against Go TiDB
//! v8.5.8. The part A records are replayed by `crates/expression/tests/govec_aggdesc.rs`; the
//! vector file is the same (a copy, see the vectors README), and so are the spec helpers.
//!
//! The test changes the global push-down blacklist, so it is the only test of this binary.

use std::collections::HashMap;
use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use protobuf::Message;
use serde_json::{Value, json};
use tidb_base::Error;
use tidb_base::kv::{StoreType, TiDB, TiFlash, TiKV, UnSpecified};
use tidb_expr_pushdown::aggregation::{
    agg_func_to_pb_expr, agg_function_mode_to_pb, check_agg_push_down, check_agg_push_flash,
    get_tipb_expr, pb_agg_func_mode_to_agg_func_mode, pb_expr_to_agg_func_desc,
};
use tidb_expr_pushdown::{DefaultExprPushDownBlacklist, new_push_down_context};
use tidb_expression::aggregation::{AggFuncDesc, AggFunctionMode, BaseFuncDesc, new_agg_func_desc};
use tidb_expression::builtin_cast::build_cast_function;
use tidb_expression::byitem::ByItems;
use tidb_expression::collation::Coercibility;
use tidb_expression::scalar_function::new_function_base;
use tidb_expression::*;
use tidb_kv::checker::RequestTypeSupportedChecker;
use tidb_kv::kv::{Client, ClientSendOption, ReqTypeDAG, Request, Response};
use tidb_sessionctx::exprstatic as es;
use tidb_types::MyDecimal;
use tidb_types::collate::set_new_collation_enabled_for_test;
use tidb_types::datum::{self as dm, Datum};
use tidb_types::etc::FieldType;
use tidb_types::field_type::new_field_type;
use tidb_types::field_type_builder::new_field_type_builder;

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

fn vectors() -> String {
    format!(
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/aggdesc.jsonl.gz",
        env!("CARGO_MANIFEST_DIR")
    )
}

/// The ops of part B.
const PART_B: &[&str] = &["push", "topb", "mode2pb", "pb2mode", "frompb", "tipb"];

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

/// d2DatumIn's kinds the recipes use.
fn datum_in(v: &Value) -> Datum {
    let p = v.as_array().unwrap();
    let k = p[0].as_u64().unwrap() as u8;
    let mut d = Datum::default();
    match k {
        dm::KindNull => d.set_null(),
        dm::KindInt64 => d.set_int64(p[1].as_i64().unwrap()),
        dm::KindUint64 => d.set_uint64(p[1].as_u64().unwrap()),
        dm::KindFloat64 => d.set_float64(f64::from_bits(p[1].as_u64().unwrap())),
        dm::KindString => d.set_string(ungs(&p[1]), p[2].as_str().unwrap().to_owned()),
        dm::KindMysqlDecimal => {
            let mut x = MyDecimal::default();
            let _ = x.from_string(p[1].as_str().unwrap().as_bytes());
            d.set_mysql_decimal(x)
        }
        dm::KindBytes => d.set_bytes(Some(unhex(p[1].as_str().unwrap()))),
        _ => panic!("datum_in: kind {k}"),
    }
    d
}

/// d2DatumInOf.
fn datum_out(d: &Datum) -> Value {
    match d.kind() {
        dm::KindNull => json!([d.kind()]),
        dm::KindInt64 => json!([d.kind(), d.get_int64()]),
        dm::KindUint64 => json!([d.kind(), d.get_uint64()]),
        dm::KindFloat64 => json!([d.kind(), d.get_float64().to_bits()]),
        dm::KindString => json!([d.kind(), gs(d.get_string()), d.collation()]),
        dm::KindMysqlDecimal => json!([d.kind(), d.get_mysql_decimal().string()]),
        dm::KindBytes => json!([d.kind(), hex(d.get_bytes())]),
        dm::KindMysqlTime => json!([d.kind(), d.get_mysql_time().raw().0]),
        dm::KindMysqlDuration => {
            let x = d.get_mysql_duration();
            json!([d.kind(), x.duration, x.fsp])
        }
        dm::KindMysqlJSON => {
            let j = d.get_mysql_json();
            json!([d.kind(), j.type_code, hex(&j.value)])
        }
        dm::KindVectorFloat32 => json!([d.kind(), d.get_vector_float32().string()]),
        dm::KindFloat32 => json!([d.kind(), f64::from(d.get_float32()).to_bits()]),
        dm::KindBinaryLiteral | dm::KindMysqlBit => json!([d.kind(), hex(d.get_bytes())]),
        dm::KindMysqlEnum => {
            let e = d.get_mysql_enum();
            json!([d.kind(), e.value, gs(&e.name), d.collation()])
        }
        dm::KindMysqlSet => {
            let st = d.get_mysql_set();
            json!([d.kind(), st.value, gs(&st.name), d.collation()])
        }
        k => panic!("datum_out: kind {k}"),
    }
}

/// d2FtIn: [tp, flag, flen, decimal, charset, collate, elems|null].
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

/// sfFtIn.
fn ft_out(ft: &FieldType) -> Value {
    let elems = ft
        .get_elems()
        .map(|es| Value::Array(es.iter().map(|e| gs(e)).collect()))
        .unwrap_or(Value::Null);
    json!([
        ft.get_type(),
        ft.get_flag(),
        ft.get_flen(),
        ft.get_decimal(),
        ft.get_charset(),
        ft.get_collate(),
        elems
    ])
}

fn ex_try(f: impl FnOnce() -> Value) -> Value {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| json!({ "panic": true }))
}

fn ex_err(e: &Error) -> Value {
    json!({ "err": gs(&e.error()) })
}

// ---- specs -----------------------------------------------------------------------------------

/// adCtx: exprstatic's context with the current time fixed and the record's
/// div_precision_increment.
fn ad_ctx(s: &Value) -> Arc<es::ExprContext> {
    let mut opts = vec![es::with_current_time(Arc::new(|| {
        Ok(tidb_base::gostd::time::unix(1767225600, 0))
    }))];
    let dpi = s["dpi"].as_i64().unwrap_or(0);
    if dpi > 0 {
        opts.push(es::with_div_precision_increment(dpi));
    }
    es::new_expr_context(vec![es::with_eval_ctx(es::new_eval_context(opts))])
}

/// adBuild: a recipe built as Go builds it (Go panics on a build error, as here).
fn ad_build(ctx: &dyn BuildContext, v: &Value) -> ExprRef {
    let o = v.as_object().unwrap();
    if let Some(a) = o.get("col") {
        let a = a.as_array().unwrap();
        let c = Column {
            unique_id: a[0].as_i64().unwrap(),
            id: a[0].as_i64().unwrap(),
            index: a[1].as_i64().unwrap() as isize,
            ret_type: Arc::new(ft_in(&a[2])),
            orig_name: String::from_utf8(ungs(&a[3])).unwrap(),
            ..Column::default()
        };
        if let Some(n) = o.get("coer") {
            c.collation_info
                .set_coercibility(Coercibility(n.as_i64().unwrap() as _));
        }
        return Expr::Column(c).into_ref();
    }
    if let Some(a) = o.get("const") {
        let a = a.as_array().unwrap();
        return Expr::Constant(Constant::new(datum_in(&a[0]), ft_in(&a[1]))).into_ref();
    }
    if let Some(a) = o.get("castf") {
        let a = a.as_array().unwrap();
        let arg = ad_build(ctx, &a[1]);
        return build_cast_function(ctx, arg, ft_in(&a[0])).unwrap();
    }
    let a = o["f"].as_array().unwrap();
    let rt = if a[1].is_null() {
        new_field_type(tidb_base::mysql::TypeUnspecified)
    } else {
        ft_in(&a[1])
    };
    let args = a[2]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| ad_build(ctx, x))
        .collect();
    new_function_base(ctx, a[0].as_str().unwrap(), rt, args).unwrap()
}

fn ad_args(ctx: &dyn BuildContext, s: &Value) -> Vec<ExprRef> {
    s["a"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| ad_build(ctx, r))
        .collect()
}

fn mode_of(m: i64) -> AggFunctionMode {
    match m {
        0 => AggFunctionMode::CompleteMode,
        1 => AggFunctionMode::FinalMode,
        2 => AggFunctionMode::Partial1Mode,
        3 => AggFunctionMode::Partial2Mode,
        4 => AggFunctionMode::DedupMode,
        _ => panic!("mode {m}"),
    }
}

fn rt_of(s: &Value) -> Option<Arc<FieldType>> {
    (!s["rt"].is_null()).then(|| Arc::new(ft_in(&s["rt"])))
}

/// adDesc.
fn ad_desc(ctx: &dyn BuildContext, s: &Value) -> Result<AggFuncDesc, Error> {
    set_new_collation_enabled_for_test(s["nc"].as_bool().unwrap());
    let args = ad_args(ctx, s);
    let name = s["n"].as_str().unwrap();
    let d = s["d"].as_bool().unwrap();
    let mut desc = if s["raw"].as_bool().unwrap() {
        AggFuncDesc::from_base(BaseFuncDesc::from_parts(name.to_owned(), args, rt_of(s)), d)
    } else {
        new_agg_func_desc(ctx, name, args, d)?
    };
    desc.mode = mode_of(s["m"].as_i64().unwrap());
    for it in s["ob"].as_array().unwrap() {
        let p = it.as_array().unwrap();
        desc.order_by_items.push(ByItems {
            expr: ad_build(ctx, &p[0]),
            desc: p[1].as_bool().unwrap(),
        });
    }
    Ok(desc)
}

fn ad_arg(e: &Expr) -> Value {
    match e {
        Expr::Column(c) => json!(["col", c.unique_id, c.index, ft_out(&c.ret_type)]),
        Expr::Constant(c) => json!(["const", datum_out(&c.value), ft_out(&c.ret_type)]),
        Expr::ScalarFunction(f) => {
            let args: Vec<Value> = f.get_args().iter().map(|a| ad_arg(a)).collect();
            json!(["f", f.func_name.l, ft_out(f.ret_type()), args])
        }
        Expr::CorrelatedColumn(_) => panic!("ad_arg: correlated column"),
    }
}

fn ad_dump(d: &AggFuncDesc) -> Value {
    let args: Vec<Value> = d.args.iter().map(|a| ad_arg(a)).collect();
    let ob: Vec<Value> = d
        .order_by_items
        .iter()
        .map(|it| json!([ad_arg(&it.expr), it.desc]))
        .collect();
    let rt = if d.ret_tp_is_nil() {
        Value::Null
    } else {
        ft_out(&d.ret_tp)
    };
    json!({"n": gs(d.name.as_bytes()), "a": args, "rt": rt, "m": d.mode as isize,
        "d": d.has_distinct, "ob": ob, "g": d.grouping_id})
}

// ---- the domain ------------------------------------------------------------------------------

/// pushdown.go's pdClient: mock.Client (kv.RequestTypeSupportedChecker), or a variant refusing
/// ReqTypeDAG (1) or everything (2).
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

/// pdBlacklist.
fn set_blacklist(v: &Value) {
    let mut m = HashMap::new();
    for e in v.as_array().map(|a| a.as_slice()).unwrap_or_default() {
        let e = e.as_array().unwrap();
        m.insert(
            e[0].as_str().unwrap().to_owned(),
            e[1].as_u64().unwrap() as u32,
        );
    }
    DefaultExprPushDownBlacklist.store(Arc::new(m));
}

fn exec(op: &str, p: &[Value]) -> Value {
    match op {
        "push" | "topb" => {
            let s = &p[0];
            let ctx = ad_ctx(s);
            let ectx = ctx.get_eval_ctx();
            let d = match ad_desc(&*ctx, s) {
                Ok(d) => d,
                Err(e) => return ex_err(&e),
            };
            if op == "push" {
                set_blacklist(&p[1]);
                let push: Vec<Value> = [TiKV, TiFlash, TiDB, UnSpecified]
                    .iter()
                    .map(|st| ex_try(|| json!(check_agg_push_down(&*ectx, &d, *st))))
                    .collect();
                return json!({"push": push, "flash": ex_try(|| json!(check_agg_push_flash(&*ectx, &d)))});
            }
            let client: Arc<dyn Client> = Arc::new(PdClient(p[1].as_u64().unwrap()));
            let pctx = new_push_down_context(
                Arc::clone(&ectx),
                client,
                false,
                None,
                None,
                p[2].as_u64().unwrap(),
            );
            match agg_func_to_pb_expr(&pctx, &d, StoreType(p[3].as_u64().unwrap() as u8)) {
                Ok(pb) => json!({"pb": hex(&pb.write_to_bytes().unwrap())}),
                Err(e) => ex_err(&e),
            }
        }
        "mode2pb" => json!(agg_function_mode_to_pb(mode_of(p[0].as_i64().unwrap())) as i32),
        "pb2mode" => {
            let m = (!p[0].is_null()).then(|| {
                <tipb::AggFunctionMode as protobuf::ProtobufEnum>::from_i32(
                    p[0].as_i64().unwrap() as i32
                )
            });
            // A value the enum does not know is not kept in the field (Go's switch keeps the
            // default for it too).
            json!(pb_agg_func_mode_to_agg_func_mode(m.flatten()) as isize)
        }
        "frompb" => {
            let pb: tipb::Expr =
                protobuf::parse_from_bytes(&unhex(p[0].as_str().unwrap())).unwrap();
            let fts: Vec<FieldType> = p[1].as_array().unwrap().iter().map(ft_in).collect();
            set_new_collation_enabled_for_test(p[2].as_bool().unwrap());
            let ctx = ad_ctx(&json!({}));
            match pb_expr_to_agg_func_desc(&*ctx, &pb, &fts) {
                Ok(d) => ad_dump(&d),
                Err(e) => ex_err(&e),
            }
        }
        "tipb" => {
            let d = BaseFuncDesc::from_parts(p[0].as_str().unwrap().to_owned(), vec![], None);
            json!(get_tipb_expr(&d, p[1].as_bool().unwrap()) as i32)
        }
        _ => panic!("aggdesc: unknown op {op}"),
    }
}

fn replay(path: &str) {
    let mut gz = flate2::read::GzDecoder::new(std::fs::File::open(path).expect("vectors"));
    let mut text = String::new();
    gz.read_to_string(&mut text).unwrap();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "aggdesc");
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let mut fails = vec![];
    let (mut n, mut replayed) = (0, 0);
    for line in lines {
        n += 1;
        let rec: Value = serde_json::from_str(line).unwrap();
        let op = rec["op"].as_str().unwrap();
        if !PART_B.contains(&op) {
            continue;
        }
        replayed += 1;
        let args = rec["args"].as_array().cloned().unwrap_or_default();
        let go_panicked = rec["err"]["name"] == "panic";
        let got = catch_unwind(AssertUnwindSafe(|| exec(op, &args)));
        DefaultExprPushDownBlacklist.store(Arc::new(HashMap::new()));
        let ok = match &got {
            Err(_) => go_panicked,
            Ok(v) => {
                let v: Value = serde_json::from_str(&v.to_string()).unwrap();
                !go_panicked && v == rec["out"]
            }
        };
        if !ok {
            let got = match got {
                Ok(v) => v.to_string(),
                Err(_) => "panic".to_owned(),
            };
            fails.push(format!("{line}\n    got {got}"));
        }
    }
    set_new_collation_enabled_for_test(true);
    std::panic::set_hook(prev);
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
        "{path}: {} of {replayed} records differ:\n{}",
        fails.len(),
        shown.join("\n")
    );
    assert_eq!(n as u64, header["records"].as_u64().unwrap());
    assert!(replayed > 1000, "{replayed} part B records");
}

#[test]
fn govec_aggdesc_pb() {
    replay(&vectors());
}
