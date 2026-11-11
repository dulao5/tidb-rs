//! Replays the govec `cast` vectors (tidb-rs-tracker `govec/domains/cast.go`): block D08.8 —
//! CAST to INT/REAL/DECIMAL/STRING/DATETIME/DATE/TIMESTAMP/TIME built with `BuildCastFunctionWithCheck` (folded on a
//! constant argument) and evaluated row by row and over a chunk (the vector driver),
//! `WrapWithCastAsInt/Real/Decimal/String/Time/Duration`, `HandleBinaryLiteral` (to_binary/from_binary)
//! and `BuildCastCollationFunction` — against Go TiDB v8.5.8.
//!
//! `GOVEC_CAST_FILES` (comma-separated paths) replays extra-seed artifacts as well.

use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use serde_json::{Value, json};
use tidb_base::Error;
use tidb_base::contextutil::{SQLWarn, WarnHandler, new_static_warn_handler};
use tidb_base::mysql::SQLMode;
use tidb_chunk::chunk::new_chunk_with_capacity;
use tidb_expression::builtin_cast::{
    build_cast_collation_function, build_cast_function_with_check, wrap_with_cast_as_decimal,
    wrap_with_cast_as_duration, wrap_with_cast_as_int, wrap_with_cast_as_real,
    wrap_with_cast_as_string, wrap_with_cast_as_time,
};
use tidb_expression::builtin_convert_charset::handle_binary_literal;
use tidb_expression::eval::{RpnProgram, RpnState, rt};
use tidb_expression::*;
use tidb_parser::types::{
    ETDatetime, ETDecimal, ETDuration, ETInt, ETReal, ETString, ETTimestamp, FieldType,
};
use tidb_sessionctx::errctx::{Level, LevelMap};
use tidb_sessionctx::exprstatic as es;
use tidb_types::MyDecimal;
use tidb_types::binary_literal::BinaryLiteral;
use tidb_types::context::Flags;
use tidb_types::datum::{self as dm, Datum};
use tidb_types::enum_type::Enum;
use tidb_types::field_type_builder::new_field_type_builder;
use tidb_types::set_type::Set;

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

fn vectors() -> String {
    format!(
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/cast.jsonl.gz",
        env!("CARGO_MANIFEST_DIR")
    )
}

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

/// d2DatumIn (the kinds the records use).
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
        dm::KindFloat32 => d.set_float32(f64::from_bits(p[1].as_u64().unwrap()) as f32),
        dm::KindMysqlBit => d.set_mysql_bit(BinaryLiteral(unhex(p[1].as_str().unwrap()))),
        dm::KindMysqlEnum => d.set_mysql_enum(
            Enum {
                value: p[1].as_u64().unwrap(),
                name: ungs(&p[2]),
            },
            p[3].as_str().unwrap().to_owned(),
        ),
        dm::KindMysqlSet => d.set_mysql_set(
            Set {
                value: p[1].as_u64().unwrap(),
                name: ungs(&p[2]),
            },
            p[3].as_str().unwrap().to_owned(),
        ),
        dm::KindMysqlTime => d.set_mysql_time(tidb_types::time::Time::from_raw(
            tidb_types::core_time::CoreTime(p[1].as_u64().unwrap()),
        )),
        dm::KindMysqlDuration => d.set_mysql_duration(tidb_types::time::Duration {
            duration: p[1].as_i64().unwrap(),
            fsp: p[2].as_i64().unwrap() as isize,
        }),
        dm::KindMysqlJSON => d.set_mysql_json(tidb_types::json::BinaryJSON {
            type_code: p[1].as_u64().unwrap() as u8,
            value: std::borrow::Cow::Owned(unhex(p[2].as_str().unwrap())),
        }),
        dm::KindVectorFloat32 => d.set_vector_float32(
            tidb_types::vector::parse_vector_float32(p[1].as_str().unwrap().as_bytes()).unwrap(),
        ),
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

