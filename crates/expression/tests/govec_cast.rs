//! Replays the govec `cast` vectors (tidb-rs-tracker `govec/domains/cast.go`): block D08.8 —
//! CAST to INT/REAL/DECIMAL/STRING/DATETIME/DATE/TIMESTAMP/TIME built with `BuildCastFunctionWithCheck` (folded on a
//! constant argument) and evaluated row by row and over a chunk (the vector driver),
//! CAST to JSON, JSON ARRAY and VECTOR, `WrapWithCastAsInt/Real/Decimal/String/Time/Duration/JSON/VectorFloat32`,
//! `ConvertJSON2Tp`, `HandleBinaryLiteral` (to_binary/from_binary)
//! and `BuildCastCollationFunction` — against Go TiDB v8.5.8.
//!
//! `GOVEC_CAST_FILES` (comma-separated paths) replays extra-seed artifacts as well.

mod cast_arith;
mod cast_cmp;
mod cast_cprop;
mod cast_eval;
mod cast_math;

use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use serde_json::{Value, json};
use tidb_base::Error;
use tidb_base::contextutil::{SQLWarn, WarnHandler, new_static_warn_handler};
use tidb_base::mysql::SQLMode;
use tidb_chunk::chunk::new_chunk_with_capacity;
use tidb_expression::builtin_cast::{
    build_cast_collation_function, build_cast_function_with_check, convert_json2_tp,
    wrap_with_cast_as_decimal, wrap_with_cast_as_duration, wrap_with_cast_as_int,
    wrap_with_cast_as_json, wrap_with_cast_as_real, wrap_with_cast_as_string,
    wrap_with_cast_as_time, wrap_with_cast_as_vector_float32,
};
use tidb_expression::builtin_convert_charset::handle_binary_literal;
use tidb_expression::eval::{RpnProgram, RpnState, rt};
use tidb_expression::*;
use tidb_parser::types::EvalType;
use tidb_parser::types::{
    ETDatetime, ETDecimal, ETDuration, ETInt, ETJson, ETReal, ETString, ETTimestamp,
    ETVectorFloat32, FieldType,
};
use tidb_sessionctx::errctx::{Level, LevelMap};
use tidb_sessionctx::exprstatic as es;
use tidb_types::MyDecimal;
use tidb_types::binary_literal::BinaryLiteral;
use tidb_types::context::Flags;
use tidb_types::datum::{self as dm, Datum};
use tidb_types::enum_type::Enum;
use tidb_types::field_type_builder::new_field_type_builder;
use tidb_types::json::{BinaryJSON, create_binary_json};
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
        // constant propagation (D08.2) builds constants of these kinds
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
                x.func_name.l, x.func_name.o, x.function.sig.name, ft_out(x.ret_type()), args,
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
    ctx_of_with(v, vec![], None)
}

