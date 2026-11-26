//! Replays the govec `aggdesc` vectors, parts A and C (tidb-rs-tracker `govec/domains/aggdesc.go`):
//! the aggregate and window function descriptors of block D08.11 (`pkg/expression/aggregation`)
//! and the `ByItems` they hold, against Go TiDB v8.5.8. The push-down and tipb records (part B
//! and part C `window_to_pb`/`window_tiflash`) are replayed by
//! `crates/expr-pushdown/tests/govec_aggdesc_pb.rs`.
//!
//! A record gives a descriptor spec (name, argument recipes, DISTINCT, mode, ORDER BY items,
//! or the raw fields); the replay builds the arguments with the real function families
//! (`new_function_base`, `build_cast_function`) as Go does, runs the same function and
//! compares the dump (arguments after the type inference's casts, return type, ...).

use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use serde_json::{Value, json};
use tidb_base::Error;
use tidb_expression::aggregation::{
    AggFuncDesc, AggFunctionMode, BaseFuncDesc, WindowFuncDesc, is_all_first_row, need_count,
    need_frame, need_value, new_agg_func_desc, new_agg_func_desc_for_window_func,
    new_window_func_desc, use_default_frame,
};
use tidb_expression::builtin_cast::build_cast_function;
use tidb_expression::byitem::{ByItems, stringify_by_items_with_ctx};
use tidb_expression::collation::Coercibility;
use tidb_expression::scalar_function::new_function_base;
use tidb_expression::schema::new_schema;
use tidb_expression::*;
use tidb_parser::types::FieldType;
use tidb_sessionctx::exprstatic as es;
use tidb_types::MyDecimal;
use tidb_types::collate::set_new_collation_enabled_for_test;
use tidb_types::datum::{self as dm, Datum};
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

/// Exact count of `new_window_desc` records covered by ruling R25 (#398): lowercase
/// `bit_and`/`bit_or`/`bit_xor` and 3-arg NOT NULL `lead`/`lag` preserve existing flags via
/// `AddFlag(NotNullFlag)` instead of Go's `SetFlag(NotNullFlag)`.
const EXPECTED_WINDOW_FLAG_RULED: usize = 45;

/// The ops replayed by tidb-expr-pushdown (parts B and C).
const PUSHDOWN_OPS: &[&str] = &[
    "push",
    "topb",
    "mode2pb",
    "pb2mode",
    "frompb",
    "tipb",
    "window_to_pb",
    "window_tiflash",
];

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

/// adWindow: a WindowFuncDesc with the descriptor's fields.
fn ad_window(d: &AggFuncDesc) -> WindowFuncDesc {
    WindowFuncDesc {
        base: d.base.clone(),
    }
}

fn ad_dump_window(w: &WindowFuncDesc) -> Value {
    let args: Vec<Value> = w.args.iter().map(|a| ad_arg(a)).collect();
    let rt = if w.ret_tp_is_nil() {
        Value::Null
    } else {
        ft_out(&w.ret_tp)
    };
    json!({"n": gs(w.name.as_bytes()), "a": args, "rt": rt})
}

fn frame_type_code(tp: tidb_parser::ast::FrameType) -> i32 {
    match tp {
        tidb_parser::ast::FrameType::Rows => 0,
        tidb_parser::ast::FrameType::Ranges => 1,
        tidb_parser::ast::FrameType::Groups => 2,
    }
}

fn bound_type_code(tp: tidb_parser::ast::BoundType) -> i32 {
    match tp {
        tidb_parser::ast::BoundType::Following => 0,
        tidb_parser::ast::BoundType::Preceding => 1,
        tidb_parser::ast::BoundType::CurrentRow => 2,
    }
}