/// sfDump.
fn dump(e: &Expr) -> Value {
    match e {
        Expr::Column(c) => {
            let ve = c.virtual_expr.as_ref().map_or(Value::Null, |v| dump(v));
            json!({ "col": [c.unique_id, c.index, ft_out(&c.ret_type), gs(c.orig_name.as_bytes()), ve] })
        }
        Expr::CorrelatedColumn(c) => {
            json!({ "corr": [c.column.unique_id, ft_out(&c.column.ret_type)] })
        }
        Expr::Constant(c) => json!({ "const": [datum_out(&c.value), ft_out(&c.ret_type)] }),
        Expr::ScalarFunction(x) => {
            let args: Vec<Value> = x.get_args().iter().map(|a| dump(a)).collect();
            let all_vec = x.get_args().iter().all(|a| a.vectorized());
            let sig_vec = if all_vec { x.vectorized() } else { true };
            let (chs, coll) = x.charset_and_collation();
            let ci = &x.function.collation_info;
            json!({ "f": [
                x.func_name.l, x.func_name.o, x.function.sig.name, ft_out(&x.ret_type), args,
                sig_vec, chs, coll, x.function.pb_code(), ci.has_coercibility(),
                x.coercibility().0, x.repertoire().0, ci.is_explicit_charset()
            ] })
        }
    }
}

fn dec_out(d: &MyDecimal) -> Value {
    let j: Value = serde_json::from_slice(&d.marshal_json().unwrap()).unwrap();
    let mut st = vec![
        j["DigitsInt"].clone(),
        j["DigitsFrac"].clone(),
        j["ResultFrac"].clone(),
        json!(i64::from(j["Negative"].as_bool().unwrap())),
    ];
    st.extend(j["WordBuf"].as_array().unwrap().iter().cloned());
    json!({ "s": String::from_utf8(d.to_string()).unwrap(), "str": d.string(), "st": st })
}

/// castTimeOut: [raw (the packed CoreTime), String()].
fn time_out(t: &tidb_types::time::Time) -> Value {
    json!([t.raw().0.to_string(), gs(&t.string())])
}

fn warns(ws: &[SQLWarn]) -> Value {
    Value::Array(
        ws.iter()
            .map(|w| json!([&*w.level, gs(&w.err.error())]))
            .collect(),
    )
}

fn ex_try(f: impl FnOnce() -> Value) -> Value {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| json!({ "panic": true }))
}

fn ex_err(e: &Error) -> Value {
    json!({ "err": gs(&e.error()) })
}

fn ex_val<T>(r: Result<Option<T>, Error>, f: impl FnOnce(T) -> Value) -> Value {
    match r {
        Err(e) => ex_err(&e),
        Ok(None) => json!({ "null": true }),
        Ok(Some(v)) => json!({ "v": f(v) }),
    }
}

type Ctxs = (Arc<es::ExprContext>, Arc<es::EvalContext>);

/// castCtx: [typeFlags, [7]errLevels, maxAllowedPacket?, sqlMode?].
fn ctx_of(v: &Value) -> Ctxs {
    let a = v.as_array().unwrap();
    let mut lm: LevelMap = Default::default();
    for (i, l) in a[1].as_array().unwrap().iter().enumerate() {
        lm[i] = Level(l.as_u64().unwrap() as _);
    }
    let mut opts = vec![
        es::with_type_flags(Flags(a[0].as_u64().unwrap() as u16)),
        es::with_err_level_map(lm),
        es::with_warn_handler(Some(Arc::new(new_static_warn_handler(0)))),
    ];
    if a.len() > 2 {
        opts.push(es::with_max_allowed_packet(a[2].as_u64().unwrap()));
        opts.push(es::with_sql_mode(SQLMode(a[3].as_u64().unwrap() as i64)));
    }
    if a.len() > 4 {
        // The statement time (unix nanoseconds, UTC).
        let ns = a[4].as_i64().unwrap();
        opts.push(es::with_current_time(Arc::new(move || {
            Ok(tidb_base::gostd::time::unix(0, ns))
        })));
    }
    let ev = es::new_eval_context(opts);
    let ctx = es::new_expr_context(vec![es::with_eval_ctx(Arc::clone(&ev))]);
    (ctx, ev)
}

/// castArg: the node and, for a column, the chunk of the rows.
fn arg_of(v: &Value, rows: Option<&Value>) -> (ExprRef, Option<tidb_chunk::Chunk>) {
    let a = v.as_array().unwrap();
    match a[0].as_str().unwrap() {
        "col" => {
            let ft = ft_in(&a[1]);
            let rows = rows.and_then(|r| r.as_array()).cloned().unwrap_or_default();
            let mut chk = new_chunk_with_capacity(std::slice::from_ref(&ft), rows.len());
            for r in &rows {
                chk.append_datum(0, &datum_in(r));
            }
            let col = Column {
                unique_id: 1,
                index: 0,
                ret_type: ft,
                orig_name: "c".into(),
                ..Column::default()
            };
            (Expr::Column(col).into_ref(), Some(chk))
        }
        "const" => (
            Expr::Constant(Constant::new(datum_in(&a[1]), ft_in(&a[2]))).into_ref(),
            None,
        ),
        k => panic!("arg_of: {k}"),
    }
}