/// castCtx with extra eval options and the connection charset (part 2b-3a).
fn ctx_of_with(v: &Value, extra: Vec<es::EvalCtxOption>, cs: Option<(String, String)>) -> Ctxs {
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
    if a.len() > 5 {
        // div_precision_increment (D08.5).
        opts.push(es::with_div_precision_increment(a[5].as_i64().unwrap()));
    }
    opts.extend(extra);
    let ev = es::new_eval_context(opts);
    let mut copts = vec![es::with_eval_ctx(Arc::clone(&ev))];
    if a.len() > 6 {
        // The seed of the MysqlRng RAND() reads (D08.5 batch 4).
        copts.push(es::with_rng(Arc::new(parking_lot::Mutex::new(
            tidb_sessionctx::variable::session_state::MysqlRng::new_with_seed(
                a[6].as_i64().unwrap(),
            ),
        ))));
    }
    if let Some((cs, coll)) = cs {
        copts.push(es::with_charset(cs, coll));
    }
    let ctx = es::new_expr_context(copts);
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

/// castJSONOut: a JSON value as [TypeCode, hex(Value)].
fn json_out(j: &BinaryJSON<'_>) -> Value {
    json!([j.type_code, hex(&j.value)])
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
    } else if et == ETJson {
        ex_val(e.eval_json(ctx, &st, row), |v| json_out(&v))
    } else if et == ETVectorFloat32 {
        ex_val(e.eval_vector_float32(ctx, &st, row), |v| {
            json!(hex(v.zero_copy_serialize()))
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
            } else if et == ETJson {
                json_out(&col.get_json(i))
            } else if et == ETVectorFloat32 {
                json!(hex(col.get_vector_float32(i).zero_copy_serialize()))
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
            let mut tp = ft_in(&p[1]);
            if p.get(6).is_some_and(|v| v.as_bool().unwrap()) {
                tp.set_array(true); // CAST(.. AS type ARRAY)
            }
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
        "json2tp" => {
            let d = datum_in(&p[0]);
            let j = d.get_mysql_json();
            let ft = ft_in(&p[1]);
            let res = ex_try(|| match convert_json2_tp(&j, &ft) {
                Ok(v) => json_out(&create_binary_json(v)),
                Err(e) => ex_err(&e),
            });
            json!({ "res": res })
        }
        "withtps" => {
            let cs = p[6].as_str().unwrap().to_owned();
            let coll = p[7].as_str().unwrap().to_owned();
            let (ctx, ev) = ctx_of_with(&p[5], vec![], Some((cs, coll)));
            let args: Vec<ExprRef> = p[2]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| arg_of(a, None).0)
                .collect();
            let name = p[1].as_str().unwrap();
            let ret = EvalType(p[3].as_i64().unwrap() as _);
            let bf = ex_try(|| {
                let r = if p[0].as_i64().unwrap() == 0 {
                    let ets: Vec<EvalType> = p[4]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|x| EvalType(x.as_i64().unwrap() as _))
                        .collect();
                    tidb_expression::builtin::new_base_builtin_func_with_tp(
                        &*ctx, name, args, ret, &ets,
                    )
                } else {
                    let fts: Vec<FieldType> = p[4].as_array().unwrap().iter().map(ft_in).collect();
                    let refs: Vec<&FieldType> = fts.iter().collect();
                    tidb_expression::builtin::new_base_builtin_func_with_field_types(
                        &*ctx, name, args, ret, &refs,
                    )
                };
                match r {
                    Err(e) => ex_err(&e),
                    Ok(bf) => {
                        let ci = &bf.collation_info;
                        let (cs, coll) = ci.charset_and_collation();
                        let key = hex(&bf.collator().key("aB é ß".as_bytes()));
                        json!({
                            "tp": ft_out(&bf.tp),
                            "cs": cs,
                            "coll": coll,
                            "coerInit": ci.has_coercibility(),
                            "coer": ci.coercibility().0,
                            "repe": ci.repertoire().0,
                            "explicit": ci.is_explicit_charset(),
                            "key": key,
                            "args": bf.args.iter().map(|a| dump(a)).collect::<Vec<_>>(),
                        })
                    }
                }
            });
            json!({ "bf": bf, "bwarns": warns(&ev.truncate_warnings(0)) })
        }
        "newfunc" => {
            let db = p[5].as_str().unwrap().to_owned();
            let (ctx, ev) = ctx_of_with(&p[4], vec![es::with_current_db(db)], None);
            let args: Vec<ExprRef> = p[3]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| arg_of(a, None).0)
                .collect();
            let name = p[1].as_str().unwrap();
            let tp = ft_in(&p[2]);
            let res = ex_try(|| {
                let r = match p[0].as_i64().unwrap() {
                    0 => tidb_expression::scalar_function::new_function(&*ctx, name, tp, args),
                    1 => tidb_expression::scalar_function::new_function_base(&*ctx, name, tp, args),
                    2 => tidb_expression::scalar_function::new_function_try_fold(
                        &*ctx, name, tp, args,
                    ),
                    3 => tidb_expression::scalar_function::new_function_with_init(
                        &*ctx, name, tp, None, args,
                    ),
                    _ => Ok(
                        match tidb_expression::scalar_function::new_function_internal(
                            &*ctx, name, tp, args,
                        ) {
                            Some(e) => e,
                            None => return Value::Null,
                        },
                    ),
                };
                match r {
                    Ok(e) => dump(&e),
                    Err(e) => ex_err(&e),
                }
            });
            json!({ "res": res, "warns": warns(&ev.truncate_warnings(0)) })
        }
        "nullinfer" => {
            let (_, ev) = ctx_of(&p[1]);
            let mut args: Vec<ExprRef> = p[0]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| arg_of(a, None).0)
                .collect();
            tidb_expression::scalar_function::type_infer_for_null(&*ev, &mut args);
            Value::Array(args.iter().map(|a| dump(a)).collect())
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
                    "json" => wrap_with_cast_as_json(&*ctx, arg),
                    "vector" => wrap_with_cast_as_vector_float32(&*ctx, arg),
                    _ => wrap_with_cast_as_decimal(&*ctx, arg),
                };
                match r {
                    Ok(r) => dump(&r),
                    Err(e) => ex_err(&e),
                }
            });
            json!({ "res": res, "bwarns": warns(&ev.truncate_warnings(0)) })
        }
        _ => cast_eval::exec(op, p)
            .or_else(|| cast_cmp::exec(op, p))
            .or_else(|| cast_arith::exec(op, p))
            .or_else(|| cast_cprop::exec(op, p))
            .or_else(|| cast_math::exec(op, p))
            .unwrap_or_else(|| panic!("cast: op {op}")),
    }
}