/// Checks whether a `new_window_desc` difference against Go v8.5.8 is solely the intentional
/// flag preservation of ruling R25 (#398): lowercase `bit_and`/`bit_or`/`bit_xor` and 3-arg
/// NOT NULL `lead`/`lag` use `AddFlag(NotNullFlag)` instead of Go's `SetFlag(NotNullFlag)`.
fn is_window_flag_ruling_diff(rec: &Value, got: &Value) -> bool {
    use tidb_base::mysql::{BinaryFlag, NotNullFlag, UnsignedFlag};
    if rec["op"] != "new_window_desc" {
        return false;
    }
    let spec = &rec["args"][0];
    let Some(name) = spec["n"].as_str() else {
        return false;
    };
    let is_bit = matches!(name, "bit_and" | "bit_or" | "bit_xor");
    let is_lead_lag3 =
        matches!(name, "lead" | "lag") && spec["a"].as_array().is_some_and(|a| a.len() == 3);
    if !is_bit && !is_lead_lag3 {
        return false;
    }
    let want = &rec["out"];
    let Some(want_flag) = want["w"]["rt"][1].as_u64() else {
        return false;
    };
    let Some(got_flag) = got["w"]["rt"][1].as_u64() else {
        return false;
    };
    if want_flag != NotNullFlag as u64 {
        return false;
    }
    if is_bit && got_flag != (NotNullFlag | UnsignedFlag | BinaryFlag) as u64 {
        return false;
    }
    if is_lead_lag3 && ((got_flag as usize & NotNullFlag) == 0 || got_flag == want_flag) {
        return false;
    }
    if got["clone"]["rt"][1].as_u64() != Some(got_flag) {
        return false;
    }
    let mut patched = want.clone();
    patched["w"]["rt"][1] = json!(got_flag);
    patched["clone"]["rt"][1] = json!(got_flag);
    got == &patched
}

// ---- the domain ------------------------------------------------------------------------------