fn row_val(ctx: &dyn EvalContext, e: &Expr, row: tidb_chunk::Row<'_>) -> Value {
    let st = EvalState::new();
    let et = e.get_type(ctx).eval_type();
    if et == ETInt {
        ex_val(e.eval_int(ctx, &st, row), |v| json!(v.to_string()))
    } else if et == ETReal {
        ex_val(e.eval_real(ctx, &st, row), |v| {
            json!(v.to_bits().to_string())
        })
    } else if et == ETDecimal {
        ex_val(e.eval_decimal(ctx, &st, row), |v| dec_out(&v))
    } else if et == ETString {
        ex_val(e.eval_string(ctx, &st, row), |v| gs(&v))
    } else if et == ETDatetime || et == ETTimestamp {
        ex_val(e.eval_time(ctx, &st, row), |v| time_out(&v))
    } else if et == ETDuration {
        ex_val(e.eval_duration(ctx, &st, row), |v| {
            json!([v.duration, v.fsp])
        })
    } else {
        panic!("row_val")
    }
}

fn vec_val(ctx: &es::EvalContext, e: &ExprRef, chk: &tidb_chunk::Chunk) -> Value {
    let et = e.get_type(ctx).eval_type();
    let prog = RpnProgram::compile(e);
    let mut col = rt::empty_column();
    let r = prog.eval(
        ctx,
        &EvalState::new(),
        &mut RpnState::default(),
        chk,
        &mut col,
    );
    let ws = warns(&ctx.truncate_warnings(0));
    if let Err(e) = r {
        return json!({ "err": gs(&e.error()), "warns": ws });
    }
    let vals: Vec<Value> = (0..chk.num_rows())
        .map(|i| {
            if col.is_null(i) {
                Value::Null
            } else if et == ETInt {
                json!(col.get_int64(i).to_string())
            } else if et == ETReal {
                json!(col.get_float64(i).to_bits().to_string())
            } else if et == ETString {
                gs(col.get_bytes(i))
            } else if et == ETDatetime || et == ETTimestamp {
                time_out(&col.get_time(i))
            } else if et == ETDuration {
                // The column keeps the nanoseconds only.
                json!(col.get_duration(i, 0).duration)
            } else {
                dec_out(&col.get_decimal(i))
            }
        })
        .collect();
    json!({ "v": vals, "warns": ws })
}

/// castEvalRows: the row-mode values and warnings per row, then the vector-mode values.
fn eval_rows(
    m: &mut serde_json::Map<String, Value>,
    ev: &es::EvalContext,
    res: &ExprRef,
    chk: &tidb_chunk::Chunk,
) {
    let mut rows = Vec::new();
    for i in 0..chk.num_rows() {
        let v = ex_try(|| row_val(ev, res, chk.get_row(i)));
        rows.push(json!([v, warns(&ev.truncate_warnings(0))]));
    }
    m.insert("rows".into(), Value::Array(rows));
    m.insert("vec".into(), ex_try(|| vec_val(ev, res, chk)));
    ev.truncate_warnings(0);
}