/// A "cprop" result with the flen, decimal and flag of every constant's field type erased
/// (see the in-place type changes in `replay`).
fn cprop_erase_types(mut v: Value) -> Value {
    fn walk(v: &mut Value) {
        match v {
            Value::Object(m) => {
                if let Some(Value::Array(c)) = m.get_mut("const")
                    && let Some(Value::Array(ft)) = c.get_mut(1)
                {
                    ft[1] = Value::Null;
                    ft[2] = Value::Null;
                    ft[3] = Value::Null;
                }
                for (_, x) in m.iter_mut() {
                    walk(x);
                }
            }
            Value::Array(a) => a.iter_mut().for_each(walk),
            _ => {}
        }
    }
    walk(&mut v);
    v
}

fn replay(path: &str) {
    let mut gz = flate2::read::GzDecoder::new(std::fs::File::open(path).expect("vectors"));
    let mut text = String::new();
    gz.read_to_string(&mut text).unwrap();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "cast");
    let prev = std::panic::take_hook();
    if std::env::var("GOVEC_PANIC").is_err() {
        std::panic::set_hook(Box::new(|_| {}));
    }
    let mut fails = vec![];
    let mut n = 0;
    let mut cprop = 0;
    let mut in_place = 0;
    let mut in_place_type = 0;
    let lenient = std::env::var("GOVEC_CPROP_STRICT").is_err();
    for line in lines {
        n += 1;
        let mut rec: Value = serde_json::from_str(line).unwrap();
        let is_cprop = rec["op"] == "cprop";
        cprop += usize::from(is_cprop);
        // "cprop": the generator marks the records where Go changed a node of the input
        // conditions in place (every holder sees it); tidb-rs changes copies (D08.2 spec §4
        // item 7, a deliberate deviation, #104). Every record is executed; a marked record
        // that differs (and does not panic) counts as a deviation, capped below.
        let marked = rec["out"]
            .as_object_mut()
            .and_then(|m| m.remove("known_deviation"))
            .is_some_and(|d| d == "in-place");
        let args = rec["args"].as_array().cloned().unwrap_or_default();
        let got = exec(rec["op"].as_str().unwrap(), &args);
        let got: Value = serde_json::from_str(&got.to_string()).unwrap();
        if got == rec["out"] {
            continue;
        }
        if lenient && is_cprop && marked && !got.to_string().contains(r#""panic":true"#) {
            in_place += 1;
            continue;
        }
        if lenient
            && is_cprop
            && cprop_erase_types(got.clone()) == cprop_erase_types(rec["out"].clone())
        {
            // Go changed the flen / decimal / flags of a constant created during propagation
            // in place (PropagateType on a DECIMAL constant); tidb-rs changes copies (D08.2
            // spec §4). Values, signatures and function types are equal.
            in_place_type += 1;
            continue;
        }
        fails.push(format!("{line}\n    got {got}"));
    }
    std::panic::set_hook(prev);
    // Caps relative to the cprop records, so that runs with other seeds or more records
    // work unchanged: ≤ 2 % in-place deviations, ≤ 0.1 % constant-type-only differences.
    assert!(
        in_place * 50 <= cprop,
        "{path}: {in_place} of {cprop} cprop records differ by Go's in-place changes (cap 2 %)"
    );
    assert!(
        in_place_type * 1000 <= cprop.max(1000),
        "{path}: {in_place_type} of {cprop} cprop records differ only in constant field types (cap 0.1 %)"
    );
    if lenient && cprop > 0 {
        eprintln!(
            "{path}: cprop {cprop}, in-place deviations {in_place}, constant-type-only {in_place_type}"
        );
    }
    if let Ok(f) = std::env::var("GOVEC_FAILS_FILE") {
        // every differing record in full (debugging aid)
        std::fs::write(f, fails.join("\n")).unwrap();
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
fn govec_cast() {
    if std::env::var("GOVEC_CAST_SKIP_MAIN").is_err() {
        replay(&vectors());
    }
    if let Ok(files) = std::env::var("GOVEC_CAST_FILES") {
        for f in files.split(',').filter(|f| !f.is_empty()) {
            replay(f);
        }
    }
}