fn exec(op: &str, p: &[Value]) -> Value {
    match op {
        "new" | "wrap" | "notnull" | "default" | "outer" | "split" | "string" | "mem" => {
            let s = &p[0];
            let ctx = ad_ctx(s);
            let ectx = ctx.get_eval_ctx();
            let mut d = match ad_desc(&*ctx, s) {
                Ok(d) => d,
                Err(e) => return ex_err(&e),
            };
            match op {
                "new" => {
                    let mut m = ad_dump(&d);
                    m["s"] = gs(&d.string_with_ctx(&*ectx, tidb_base::errors::RedactLogDisable));
                    m
                }
                "wrap" => match d.wrap_cast_for_agg_args(&*ctx) {
                    Ok(()) => ad_dump(&d),
                    Err(e) => json!({ "rust_err": gs(&e.error()) }),
                },
                "notnull" => match d.update_not_null_flag4_ret_type(
                    p[1].as_bool().unwrap(),
                    p[2].as_bool().unwrap(),
                ) {
                    Ok(()) => ad_dump(&d),
                    Err(e) => ex_err(&e),
                },
                "default" => datum_out(&d.get_default_value()),
                "outer" => {
                    let cols = p[1]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|u| {
                            Arc::new(Column {
                                unique_id: u.as_i64().unwrap(),
                                ..Column::default()
                            })
                        })
                        .collect();
                    match d.eval_null_value_in_outer_join(&*ctx, &new_schema(cols)) {
                        Ok((v, ok)) => json!([datum_out(&v), ok]),
                        Err(e) => ex_err(&e),
                    }
                }
                "split" => {
                    let ord: Vec<isize> = p[1]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|x| x.as_i64().unwrap() as isize)
                        .collect();
                    let (partial, final_) = d.split(&ord);
                    json!([ad_dump(&d), ad_dump(&partial), ad_dump(&final_)])
                }
                "string" => {
                    let redact = p[1].as_str().unwrap();
                    let items: Vec<Value> = d
                        .order_by_items
                        .iter()
                        .map(|it| gs(&it.string_with_ctx(&*ectx, redact)))
                        .collect();
                    json!({
                        "agg": gs(&d.string_with_ctx(&*ectx, redact)),
                        "base": gs(&ad_window(&d).string_with_ctx(&*ectx, redact)),
                        "items": items,
                        "all": gs(&stringify_by_items_with_ctx(&*ectx, &d.order_by_items)),
                    })
                }
                "mem" => {
                    let items: Vec<i64> = d
                        .order_by_items
                        .iter()
                        .map(|it| it.memory_usage())
                        .collect();
                    json!([d.memory_usage(), ad_window(&d).memory_usage(), items])
                }
                _ => unreachable!(),
            }
        }
        "avgsum" => {
            let mut d = AggFuncDesc::from_base(
                BaseFuncDesc::from_parts(String::new(), vec![], Some(Arc::new(ft_in(&p[0])))),
                false,
            );
            d.type_infer4_avg_sum(&ft_in(&p[1]));
            ft_out(&d.ret_tp)
        }
        "finalcount" => {
            let mut d = AggFuncDesc::from_base(
                BaseFuncDesc::from_parts(String::new(), vec![], None),
                false,
            );
            d.type_infer4_final_count(&ft_in(&p[0]));
            ft_out(&d.ret_tp)
        }
        "equal" => {
            let ctx = ad_ctx(&p[0]);
            let ectx = ctx.get_eval_ctx();
            let a = match ad_desc(&*ctx, &p[0]) {
                Ok(d) => d,
                Err(e) => return ex_err(&e),
            };
            let b = match ad_desc(&*ctx, &p[1]) {
                Ok(d) => d,
                Err(e) => return ex_err(&e),
            };
            let c = a.clone_desc();
            let items: Vec<bool> = a
                .order_by_items
                .iter()
                .zip(&b.order_by_items)
                .map(|(x, y)| x.equal(&*ectx, y))
                .collect();
            json!({"ab": a.equal(&*ectx, &b), "ba": b.equal(&*ectx, &a), "ca": c.equal(&*ectx, &a),
                "clone": ad_dump(&c), "items": items})
        }
        "names" => {
            let n = p[0].as_str().unwrap();
            json!([need_count(n), need_value(n)])
        }
        "mode" => json!(mode_of(p[0].as_i64().unwrap()).to_string()),
        "allfirst" => {
            let ds: Vec<AggFuncDesc> = p[0]
                .as_array()
                .unwrap()
                .iter()
                .map(|n| {
                    AggFuncDesc::from_base(
                        BaseFuncDesc::from_parts(n.as_str().unwrap().to_owned(), vec![], None),
                        false,
                    )
                })
                .collect();
            json!(is_all_first_row(&ds))
        }
        "window" => {
            let s = &p[0];
            let ctx = ad_ctx(s);
            set_new_collation_enabled_for_test(s["nc"].as_bool().unwrap());
            let w = WindowFuncDesc {
                base: BaseFuncDesc::from_parts(
                    s["n"].as_str().unwrap().to_owned(),
                    ad_args(&*ctx, s),
                    rt_of(s),
                ),
            };
            match new_agg_func_desc_for_window_func(&*ctx, &w, s["d"].as_bool().unwrap()) {
                Ok(d) => ad_dump(&d),
                Err(e) => ex_err(&e),
            }
        }
        "new_window_desc" => {
            let s = &p[0];
            let ctx = ad_ctx(s);
            let ectx = ctx.get_eval_ctx();
            set_new_collation_enabled_for_test(s["nc"].as_bool().unwrap());
            let args = ad_args(&*ctx, s);
            let skip = p[1].as_bool().unwrap();
            match new_window_func_desc(&*ctx, s["n"].as_str().unwrap(), args, skip) {
                Err(e) => ex_err(&e),
                Ok(None) => Value::Null,
                Ok(Some(w)) => {
                    let c = w.clone_desc();
                    json!({
                        "w": ad_dump_window(&w),
                        "clone": ad_dump_window(&c),
                        "s": gs(&w.string_with_ctx(&*ectx, tidb_base::errors::RedactLogDisable)),
                    })
                }
            }
        }
        "window_frame" => {
            let n = p[0].as_str().unwrap();
            let (use_def, frame) = use_default_frame(n);
            json!({
                "use": use_def,
                "tp": frame_type_code(frame.tp),
                "start": [bound_type_code(frame.extent.start.tp), frame.extent.start.unbounded],
                "end": [bound_type_code(frame.extent.end.tp), frame.extent.end.unbounded],
                "need": need_frame(n),
            })
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
    let (mut n, mut replayed, mut ruled) = (0, 0, 0);
    for line in lines {
        n += 1;
        let rec: Value = serde_json::from_str(line).unwrap();
        let op = rec["op"].as_str().unwrap();
        if PUSHDOWN_OPS.contains(&op) {
            continue;
        }
        replayed += 1;
        let args = rec["args"].as_array().cloned().unwrap_or_default();
        let go_panicked = rec["err"]["name"] == "panic";
        let got = catch_unwind(AssertUnwindSafe(|| exec(op, &args)));
        let ok = match &got {
            Err(_) => go_panicked,
            Ok(v) => {
                let v: Value = serde_json::from_str(&v.to_string()).unwrap();
                if !go_panicked && v == rec["out"] {
                    true
                } else if !go_panicked && is_window_flag_ruling_diff(&rec, &v) {
                    ruled += 1;
                    true
                } else {
                    false
                }
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
    assert!(replayed > 5000, "{replayed} part A+C records");
    assert_eq!(
        ruled, EXPECTED_WINDOW_FLAG_RULED,
        "{path}: ruled window flag records"
    );
    eprintln!("{path}: {replayed} records replayed, {ruled} ruled (R25)");
}

#[test]
fn govec_aggdesc() {
    replay(&vectors());
}