fn exec(op: &str, p: &[Value]) -> Value {
    match op {
        "cast" => {
            let (ctx, ev) = ctx_of(&p[3]);
            let a0 = p[0].as_array().unwrap();
            let (arg, chk) = if a0[0].as_str() == Some("cast") {
                // ["cast", tpIn, colArg]: the argument is CAST(column AS tp).
                let (inner, chk) = arg_of(&a0[2], p.get(4));
                let e = build_cast_function_with_check(&*ctx, inner, ft_in(&a0[1]), false, false)
                    .unwrap();
                ev.truncate_warnings(0);
                (e, chk)
            } else {
                arg_of(&p[0], p.get(4))
            };
            let tp = ft_in(&p[1]);
            let mut res: Option<ExprRef> = None;
            let build = ex_try(|| {
                let explicit = p.get(5).is_some_and(|v| v.as_bool().unwrap());
                match build_cast_function_with_check(
                    &*ctx,
                    arg,
                    tp,
                    p[2].as_bool().unwrap(),
                    explicit,
                ) {
                    Err(e) => ex_err(&e),
                    Ok(r) => {
                        let d = dump(&r);
                        res = Some(r);
                        d
                    }
                }
            });
            let mut m = serde_json::Map::new();
            m.insert("build".into(), build);
            m.insert("bwarns".into(), warns(&ev.truncate_warnings(0)));
            let Some(res) = res else {
                return Value::Object(m);
            };
            let meta = match &*res {
                Expr::ScalarFunction(sf) => {
                    sf.function.in_union().map_or(Value::Null, |b| json!(b))
                }
                _ => Value::Null,
            };
            m.insert("meta".into(), meta);
            if let (Some(chk), Expr::ScalarFunction(_)) = (&chk, &*res) {
                eval_rows(&mut m, &ev, &res, chk);
            }
            Value::Object(m)
        }
        "binlit" => {
            let (ctx, ev) = ctx_of(&p[5]);
            let (arg, chk) = arg_of(&p[0], p.get(6));
            let mut res: Option<ExprRef> = None;
            let build = ex_try(|| {
                let ec = ExprCollation {
                    coer: CoercibilityExplicit,
                    repe: Default::default(),
                    charset: p[1].as_str().unwrap().to_owned(),
                    collation: p[2].as_str().unwrap().to_owned(),
                };
                let r = handle_binary_literal(
                    &*ctx,
                    arg,
                    &ec,
                    p[3].as_str().unwrap(),
                    p[4].as_bool().unwrap(),
                );
                let d = dump(&r);
                res = Some(r);
                d
            });
            let mut m = serde_json::Map::new();
            m.insert("build".into(), build);
            m.insert("bwarns".into(), warns(&ev.truncate_warnings(0)));
            if let (Some(res), Some(chk)) = (&res, &chk)
                && let Expr::ScalarFunction(_) = &**res
            {
                eval_rows(&mut m, &ev, res, chk);
            }
            Value::Object(m)
        }
        "collcast" => {
            let (ctx, ev) = ctx_of(&p[4]);
            let (arg, _) = arg_of(&p[0], None);
            let res = ex_try(|| {
                let ec = ExprCollation {
                    coer: CoercibilityExplicit,
                    repe: Default::default(),
                    charset: p[1].as_str().unwrap().to_owned(),
                    collation: p[2].as_str().unwrap().to_owned(),
                };
                match build_cast_collation_function(&*ctx, arg, &ec, p[3].as_bool().unwrap()) {
                    Ok(r) => dump(&r),
                    Err(e) => ex_err(&e),
                }
            });
            json!({ "res": res, "bwarns": warns(&ev.truncate_warnings(0)) })
        }
        "wrap" => {
            let (ctx, ev) = ctx_of(&p[2]);
            let (arg, _) = arg_of(&p[1], None);
            let res = ex_try(|| {
                let r = match p[0].as_str().unwrap() {
                    "int" => wrap_with_cast_as_int(&*ctx, arg),
                    "real" => wrap_with_cast_as_real(&*ctx, arg),
                    "string" => wrap_with_cast_as_string(&*ctx, arg),
                    "time" => wrap_with_cast_as_time(&*ctx, arg, ft_in(&p[3])),
                    "duration" => wrap_with_cast_as_duration(&*ctx, arg),
                    _ => wrap_with_cast_as_decimal(&*ctx, arg),
                };
                match r {
                    Ok(r) => dump(&r),
                    Err(e) => ex_err(&e),
                }
            });
            json!({ "res": res, "bwarns": warns(&ev.truncate_warnings(0)) })
        }
        _ => panic!("cast: op {op}"),
    }
}

fn replay(path: &str) {
    let mut gz = flate2::read::GzDecoder::new(std::fs::File::open(path).expect("vectors"));
    let mut text = String::new();
    gz.read_to_string(&mut text).unwrap();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "cast");
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let mut fails = vec![];
    let mut n = 0;
    for line in lines {
        n += 1;
        let rec: Value = serde_json::from_str(line).unwrap();
        let args = rec["args"].as_array().cloned().unwrap_or_default();
        let got = exec(rec["op"].as_str().unwrap(), &args);
        let got: Value = serde_json::from_str(&got.to_string()).unwrap();
        if got != rec["out"] {
            fails.push(format!("{line}\n    got {got}"));
        }
    }
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
        "{path}: {} of {n} records differ:\n{}",
        fails.len(),
        shown.join("\n")
    );
    assert_eq!(n as u64, header["records"].as_u64().unwrap());
}

#[test]
fn govec_cast() {
    replay(&vectors());
    if let Ok(files) = std::env::var("GOVEC_CAST_FILES") {
        for f in files.split(',').filter(|f| !f.is_empty()) {
            replay(f);
        }
    }
}
